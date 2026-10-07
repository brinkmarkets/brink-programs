//! Brink yield splitting.
//!
//! A `Market` splits an interest-bearing unit (the SY: a Brink pool share or a venue receipt) into a principal
//! token and a yield token that share one maturity. Both are denominated in the asset the unit accrues in, USDC,
//! so one PT is a claim on one USDC of principal at maturity and one YT is a claim on the yield one USDC of SY
//! earns until then.
//!
//! ```text
//! mint_py(sy)        SY in; PT and YT out, each worth the SY's asset value at the market's origin rate
//! redeem_py(py)      equal PT and YT in; SY out at the origin rate (any time before settlement)
//! settle()           at or after maturity, anyone: the maturity rate is frozen and the SY in the vault is
//!                    divided between principal (PT) and yield (YT)
//! redeem_pt / redeem_yt   after settlement: each token redeems its share of its side
//! ```
//!
//! * Every pair is backed by the same quantity of SY, `1 / rate_origin` per asset unit, whenever it was minted:
//!   the conversion between SY and PT plus YT is fixed at the rate the unit had when the market was created.
//!   A pair therefore always redeems exactly the SY that backs it, a YT's claim at maturity is the yield its
//!   backing earned over the whole term, and a late minter contributes the yield already embedded in the SY
//!   they lock in return for a YT that claims it back: no holder can take another's yield or backing by
//!   choosing when to mint or redeem (external scan 2, findings 2 and 3).
//! * The origin rate is read from the unit's own program account when the market is created, and the maturity
//!   rate is read when the market settles: a pool's value per share, or a venue's index. A venue market matures
//!   at a UTC midnight and settles at the venue's index for that midnight, reconstructed from the benchmark's
//!   fixings whenever settlement is called (external scan 2, findings 5 and 20); a pool market settles at the
//!   first value read at or after maturity, since a pool's value per share has no on-chain history. Nothing is
//!   quoted or estimated. The implied fixed rate a PT trades at is a market observation, published alongside
//!   the swap curve.
//! * Yield settles at maturity. A YT holder's claim is the yield their backing earned over the term; a PT
//!   holder's claim is their principal in SY at the maturity rate. If the unit lost value over the term, YT
//!   redeems to nothing and PT redeems pro rata to what is there.
//! * A mint fee of 5 basis points is kept in SY in the market's fee account and swept, half and half, to the
//!   protocol treasury and the buyback escrow owners named on the AMM.
//! * Markets created by the previous build (version 1) priced pairs at the live rate. They no longer mint or
//!   redeem pairs; they settle and redeem their sides as before so every holder can exit.
#![allow(clippy::module_name_repetitions)]
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::arithmetic_side_effects,
        clippy::indexing_slicing
    )
)]

use anchor_lang::prelude::*;
use anchor_lang::solana_program::bpf_loader_upgradeable;
use anchor_spl::token_interface::{
    self, Burn, Mint, MintTo, TokenAccount, TokenInterface, TransferChecked,
};
use brink_index::Benchmark;
use brink_venue::Venue;
use swap_amm::state::{Global as AmmGlobal, Pool as AmmPool};

declare_id!("ASYzAAxpLwQW5XwdL1GJbpQ15onLR6uM1HBSCRTza9sz");

pub const RATE_SCALE: u128 = 1_000_000_000_000_000_000;
/// Mint fee in basis points of the SY deposited.
pub const MINT_FEE_BP: u128 = 5;
/// PT and YT decimals: the asset's.
pub const PY_DECIMALS: u8 = 6;
/// A market must run at least a day and at most a year.
pub const MIN_TERM: i64 = 86_400;
pub const MAX_TERM: i64 = 365 * 86_400;
/// Layout and pricing version written by this build. Version 1 markets refuse `mint_py` and `redeem_py`.
pub const MARKET_VERSION: u8 = 2;

/// Where the SY's rate comes from.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq, Eq, InitSpace, Debug)]
pub enum SourceKind {
    /// A swap AMM pool: the SY is its share mint, the rate its value per share.
    BrinkPool,
    /// A venue adapter: the SY is its receipt mint, the rate its index.
    BrinkVenue,
}

#[program]
pub mod brink_split {
    use super::*;

    /// Creates a market over a unit with a maturity. Upgrade authority only.
    pub fn create_market(ctx: Context<CreateMarket>, args: CreateMarketArgs) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let term = args
            .maturity_ts
            .checked_sub(now)
            .ok_or(SplitError::Overflow)?;
        require!((MIN_TERM..=MAX_TERM).contains(&term), SplitError::Term);
        // The source must be the unit's own program account and the SY mint must be the one it names; the
        // origin rate every pair is priced at is read here and never again.
        let source = &ctx.accounts.source;
        let origin = match args.source_kind {
            SourceKind::BrinkPool => {
                require!(*source.owner == swap_amm::ID, SplitError::Source);
                let data = source.try_borrow_data()?;
                let mut slice: &[u8] = &data;
                let pool = AmmPool::try_deserialize(&mut slice).map_err(|_| SplitError::Source)?;
                require!(
                    pool.share_mint == ctx.accounts.sy_mint.key(),
                    SplitError::Source
                );
                pool_rate(pool.tvl, pool.share_supply)?
            }
            SourceKind::BrinkVenue => {
                require!(*source.owner == brink_venue::ID, SplitError::Source);
                // A venue market matures at a UTC midnight so that its maturity rate is the venue's index for
                // that midnight, a pure function of the benchmark's fixings.
                require!(
                    args.maturity_ts.rem_euclid(brink_index::SECONDS_PER_DAY) == 0,
                    SplitError::Midnight
                );
                let data = source.try_borrow_data()?;
                let mut slice: &[u8] = &data;
                let venue = Venue::try_deserialize(&mut slice).map_err(|_| SplitError::Source)?;
                require!(
                    venue.receipt_mint == ctx.accounts.sy_mint.key(),
                    SplitError::Source
                );
                venue_rate(&venue, ctx.accounts.benchmark.as_deref(), now)?
            }
        };
        let m = &mut ctx.accounts.market;
        m.version = MARKET_VERSION;
        m.authority = ctx.accounts.authority.key();
        m.source_kind = args.source_kind;
        m.source = source.key();
        m.sy_mint = ctx.accounts.sy_mint.key();
        m.sy_vault = ctx.accounts.sy_vault.key();
        m.fee_account = ctx.accounts.fee_account.key();
        m.pt_mint = ctx.accounts.pt_mint.key();
        m.yt_mint = ctx.accounts.yt_mint.key();
        m.maturity_ts = args.maturity_ts;
        m.created_ts = now;
        m.min_mint = args.min_mint;
        m.rate_at_maturity_e18 = 0;
        m.settled = false;
        m.sy_locked = 0;
        m.py_supply = 0;
        m.sy_for_pt = 0;
        m.sy_for_yt = 0;
        m.pt_at_settle = 0;
        m.yt_at_settle = 0;
        m.fees = 0;
        m.bump = ctx.bumps.market;
        m.rate_origin_e18 = origin;
        m.reserved = [0; 48];
        emit_cpi!(MarketCreated {
            market: m.key(),
            source: m.source,
            sy_mint: m.sy_mint,
            pt_mint: m.pt_mint,
            yt_mint: m.yt_mint,
            maturity_ts: m.maturity_ts,
            rate_origin_e18: origin,
        });
        Ok(())
    }

    /// Deposits SY and mints equal PT and YT worth its asset value at the origin rate, less the mint fee.
    pub fn mint_py(ctx: Context<MintPy>, sy_amount: u64, min_py: u64) -> Result<()> {
        require!(sy_amount > 0, SplitError::Zero);
        let now = Clock::get()?.unix_timestamp;
        require!(!ctx.accounts.market.settled, SplitError::Settled);
        require!(now < ctx.accounts.market.maturity_ts, SplitError::Matured);
        let rate = origin_rate(&ctx.accounts.market)?;
        let fee = u64::try_from(
            u128::from(sy_amount)
                .checked_mul(MINT_FEE_BP)
                .ok_or(SplitError::Overflow)?
                .div_ceil(10_000),
        )
        .map_err(|_| SplitError::Overflow)?;
        let net = sy_amount.checked_sub(fee).ok_or(SplitError::Overflow)?;
        let py = asset_for(net, rate)?;
        require!(
            py >= ctx.accounts.market.min_mint && py > 0,
            SplitError::BelowMinimum
        );
        require!(py >= min_py, SplitError::BelowMinimum);
        // SY in: the net to the vault, the fee to the fee account.
        token_interface::transfer_checked(
            CpiContext::new(
                ctx.accounts.token_program.key(),
                TransferChecked {
                    from: ctx.accounts.owner_sy.to_account_info(),
                    mint: ctx.accounts.sy_mint.to_account_info(),
                    to: ctx.accounts.sy_vault.to_account_info(),
                    authority: ctx.accounts.owner.to_account_info(),
                },
            ),
            net,
            ctx.accounts.sy_mint.decimals,
        )?;
        if fee > 0 {
            token_interface::transfer_checked(
                CpiContext::new(
                    ctx.accounts.token_program.key(),
                    TransferChecked {
                        from: ctx.accounts.owner_sy.to_account_info(),
                        mint: ctx.accounts.sy_mint.to_account_info(),
                        to: ctx.accounts.fee_account.to_account_info(),
                        authority: ctx.accounts.owner.to_account_info(),
                    },
                ),
                fee,
                ctx.accounts.sy_mint.decimals,
            )?;
        }
        let m = &ctx.accounts.market;
        let seeds: &[&[u8]] = &[
            Market::SEED,
            m.sy_mint.as_ref(),
            &m.maturity_ts.to_le_bytes(),
            &[m.bump],
        ];
        for (mint, to) in [
            (&ctx.accounts.pt_mint, &ctx.accounts.owner_pt),
            (&ctx.accounts.yt_mint, &ctx.accounts.owner_yt),
        ] {
            token_interface::mint_to(
                CpiContext::new_with_signer(
                    ctx.accounts.token_program.key(),
                    MintTo {
                        mint: mint.to_account_info(),
                        to: to.to_account_info(),
                        authority: m.to_account_info(),
                    },
                    &[seeds],
                ),
                py,
            )?;
        }
        let m = &mut ctx.accounts.market;
        m.sy_locked = m.sy_locked.checked_add(net).ok_or(SplitError::Overflow)?;
        m.py_supply = m.py_supply.checked_add(py).ok_or(SplitError::Overflow)?;
        m.fees = m.fees.checked_add(fee).ok_or(SplitError::Overflow)?;
        emit_cpi!(Minted {
            market: m.key(),
            owner: ctx.accounts.owner.key(),
            sy_in: sy_amount,
            fee,
            py,
            rate_e18: rate,
        });
        Ok(())
    }

    /// Burns equal PT and YT and returns the SY that backs them, at the origin rate. Allowed until the market
    /// settles.
    pub fn redeem_py(ctx: Context<RedeemPy>, py_amount: u64, min_sy: u64) -> Result<()> {
        require!(py_amount > 0, SplitError::Zero);
        require!(!ctx.accounts.market.settled, SplitError::Settled);
        let rate = origin_rate(&ctx.accounts.market)?;
        let sy = sy_for(py_amount, rate)?.min(ctx.accounts.market.sy_locked);
        require!(sy >= min_sy && sy > 0, SplitError::BelowMinimum);
        burn_py(
            &ctx.accounts.token_program,
            &ctx.accounts.pt_mint,
            &ctx.accounts.owner_pt,
            &ctx.accounts.owner,
            py_amount,
        )?;
        burn_py(
            &ctx.accounts.token_program,
            &ctx.accounts.yt_mint,
            &ctx.accounts.owner_yt,
            &ctx.accounts.owner,
            py_amount,
        )?;
        pay_sy(
            &ctx.accounts.market,
            &ctx.accounts.sy_vault,
            &ctx.accounts.owner_sy,
            &ctx.accounts.sy_mint,
            &ctx.accounts.token_program,
            sy,
        )?;
        let m = &mut ctx.accounts.market;
        m.sy_locked = m.sy_locked.checked_sub(sy).ok_or(SplitError::Overflow)?;
        m.py_supply = m
            .py_supply
            .checked_sub(py_amount)
            .ok_or(SplitError::Overflow)?;
        emit_cpi!(RedeemedPair {
            market: m.key(),
            owner: ctx.accounts.owner.key(),
            py: py_amount,
            sy_out: sy,
            rate_e18: rate,
        });
        Ok(())
    }

    /// At or after maturity, anyone freezes the maturity rate and divides the vault between principal and
    /// yield. A venue market's rate is the venue's index at the maturity instant, whenever this is called.
    pub fn settle(ctx: Context<Settle>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        require!(!ctx.accounts.market.settled, SplitError::Settled);
        require!(
            now >= ctx.accounts.market.maturity_ts,
            SplitError::NotMatured
        );
        let rate = maturity_rate(
            &ctx.accounts.market,
            &ctx.accounts.source,
            ctx.accounts.benchmark.as_deref(),
        )?;
        let total = ctx.accounts.sy_vault.amount;
        let pt = ctx.accounts.pt_mint.supply;
        let yt = ctx.accounts.yt_mint.supply;
        // Principal first: PT redeems its asset value in SY at the maturity rate, capped at what is there.
        let for_pt = sy_for(pt, rate)?.min(total);
        let for_yt = total.checked_sub(for_pt).ok_or(SplitError::Overflow)?;
        let m = &mut ctx.accounts.market;
        m.settled = true;
        m.rate_at_maturity_e18 = rate;
        m.sy_for_pt = for_pt;
        m.sy_for_yt = for_yt;
        m.pt_at_settle = pt;
        m.yt_at_settle = yt;
        emit_cpi!(MarketSettled {
            market: m.key(),
            rate_e18: rate,
            sy_total: total,
            sy_for_pt: for_pt,
            sy_for_yt: for_yt,
            pt_supply: pt,
            yt_supply: yt,
        });
        Ok(())
    }

    /// After settlement, PT redeems its share of the principal side.
    pub fn redeem_pt(ctx: Context<RedeemSide>, amount: u64) -> Result<()> {
        require!(amount > 0, SplitError::Zero);
        let m = &ctx.accounts.market;
        require!(m.settled, SplitError::NotSettled);
        require!(ctx.accounts.side_mint.key() == m.pt_mint, SplitError::Mint);
        let sy = share_of(amount, m.sy_for_pt, m.pt_at_settle)?;
        burn_py(
            &ctx.accounts.token_program,
            &ctx.accounts.side_mint,
            &ctx.accounts.owner_side,
            &ctx.accounts.owner,
            amount,
        )?;
        if sy > 0 {
            pay_sy(
                &ctx.accounts.market,
                &ctx.accounts.sy_vault,
                &ctx.accounts.owner_sy,
                &ctx.accounts.sy_mint,
                &ctx.accounts.token_program,
                sy,
            )?;
        }
        let m = &mut ctx.accounts.market;
        m.sy_locked = m.sy_locked.saturating_sub(sy);
        emit_cpi!(RedeemedSide {
            market: m.key(),
            owner: ctx.accounts.owner.key(),
            principal: true,
            amount,
            sy_out: sy,
        });
        Ok(())
    }

    /// After settlement, YT redeems its share of the yield side.
    pub fn redeem_yt(ctx: Context<RedeemSide>, amount: u64) -> Result<()> {
        require!(amount > 0, SplitError::Zero);
        let m = &ctx.accounts.market;
        require!(m.settled, SplitError::NotSettled);
        require!(ctx.accounts.side_mint.key() == m.yt_mint, SplitError::Mint);
        let sy = share_of(amount, m.sy_for_yt, m.yt_at_settle)?;
        burn_py(
            &ctx.accounts.token_program,
            &ctx.accounts.side_mint,
            &ctx.accounts.owner_side,
            &ctx.accounts.owner,
            amount,
        )?;
        if sy > 0 {
            pay_sy(
                &ctx.accounts.market,
                &ctx.accounts.sy_vault,
                &ctx.accounts.owner_sy,
                &ctx.accounts.sy_mint,
                &ctx.accounts.token_program,
                sy,
            )?;
        }
        let m = &mut ctx.accounts.market;
        m.sy_locked = m.sy_locked.saturating_sub(sy);
        emit_cpi!(RedeemedSide {
            market: m.key(),
            owner: ctx.accounts.owner.key(),
            principal: false,
            amount,
            sy_out: sy,
        });
        Ok(())
    }

    /// Anyone moves the accrued mint fees, half and half, to SY accounts owned by the owners of the AMM's
    /// treasury and buyback escrow.
    pub fn sweep_fees(ctx: Context<SweepFees>) -> Result<()> {
        let amount = ctx.accounts.fee_account.amount;
        require!(amount > 0, SplitError::Zero);
        let half = amount.checked_div(2).ok_or(SplitError::Overflow)?;
        let other = amount.checked_sub(half).ok_or(SplitError::Overflow)?;
        let m = &ctx.accounts.market;
        let seeds: &[&[u8]] = &[
            Market::SEED,
            m.sy_mint.as_ref(),
            &m.maturity_ts.to_le_bytes(),
            &[m.bump],
        ];
        for (to, amt) in [
            (&ctx.accounts.treasury_sy, half),
            (&ctx.accounts.buyback_sy, other),
        ] {
            if amt == 0 {
                continue;
            }
            token_interface::transfer_checked(
                CpiContext::new_with_signer(
                    ctx.accounts.token_program.key(),
                    TransferChecked {
                        from: ctx.accounts.fee_account.to_account_info(),
                        mint: ctx.accounts.sy_mint.to_account_info(),
                        to: to.to_account_info(),
                        authority: m.to_account_info(),
                    },
                    &[seeds],
                ),
                amt,
                ctx.accounts.sy_mint.decimals,
            )?;
        }
        emit_cpi!(FeesSwept {
            market: m.key(),
            amount
        });
        Ok(())
    }
}

/// The rate every pair of a market is priced at: the unit's rate when the market was created. A version 1
/// market, which priced pairs at the live rate, has none and refuses pairs in both directions.
pub fn origin_rate(m: &Market) -> Result<u128> {
    require!(
        m.version >= MARKET_VERSION && m.rate_origin_e18 > 0,
        SplitError::Version
    );
    Ok(m.rate_origin_e18)
}

/// A venue's index at `t` from its account and its benchmark, which must be supplied for a venue market.
fn venue_rate(venue: &Venue, benchmark: Option<&Account<Benchmark>>, t: i64) -> Result<u128> {
    let b = benchmark.ok_or(SplitError::Benchmark)?;
    require!(b.key() == venue.benchmark, SplitError::Benchmark);
    // Settlement is its own instruction, so the backward walk to the maturity midnight runs exactly for as
    // many days as the benchmark's fixings ring holds; only a market settled later than that against a venue
    // touched since takes the venue's flat step over the remainder.
    venue.index_within(b, t, brink_index::FIXING_DAYS)
}

/// Asset base units per SY base unit at maturity, scaled by 1e18, read from the unit's own account: a pool's
/// value per share as it stands, a venue's index reconstructed for the maturity instant.
pub fn maturity_rate(
    m: &Market,
    source: &UncheckedAccount,
    benchmark: Option<&Account<Benchmark>>,
) -> Result<u128> {
    require!(source.key() == m.source, SplitError::Source);
    match m.source_kind {
        SourceKind::BrinkPool => {
            require!(*source.owner == swap_amm::ID, SplitError::Source);
            let data = source.try_borrow_data()?;
            let mut slice: &[u8] = &data;
            let pool = AmmPool::try_deserialize(&mut slice).map_err(|_| SplitError::Source)?;
            pool_rate(pool.tvl, pool.share_supply)
        }
        SourceKind::BrinkVenue => {
            require!(*source.owner == brink_venue::ID, SplitError::Source);
            let data = source.try_borrow_data()?;
            let mut slice: &[u8] = &data;
            let venue = Venue::try_deserialize(&mut slice).map_err(|_| SplitError::Source)?;
            venue_rate(&venue, benchmark, m.maturity_ts)
        }
    }
}

/// The AMM's value per share base unit, with its virtual offsets, scaled by 1e18.
pub fn pool_rate(tvl: u64, supply: u64) -> Result<u128> {
    (u128::from(tvl).checked_add(swap_amm::instructions::math::VIRTUAL_TVL))
        .and_then(|t| t.checked_mul(RATE_SCALE))
        .and_then(|t| {
            t.checked_div(
                u128::from(supply).checked_add(swap_amm::instructions::math::VIRTUAL_SHARES)?,
            )
        })
        .ok_or_else(|| SplitError::Overflow.into())
}

pub fn asset_for(sy: u64, rate_e18: u128) -> Result<u64> {
    u64::try_from(
        u128::from(sy)
            .checked_mul(rate_e18)
            .ok_or(SplitError::Overflow)?
            .checked_div(RATE_SCALE)
            .ok_or(SplitError::Overflow)?,
    )
    .map_err(|_| SplitError::Overflow.into())
}

pub fn sy_for(asset: u64, rate_e18: u128) -> Result<u64> {
    require!(rate_e18 > 0, SplitError::Overflow);
    u64::try_from(
        u128::from(asset)
            .checked_mul(RATE_SCALE)
            .ok_or(SplitError::Overflow)?
            .checked_div(rate_e18)
            .ok_or(SplitError::Overflow)?,
    )
    .map_err(|_| SplitError::Overflow.into())
}

fn share_of(amount: u64, total: u64, supply: u64) -> Result<u64> {
    if supply == 0 {
        return Ok(0);
    }
    u64::try_from(
        u128::from(amount)
            .checked_mul(u128::from(total))
            .ok_or(SplitError::Overflow)?
            .checked_div(u128::from(supply))
            .ok_or(SplitError::Overflow)?,
    )
    .map_err(|_| SplitError::Overflow.into())
}

fn burn_py<'info>(
    token_program: &Interface<'info, TokenInterface>,
    mint: &InterfaceAccount<'info, Mint>,
    from: &InterfaceAccount<'info, TokenAccount>,
    owner: &Signer<'info>,
    amount: u64,
) -> Result<()> {
    token_interface::burn(
        CpiContext::new(
            token_program.key(),
            Burn {
                mint: mint.to_account_info(),
                from: from.to_account_info(),
                authority: owner.to_account_info(),
            },
        ),
        amount,
    )
}

fn pay_sy<'info>(
    market: &Account<'info, Market>,
    vault: &InterfaceAccount<'info, TokenAccount>,
    to: &InterfaceAccount<'info, TokenAccount>,
    mint: &InterfaceAccount<'info, Mint>,
    token_program: &Interface<'info, TokenInterface>,
    amount: u64,
) -> Result<()> {
    let seeds: &[&[u8]] = &[
        Market::SEED,
        market.sy_mint.as_ref(),
        &market.maturity_ts.to_le_bytes(),
        &[market.bump],
    ];
    token_interface::transfer_checked(
        CpiContext::new_with_signer(
            token_program.key(),
            TransferChecked {
                from: vault.to_account_info(),
                mint: mint.to_account_info(),
                to: to.to_account_info(),
                authority: market.to_account_info(),
            },
            &[seeds],
        ),
        amount,
        mint.decimals,
    )
}

// ---------------------------------------------------------------- state

#[account]
#[derive(InitSpace)]
pub struct Market {
    pub version: u8,
    pub authority: Pubkey,
    pub source_kind: SourceKind,
    pub source: Pubkey,
    pub sy_mint: Pubkey,
    pub sy_vault: Pubkey,
    pub fee_account: Pubkey,
    pub pt_mint: Pubkey,
    pub yt_mint: Pubkey,
    pub maturity_ts: i64,
    pub created_ts: i64,
    pub min_mint: u64,
    pub rate_at_maturity_e18: u128,
    pub settled: bool,
    pub sy_locked: u64,
    pub py_supply: u64,
    pub sy_for_pt: u64,
    pub sy_for_yt: u64,
    pub pt_at_settle: u64,
    pub yt_at_settle: u64,
    pub fees: u64,
    pub bump: u8,
    /// The unit's rate when the market was created, 1e18 scale: every pair is minted and redeemed at it. Zero
    /// on a version 1 market.
    pub rate_origin_e18: u128,
    pub reserved: [u8; 48],
}
impl Market {
    pub const SEED: &'static [u8] = b"market";
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy)]
pub struct CreateMarketArgs {
    pub source_kind: SourceKind,
    pub maturity_ts: i64,
    pub min_mint: u64,
}

// ---------------------------------------------------------------- accounts

#[event_cpi]
#[derive(Accounts)]
#[instruction(args: CreateMarketArgs)]
pub struct CreateMarket<'info> {
    #[account(mut)]
    pub authority: Signer<'info>,
    #[account(
        address = bpf_loader_upgradeable::get_program_data_address(&crate::ID) @ SplitError::NotUpgradeAuthority,
        constraint = program_data.upgrade_authority_address == Some(authority.key()) @ SplitError::NotUpgradeAuthority,
    )]
    pub program_data: Account<'info, ProgramData>,
    /// CHECK: the unit's program account (pool or venue); owner and contents checked against `source_kind`.
    pub source: UncheckedAccount<'info>,
    /// The venue's benchmark, required for a venue market (the origin rate is the venue's index now).
    pub benchmark: Option<Box<Account<'info, Benchmark>>>,
    pub sy_mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(init, payer = authority, space = 8 + Market::INIT_SPACE, seeds = [Market::SEED, sy_mint.key().as_ref(), &args.maturity_ts.to_le_bytes()], bump)]
    pub market: Box<Account<'info, Market>>,
    #[account(init, payer = authority, seeds = [b"sy", market.key().as_ref()], bump, token::mint = sy_mint, token::authority = market, token::token_program = token_program)]
    pub sy_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(init, payer = authority, seeds = [b"fees", market.key().as_ref()], bump, token::mint = sy_mint, token::authority = market, token::token_program = token_program)]
    pub fee_account: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(init, payer = authority, seeds = [b"pt", market.key().as_ref()], bump, mint::decimals = PY_DECIMALS, mint::authority = market, mint::token_program = token_program)]
    pub pt_mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(init, payer = authority, seeds = [b"yt", market.key().as_ref()], bump, mint::decimals = PY_DECIMALS, mint::authority = market, mint::token_program = token_program)]
    pub yt_mint: Box<InterfaceAccount<'info, Mint>>,
    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

#[event_cpi]
#[derive(Accounts)]
pub struct MintPy<'info> {
    #[account(mut, seeds = [Market::SEED, market.sy_mint.as_ref(), &market.maturity_ts.to_le_bytes()], bump = market.bump, has_one = sy_mint, has_one = sy_vault, has_one = fee_account, has_one = pt_mint, has_one = yt_mint)]
    pub market: Box<Account<'info, Market>>,
    pub owner: Signer<'info>,
    #[account(mut, constraint = owner_sy.owner == owner.key() @ SplitError::TokenOwner, constraint = owner_sy.mint == sy_mint.key() @ SplitError::Mint)]
    pub owner_sy: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, constraint = owner_pt.mint == pt_mint.key() @ SplitError::Mint)]
    pub owner_pt: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, constraint = owner_yt.mint == yt_mint.key() @ SplitError::Mint)]
    pub owner_yt: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut)]
    pub sy_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut)]
    pub fee_account: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut)]
    pub pt_mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(mut)]
    pub yt_mint: Box<InterfaceAccount<'info, Mint>>,
    pub sy_mint: Box<InterfaceAccount<'info, Mint>>,
    pub token_program: Interface<'info, TokenInterface>,
}

#[event_cpi]
#[derive(Accounts)]
pub struct RedeemPy<'info> {
    #[account(mut, seeds = [Market::SEED, market.sy_mint.as_ref(), &market.maturity_ts.to_le_bytes()], bump = market.bump, has_one = sy_mint, has_one = sy_vault, has_one = pt_mint, has_one = yt_mint)]
    pub market: Box<Account<'info, Market>>,
    pub owner: Signer<'info>,
    #[account(mut, constraint = owner_sy.mint == sy_mint.key() @ SplitError::Mint)]
    pub owner_sy: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, constraint = owner_pt.owner == owner.key() @ SplitError::TokenOwner, constraint = owner_pt.mint == pt_mint.key() @ SplitError::Mint)]
    pub owner_pt: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, constraint = owner_yt.owner == owner.key() @ SplitError::TokenOwner, constraint = owner_yt.mint == yt_mint.key() @ SplitError::Mint)]
    pub owner_yt: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut)]
    pub sy_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut)]
    pub pt_mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(mut)]
    pub yt_mint: Box<InterfaceAccount<'info, Mint>>,
    pub sy_mint: Box<InterfaceAccount<'info, Mint>>,
    pub token_program: Interface<'info, TokenInterface>,
}

#[event_cpi]
#[derive(Accounts)]
pub struct Settle<'info> {
    #[account(mut, seeds = [Market::SEED, market.sy_mint.as_ref(), &market.maturity_ts.to_le_bytes()], bump = market.bump, has_one = source, has_one = sy_vault, has_one = pt_mint, has_one = yt_mint)]
    pub market: Box<Account<'info, Market>>,
    /// CHECK: the unit's program account; bound by `has_one` and read for the maturity rate.
    pub source: UncheckedAccount<'info>,
    /// The venue's benchmark, required for a venue market: the maturity rate is reconstructed from its fixings.
    pub benchmark: Option<Box<Account<'info, Benchmark>>>,
    pub sy_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    pub pt_mint: Box<InterfaceAccount<'info, Mint>>,
    pub yt_mint: Box<InterfaceAccount<'info, Mint>>,
    pub cranker: Signer<'info>,
}

#[event_cpi]
#[derive(Accounts)]
pub struct RedeemSide<'info> {
    #[account(mut, seeds = [Market::SEED, market.sy_mint.as_ref(), &market.maturity_ts.to_le_bytes()], bump = market.bump, has_one = sy_mint, has_one = sy_vault)]
    pub market: Box<Account<'info, Market>>,
    pub owner: Signer<'info>,
    #[account(mut, constraint = owner_sy.mint == sy_mint.key() @ SplitError::Mint)]
    pub owner_sy: Box<InterfaceAccount<'info, TokenAccount>>,
    /// PT or YT, checked against the market by the instruction.
    #[account(mut)]
    pub side_mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(mut, constraint = owner_side.owner == owner.key() @ SplitError::TokenOwner, constraint = owner_side.mint == side_mint.key() @ SplitError::Mint)]
    pub owner_side: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut)]
    pub sy_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    pub sy_mint: Box<InterfaceAccount<'info, Mint>>,
    pub token_program: Interface<'info, TokenInterface>,
}

#[event_cpi]
#[derive(Accounts)]
pub struct SweepFees<'info> {
    #[account(seeds = [Market::SEED, market.sy_mint.as_ref(), &market.maturity_ts.to_le_bytes()], bump = market.bump, has_one = sy_mint, has_one = fee_account)]
    pub market: Box<Account<'info, Market>>,
    #[account(seeds = [b"global"], bump = amm_global.bump, seeds::program = swap_amm::ID, has_one = treasury, has_one = buyback_escrow)]
    pub amm_global: Box<Account<'info, AmmGlobal>>,
    /// The AMM's treasury USDC account; its owner receives the treasury half in SY.
    pub treasury: Box<InterfaceAccount<'info, TokenAccount>>,
    /// The AMM's buyback escrow USDC account; its owner receives the buyback half in SY.
    pub buyback_escrow: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, constraint = treasury_sy.owner == treasury.owner @ SplitError::FeeDestination, constraint = treasury_sy.mint == sy_mint.key() @ SplitError::Mint)]
    pub treasury_sy: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, constraint = buyback_sy.owner == buyback_escrow.owner @ SplitError::FeeDestination, constraint = buyback_sy.mint == sy_mint.key() @ SplitError::Mint)]
    pub buyback_sy: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut)]
    pub fee_account: Box<InterfaceAccount<'info, TokenAccount>>,
    pub sy_mint: Box<InterfaceAccount<'info, Mint>>,
    pub token_program: Interface<'info, TokenInterface>,
}

// ---------------------------------------------------------------- events

#[event]
pub struct MarketCreated {
    pub market: Pubkey,
    pub source: Pubkey,
    pub sy_mint: Pubkey,
    pub pt_mint: Pubkey,
    pub yt_mint: Pubkey,
    pub maturity_ts: i64,
    pub rate_origin_e18: u128,
}
#[event]
pub struct Minted {
    pub market: Pubkey,
    pub owner: Pubkey,
    pub sy_in: u64,
    pub fee: u64,
    pub py: u64,
    pub rate_e18: u128,
}
#[event]
pub struct RedeemedPair {
    pub market: Pubkey,
    pub owner: Pubkey,
    pub py: u64,
    pub sy_out: u64,
    pub rate_e18: u128,
}
#[event]
pub struct MarketSettled {
    pub market: Pubkey,
    pub rate_e18: u128,
    pub sy_total: u64,
    pub sy_for_pt: u64,
    pub sy_for_yt: u64,
    pub pt_supply: u64,
    pub yt_supply: u64,
}
#[event]
pub struct RedeemedSide {
    pub market: Pubkey,
    pub owner: Pubkey,
    pub principal: bool,
    pub amount: u64,
    pub sy_out: u64,
}
#[event]
pub struct FeesSwept {
    pub market: Pubkey,
    pub amount: u64,
}

// ---------------------------------------------------------------- errors

#[error_code]
pub enum SplitError {
    #[msg("Only the program's upgrade authority may create a market")]
    NotUpgradeAuthority,
    #[msg("The term must be between one day and one year")]
    Term,
    #[msg("The source account is not the unit's program account for this SY mint")]
    Source,
    #[msg("Amount must be positive")]
    Zero,
    #[msg("The market has settled; redeem PT and YT separately")]
    Settled,
    #[msg("The market has matured; settle it, then redeem")]
    Matured,
    #[msg("The market has not matured")]
    NotMatured,
    #[msg("The market has not settled")]
    NotSettled,
    #[msg("Output below the minimum")]
    BelowMinimum,
    #[msg("Arithmetic overflow")]
    Overflow,
    #[msg("Token account owner mismatch")]
    TokenOwner,
    #[msg("Token mint mismatch")]
    Mint,
    #[msg("Fee destinations must be owned by the owners of the AMM's treasury and buyback escrow")]
    FeeDestination,
    #[msg("A venue market must mature at 00:00 UTC")]
    Midnight,
    #[msg("A venue market needs the venue's benchmark account")]
    Benchmark,
    #[msg("This market was created by a previous build and no longer mints or redeems pairs")]
    Version,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn market(version: u8, origin: u128) -> Market {
        Market {
            version,
            authority: Pubkey::default(),
            source_kind: SourceKind::BrinkVenue,
            source: Pubkey::default(),
            sy_mint: Pubkey::default(),
            sy_vault: Pubkey::default(),
            fee_account: Pubkey::default(),
            pt_mint: Pubkey::default(),
            yt_mint: Pubkey::default(),
            maturity_ts: 0,
            created_ts: 0,
            min_mint: 0,
            rate_at_maturity_e18: 0,
            settled: false,
            sy_locked: 0,
            py_supply: 0,
            sy_for_pt: 0,
            sy_for_yt: 0,
            pt_at_settle: 0,
            yt_at_settle: 0,
            fees: 0,
            bump: 0,
            rate_origin_e18: origin,
            reserved: [0; 48],
        }
    }

    /// A version 1 market, or one without an origin rate, prices no pairs in either direction.
    #[test]
    fn version_one_markets_refuse_pairs() {
        assert!(origin_rate(&market(1, 0)).is_err());
        assert!(origin_rate(&market(1, RATE_SCALE)).is_err());
        assert!(origin_rate(&market(2, 0)).is_err());
        assert_eq!(origin_rate(&market(2, RATE_SCALE)).unwrap(), RATE_SCALE);
    }

    /// External scan 2, findings 2 and 3. Pairs minted at any time are backed by the same SY, so each holder's
    /// principal and yield sides at maturity sum to the SY they locked, whatever the index did between their
    /// mint and the origin, and a pair redeemed early returns no more than its backing.
    #[test]
    fn every_pair_settles_to_its_own_backing() {
        let origin: u128 = 1_020_000_000_000_000_000; // the venue index when the market was created
        let maturity: u128 = 1_050_000_000_000_000_000;
        // Early minter locks 1,000 SY at origin; late minter locks 700 SY once the index has risen: both are
        // priced at the origin rate.
        let (sy_early, sy_late) = (1_000_000_000u64, 700_000_000u64);
        let py_early = asset_for(sy_early, origin).unwrap();
        let py_late = asset_for(sy_late, origin).unwrap();
        let total_sy = sy_early + sy_late;
        let pt_supply = py_early + py_late;
        // Settlement: principal first at the maturity rate, the rest is yield.
        let for_pt = sy_for(pt_supply, maturity).unwrap().min(total_sy);
        let for_yt = total_sy - for_pt;
        assert!(for_yt > 0, "the index rose, so there is yield");
        for (py, locked) in [(py_early, sy_early), (py_late, sy_late)] {
            let out =
                share_of(py, for_pt, pt_supply).unwrap() + share_of(py, for_yt, pt_supply).unwrap();
            assert!(
                out <= locked && out + 2 >= locked,
                "pt plus yt return the backing: {out} vs {locked}"
            );
            // Early exit returns the backing too, never more.
            let back = sy_for(py, origin).unwrap();
            assert!(
                back <= locked && back + 1 >= locked,
                "pair exit {back} vs {locked}"
            );
        }
        // When the unit lost value, PT takes everything pro rata and YT nothing.
        let fell: u128 = 900_000_000_000_000_000;
        let for_pt = sy_for(pt_supply, fell).unwrap().min(total_sy);
        assert_eq!(for_pt, total_sy);
    }

    /// The pool rate carries the virtual offsets so that an empty pool has a defined, finite rate.
    #[test]
    fn pool_rate_is_defined_at_zero_supply() {
        let r = pool_rate(0, 0).unwrap();
        assert_eq!(
            r,
            swap_amm::instructions::math::VIRTUAL_TVL * RATE_SCALE
                / swap_amm::instructions::math::VIRTUAL_SHARES
        );
        assert!(pool_rate(u64::MAX, 1).is_ok());
    }
}
