//! Brink fixed vaults.
//!
//! A fixed vault turns a benchmark's floating rate into a fixed APY for a fixed term. Each vault is a `Series`
//! with one share mint and one maturity. Depositors subscribe USDC during an open window; when the window closes
//! anyone may `hedge`: the series opens a receive-fixed swap on the Brink pool for the notional the deposits
//! support and places the rest of the USDC at the benchmark's venue through the venue adapter. Floating in from
//! the venue, floating out on the swap, fixed kept. At maturity anyone may `settle`: the swap is settled, the
//! placement redeemed, the fee taken on the yield, and shares redeem pro rata for USDC.
//!
//! ```text
//! create_series ──deposit* / withdraw*──▶ [window closes] ──hedge──▶ Hedged ──[maturity]──settle──▶ Settled ──redeem*
//!                                                 │                                                   
//!                                          below the minimum, or                                      
//!                                          nobody hedged in the grace ──▶ Cancelled ──withdraw* (1:1)
//! ```
//!
//! * The fixed rate is whatever the pool quotes at the hedge, within the cranker's limit; it is written to the
//!   series and shown to every holder. The APY a depositor earns is that rate on the hedged notional, less the
//!   series fee on the yield. Collateral the swap requires is carved from the deposits and earns nothing, so the
//!   effective APY is `fixed × notional / deposits`, which the series exposes.
//! * Nothing leaves a series on an authority's say-so. Before the hedge, every depositor can withdraw 1:1. After
//!   it, funds sit in the swap and the venue until maturity. The authority can only create series and cancel one
//!   that has not hedged, which returns deposits 1:1.
//! * The series trades through its own system-owned PDA, so the swap and the placement are the series' and no
//!   one else's; the AMM's exits are unconditional, so a matured swap can always be settled by anyone.
//! * Fees on the yield go half to the protocol treasury and half to the buyback escrow named on the AMM's global
//!   account, the same 50/50 split as every platform fee.
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
use anchor_lang::system_program;
use anchor_spl::token_interface::{
    self, Burn, Mint, MintTo, TokenAccount, TokenInterface, TransferChecked,
};
use brink_index::Benchmark;
use brink_venue::program::BrinkVenue;
use brink_venue::Venue;
use swap_amm::program::SwapAmm;
use swap_amm::state::{Global as AmmGlobal, LegKind, Pool as AmmPool, Swap as AmmSwap};

declare_id!("HhDTKdT36W1vE1DBgKnXDp7gFtxpxhRxovmfanuhaQiy");

/// Shortest and longest subscription windows.
pub const MIN_WINDOW: i64 = 3_600;
pub const MAX_WINDOW: i64 = 7 * 86_400;
/// If nobody hedges within this long of the window closing, anyone may cancel and deposits return 1:1.
pub const HEDGE_GRACE: i64 = 3 * 86_400;
/// Highest fee on yield a series may charge: 20 percent.
pub const MAX_FEE_BP: u16 = 2_000;
/// Lamports the series' trader PDA holds for the swap account's rent and the transaction it signs for.
pub const TRADER_FLOAT: u64 = 10_000_000;
/// Share decimals: the same as USDC, so one share is one USDC at subscription.
pub const SHARE_DECIMALS: u8 = 6;

#[program]
pub mod brink_vaults {
    use super::*;

    /// Creates a series on a pool and its venue. Upgrade authority only.
    pub fn create_series(ctx: Context<CreateSeries>, args: CreateSeriesArgs) -> Result<()> {
        require!(
            (MIN_WINDOW..=MAX_WINDOW).contains(&args.subscribe_seconds),
            VaultError::Window
        );
        require!(args.fee_bp <= MAX_FEE_BP, VaultError::Fee);
        require!(usize::from(args.tenor) < 4, VaultError::Tenor);
        require!(
            args.cap > 0 && args.min_total > 0 && args.min_total <= args.cap,
            VaultError::Caps
        );
        let pool = &ctx.accounts.pool;
        require!(
            ctx.accounts.venue.benchmark == pool.benchmark,
            VaultError::VenueBenchmark
        );
        // The hedged notional is at most the cap; it must fit the pool's bounds or the hedge could never place.
        require!(args.cap <= pool.max_notional, VaultError::Caps);
        let now = Clock::get()?.unix_timestamp;
        let s = &mut ctx.accounts.series;
        s.version = 1;
        s.authority = ctx.accounts.authority.key();
        s.pool = pool.key();
        s.benchmark = pool.benchmark;
        s.venue = ctx.accounts.venue.key();
        s.usdc_mint = ctx.accounts.usdc_mint.key();
        s.share_mint = ctx.accounts.share_mint.key();
        s.trader = ctx.accounts.trader.key();
        s.usdc_account = ctx.accounts.usdc_account.key();
        s.receipts_account = ctx.accounts.receipts_account.key();
        s.series_id = args.series_id;
        s.tenor = args.tenor;
        s.status = SeriesStatus::Subscribing;
        s.created_ts = now;
        s.subscribe_until_ts = now
            .checked_add(args.subscribe_seconds)
            .ok_or(VaultError::Overflow)?;
        s.hedged_ts = 0;
        s.matures_ts = 0;
        s.deposits = 0;
        s.cap = args.cap;
        s.min_total = args.min_total;
        s.fee_bp = args.fee_bp;
        s.min_fixed_bp = args.min_fixed_bp;
        s.fixed_bp = 0;
        s.notional = 0;
        s.collateral = 0;
        s.placed = 0;
        s.receipts = 0;
        s.swap = Pubkey::default();
        s.client_seed = args.client_seed;
        s.settled_assets = 0;
        s.fee_paid = 0;
        s.bump = ctx.bumps.series;
        s.trader_bump = ctx.bumps.trader;
        s.reserved = [0; 62];
        // Float for the trader PDA: the swap account's rent, returned to it when the swap closes.
        system_program::transfer(
            CpiContext::new(
                ctx.accounts.system_program.key(),
                system_program::Transfer {
                    from: ctx.accounts.authority.to_account_info(),
                    to: ctx.accounts.trader.to_account_info(),
                },
            ),
            TRADER_FLOAT,
        )?;
        emit_cpi!(SeriesCreated {
            series: s.key(),
            pool: s.pool,
            venue: s.venue,
            series_id: s.series_id,
            tenor: s.tenor,
            subscribe_until_ts: s.subscribe_until_ts,
            cap: s.cap,
            min_total: s.min_total,
            fee_bp: s.fee_bp,
            min_fixed_bp: s.min_fixed_bp,
        });
        Ok(())
    }

    /// Subscribes USDC while the window is open; one share per USDC.
    pub fn deposit(ctx: Context<Deposit>, amount: u64) -> Result<()> {
        require!(amount > 0, VaultError::Zero);
        let now = Clock::get()?.unix_timestamp;
        let s = &mut ctx.accounts.series;
        require!(s.status == SeriesStatus::Subscribing, VaultError::Status);
        require!(now < s.subscribe_until_ts, VaultError::WindowClosed);
        let total = s.deposits.checked_add(amount).ok_or(VaultError::Overflow)?;
        require!(total <= s.cap, VaultError::Cap);
        token_interface::transfer_checked(
            CpiContext::new(
                ctx.accounts.token_program.key(),
                TransferChecked {
                    from: ctx.accounts.depositor_usdc.to_account_info(),
                    mint: ctx.accounts.usdc_mint.to_account_info(),
                    to: ctx.accounts.usdc_account.to_account_info(),
                    authority: ctx.accounts.depositor.to_account_info(),
                },
            ),
            amount,
            ctx.accounts.usdc_mint.decimals,
        )?;
        let seeds: &[&[u8]] = &[
            Series::SEED,
            s.pool.as_ref(),
            &s.series_id.to_le_bytes(),
            &[s.bump],
        ];
        token_interface::mint_to(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.key(),
                MintTo {
                    mint: ctx.accounts.share_mint.to_account_info(),
                    to: ctx.accounts.depositor_shares.to_account_info(),
                    authority: s.to_account_info(),
                },
                &[seeds],
            ),
            amount,
        )?;
        s.deposits = total;
        emit_cpi!(Subscribed {
            series: s.key(),
            depositor: ctx.accounts.depositor.key(),
            amount,
            deposits: s.deposits,
        });
        Ok(())
    }

    /// Returns USDC 1:1 for shares while the series is subscribing or after it was cancelled.
    pub fn withdraw(ctx: Context<Withdraw>, shares: u64) -> Result<()> {
        require!(shares > 0, VaultError::Zero);
        let s = &mut ctx.accounts.series;
        require!(
            matches!(
                s.status,
                SeriesStatus::Subscribing | SeriesStatus::Cancelled
            ),
            VaultError::Status
        );
        token_interface::burn(
            CpiContext::new(
                ctx.accounts.token_program.key(),
                Burn {
                    mint: ctx.accounts.share_mint.to_account_info(),
                    from: ctx.accounts.holder_shares.to_account_info(),
                    authority: ctx.accounts.holder.to_account_info(),
                },
            ),
            shares,
        )?;
        pay_from_series(
            &ctx.accounts.series,
            &ctx.accounts.trader,
            &ctx.accounts.usdc_account,
            &ctx.accounts.holder_usdc,
            &ctx.accounts.usdc_mint,
            &ctx.accounts.token_program,
            shares,
        )?;
        let s = &mut ctx.accounts.series;
        s.deposits = s.deposits.checked_sub(shares).ok_or(VaultError::Overflow)?;
        emit_cpi!(Withdrawn {
            series: s.key(),
            holder: ctx.accounts.holder.key(),
            shares,
            amount: shares
        });
        Ok(())
    }

    /// Once the window has closed, anyone places the hedge: a receive-fixed swap for the notional the deposits
    /// support, the rest placed at the venue. A series below its minimum cancels instead. The fixed rate the
    /// swap commits the series to is never below the series' own floor, whatever limit the caller supplies
    /// (external scan 2, finding 15): the floor is set when the series is created and published with it.
    pub fn hedge(ctx: Context<Hedge>, limit_rate_bp: u16) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let limit_rate_bp = {
            let s = &ctx.accounts.series;
            require!(s.status == SeriesStatus::Subscribing, VaultError::Status);
            require!(now >= s.subscribe_until_ts, VaultError::WindowOpen);
            limit_rate_bp.max(s.min_fixed_bp)
        };
        if ctx.accounts.series.deposits < ctx.accounts.series.min_total {
            let s = &mut ctx.accounts.series;
            s.status = SeriesStatus::Cancelled;
            emit_cpi!(SeriesCancelled {
                series: s.key(),
                deposits: s.deposits,
                reason: CancelReason::BelowMinimum
            });
            return Ok(());
        }
        let deposits = ctx.accounts.series.deposits;
        let tenor = usize::from(ctx.accounts.series.tenor);
        let coll_bp = u128::from(
            *ctx.accounts
                .pool
                .params
                .collateral_bp
                .get(tenor)
                .ok_or(VaultError::Tenor)?,
        );
        // notional + collateral(notional) <= deposits, with the AMM's ceiling rounding on the collateral.
        let mut notional = u64::try_from(
            u128::from(deposits)
                .checked_mul(10_000)
                .ok_or(VaultError::Overflow)?
                .checked_div(
                    10_000u128
                        .checked_add(coll_bp)
                        .ok_or(VaultError::Overflow)?,
                )
                .ok_or(VaultError::Overflow)?,
        )
        .map_err(|_| VaultError::Overflow)?;
        while collateral_for(notional, coll_bp)?
            .checked_add(notional)
            .ok_or(VaultError::Overflow)?
            > deposits
        {
            notional = notional.checked_sub(1).ok_or(VaultError::Overflow)?;
        }
        if notional < ctx.accounts.pool.min_notional {
            let s = &mut ctx.accounts.series;
            s.status = SeriesStatus::Cancelled;
            emit_cpi!(SeriesCancelled {
                series: s.key(),
                deposits: s.deposits,
                reason: CancelReason::BelowMinimum
            });
            return Ok(());
        }
        require!(notional <= ctx.accounts.pool.max_notional, VaultError::Cap);

        let (pool_key, trader_bump, client_seed, tenor_u8) = {
            let s = &ctx.accounts.series;
            (s.pool, s.trader_bump, s.client_seed, s.tenor)
        };
        let series_key = ctx.accounts.series.key();
        let trader_seeds: &[&[u8]] = &[Series::TRADER_SEED, series_key.as_ref(), &[trader_bump]];

        // 1. The swap: receive fixed on the pool, the series' trader PDA as the trader.
        swap_amm::cpi::trader_open_swap(
            CpiContext::new_with_signer(
                ctx.accounts.swap_program.key(),
                swap_amm::cpi::accounts::TraderOpenSwap {
                    global: ctx.accounts.amm_global.to_account_info(),
                    pool: ctx.accounts.pool.to_account_info(),
                    benchmark: ctx.accounts.benchmark.to_account_info(),
                    swap: ctx.accounts.swap.to_account_info(),
                    trader: ctx.accounts.trader.to_account_info(),
                    trader_usdc: ctx.accounts.usdc_account.to_account_info(),
                    vault: ctx.accounts.pool_vault.to_account_info(),
                    usdc_mint: ctx.accounts.usdc_mint.to_account_info(),
                    hook_program: None,
                    token_program: ctx.accounts.token_program.to_account_info(),
                    system_program: ctx.accounts.system_program.to_account_info(),
                    event_authority: ctx.accounts.amm_event_authority.to_account_info(),
                    program: ctx.accounts.swap_program.to_account_info(),
                },
                &[trader_seeds],
            ),
            swap_amm::instructions::OpenSwapArgs {
                leg: LegKind::ReceiveFixed,
                tenor: tenor_u8,
                notional,
                limit_rate_bp,
                client_seed,
            },
        )?;
        let (fixed_bp, matures_ts, collateral) = {
            let data = ctx.accounts.swap.try_borrow_data()?;
            let mut slice: &[u8] = &data;
            let sw = AmmSwap::try_deserialize(&mut slice).map_err(|_| VaultError::SwapRead)?;
            require!(
                sw.trader == ctx.accounts.trader.key() && sw.pool == pool_key,
                VaultError::SwapRead
            );
            (sw.fixed_bp, sw.matures_ts, sw.collateral)
        };
        require!(
            fixed_bp >= ctx.accounts.series.min_fixed_bp,
            VaultError::HedgeRateFloor
        );

        // 2. The placement: everything the swap did not take goes to the venue.
        ctx.accounts.usdc_account.reload()?;
        let placed = ctx.accounts.usdc_account.amount;
        require!(placed > 0, VaultError::Zero);
        brink_venue::cpi::place(
            CpiContext::new_with_signer(
                ctx.accounts.venue_program.key(),
                brink_venue::cpi::accounts::Place {
                    venue: ctx.accounts.venue.to_account_info(),
                    benchmark: ctx.accounts.benchmark.to_account_info(),
                    owner: ctx.accounts.trader.to_account_info(),
                    owner_usdc: ctx.accounts.usdc_account.to_account_info(),
                    owner_receipts: ctx.accounts.receipts_account.to_account_info(),
                    reserve: ctx.accounts.venue_reserve.to_account_info(),
                    receipt_mint: ctx.accounts.receipt_mint.to_account_info(),
                    usdc_mint: ctx.accounts.usdc_mint.to_account_info(),
                    token_program: ctx.accounts.token_program.to_account_info(),
                    event_authority: ctx.accounts.venue_event_authority.to_account_info(),
                    program: ctx.accounts.venue_program.to_account_info(),
                },
                &[trader_seeds],
            ),
            placed,
            0,
        )?;
        ctx.accounts.receipts_account.reload()?;
        let receipts = ctx.accounts.receipts_account.amount;

        let s = &mut ctx.accounts.series;
        s.status = SeriesStatus::Hedged;
        s.hedged_ts = now;
        s.matures_ts = matures_ts;
        s.fixed_bp = fixed_bp;
        s.notional = notional;
        s.collateral = collateral;
        s.placed = placed;
        s.receipts = receipts;
        s.swap = ctx.accounts.swap.key();
        emit_cpi!(Hedged {
            series: s.key(),
            swap: s.swap,
            fixed_bp,
            notional,
            collateral,
            placed,
            receipts,
            matures_ts,
        });
        Ok(())
    }

    /// Anyone cancels a series that nobody hedged within the grace after its window closed. The authority may
    /// cancel earlier while the series is still subscribing. Deposits return 1:1 through `withdraw`.
    pub fn cancel(ctx: Context<Cancel>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let s = &mut ctx.accounts.series;
        require!(s.status == SeriesStatus::Subscribing, VaultError::Status);
        let by_authority = ctx.accounts.signer.key() == s.authority;
        let grace_over = now
            >= s.subscribe_until_ts
                .checked_add(HEDGE_GRACE)
                .ok_or(VaultError::Overflow)?;
        require!(by_authority || grace_over, VaultError::NotYet);
        s.status = SeriesStatus::Cancelled;
        emit_cpi!(SeriesCancelled {
            series: s.key(),
            deposits: s.deposits,
            reason: if by_authority {
                CancelReason::Authority
            } else {
                CancelReason::Unhedged
            },
        });
        Ok(())
    }

    /// After maturity, anyone settles: the swap is settled if it is still open, the placement is redeemed, the
    /// fee on the yield is split 50/50 to the treasury and the buyback escrow, and shares become redeemable.
    pub fn settle(ctx: Context<Settle>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        {
            let s = &ctx.accounts.series;
            require!(s.status == SeriesStatus::Hedged, VaultError::Status);
            require!(now >= s.matures_ts, VaultError::NotMatured);
        }
        let series_key = ctx.accounts.series.key();
        let trader_bump = ctx.accounts.series.trader_bump;
        let trader_seeds: &[&[u8]] = &[Series::TRADER_SEED, series_key.as_ref(), &[trader_bump]];

        // 1. The swap, if the AMM's crank has not already closed it.
        if !ctx.accounts.swap.data_is_empty() {
            swap_amm::cpi::crank_settle_swap(CpiContext::new_with_signer(
                ctx.accounts.swap_program.key(),
                swap_amm::cpi::accounts::CloseSwap {
                    global: ctx.accounts.amm_global.to_account_info(),
                    pool: ctx.accounts.pool.to_account_info(),
                    benchmark: ctx.accounts.benchmark.to_account_info(),
                    swap: ctx.accounts.swap.to_account_info(),
                    trader: ctx.accounts.trader.to_account_info(),
                    signer: ctx.accounts.trader.to_account_info(),
                    trader_usdc: ctx.accounts.usdc_account.to_account_info(),
                    cranker_usdc: None,
                    vault: ctx.accounts.pool_vault.to_account_info(),
                    usdc_mint: ctx.accounts.usdc_mint.to_account_info(),
                    token_program: ctx.accounts.token_program.to_account_info(),
                    event_authority: ctx.accounts.amm_event_authority.to_account_info(),
                    program: ctx.accounts.swap_program.to_account_info(),
                },
                &[trader_seeds],
            ))?;
        }

        // 2. The placement.
        ctx.accounts.receipts_account.reload()?;
        let receipts = ctx.accounts.receipts_account.amount;
        if receipts > 0 {
            brink_venue::cpi::redeem(
                CpiContext::new_with_signer(
                    ctx.accounts.venue_program.key(),
                    brink_venue::cpi::accounts::Redeem {
                        venue: ctx.accounts.venue.to_account_info(),
                        benchmark: ctx.accounts.benchmark.to_account_info(),
                        owner: ctx.accounts.trader.to_account_info(),
                        owner_usdc: ctx.accounts.usdc_account.to_account_info(),
                        owner_receipts: ctx.accounts.receipts_account.to_account_info(),
                        reserve: ctx.accounts.venue_reserve.to_account_info(),
                        receipt_mint: ctx.accounts.receipt_mint.to_account_info(),
                        usdc_mint: ctx.accounts.usdc_mint.to_account_info(),
                        token_program: ctx.accounts.token_program.to_account_info(),
                        event_authority: ctx.accounts.venue_event_authority.to_account_info(),
                        program: ctx.accounts.venue_program.to_account_info(),
                    },
                    &[trader_seeds],
                ),
                receipts,
                0,
            )?;
        }

        // 3. The fee on the yield, half and half.
        ctx.accounts.usdc_account.reload()?;
        let total = ctx.accounts.usdc_account.amount;
        let deposits = ctx.accounts.series.deposits;
        let fee_bp = u128::from(ctx.accounts.series.fee_bp);
        let yield_ = total.saturating_sub(deposits);
        let fee = u64::try_from(
            u128::from(yield_)
                .checked_mul(fee_bp)
                .ok_or(VaultError::Overflow)?
                .checked_div(10_000)
                .ok_or(VaultError::Overflow)?,
        )
        .map_err(|_| VaultError::Overflow)?;
        let half = fee.checked_div(2).ok_or(VaultError::Overflow)?;
        let other = fee.checked_sub(half).ok_or(VaultError::Overflow)?;
        if half > 0 {
            pay_from_series(
                &ctx.accounts.series,
                &ctx.accounts.trader,
                &ctx.accounts.usdc_account,
                &ctx.accounts.treasury,
                &ctx.accounts.usdc_mint,
                &ctx.accounts.token_program,
                half,
            )?;
        }
        if other > 0 {
            pay_from_series(
                &ctx.accounts.series,
                &ctx.accounts.trader,
                &ctx.accounts.usdc_account,
                &ctx.accounts.buyback_escrow,
                &ctx.accounts.usdc_mint,
                &ctx.accounts.token_program,
                other,
            )?;
        }
        let s = &mut ctx.accounts.series;
        s.status = SeriesStatus::Settled;
        s.settled_assets = total.checked_sub(fee).ok_or(VaultError::Overflow)?;
        s.fee_paid = fee;
        s.receipts = 0;
        emit_cpi!(SeriesSettled {
            series: s.key(),
            total,
            fee,
            settled_assets: s.settled_assets,
            deposits
        });
        Ok(())
    }

    /// Redeems shares pro rata for the settled USDC.
    pub fn redeem(ctx: Context<Redeem>, shares: u64) -> Result<()> {
        require!(shares > 0, VaultError::Zero);
        require!(
            ctx.accounts.series.status == SeriesStatus::Settled,
            VaultError::Status
        );
        let supply = ctx.accounts.share_mint.supply;
        require!(supply > 0, VaultError::Zero);
        let assets = ctx.accounts.usdc_account.amount;
        let amount = u64::try_from(
            u128::from(shares)
                .checked_mul(u128::from(assets))
                .ok_or(VaultError::Overflow)?
                .checked_div(u128::from(supply))
                .ok_or(VaultError::Overflow)?,
        )
        .map_err(|_| VaultError::Overflow)?;
        token_interface::burn(
            CpiContext::new(
                ctx.accounts.token_program.key(),
                Burn {
                    mint: ctx.accounts.share_mint.to_account_info(),
                    from: ctx.accounts.holder_shares.to_account_info(),
                    authority: ctx.accounts.holder.to_account_info(),
                },
            ),
            shares,
        )?;
        if amount > 0 {
            pay_from_series(
                &ctx.accounts.series,
                &ctx.accounts.trader,
                &ctx.accounts.usdc_account,
                &ctx.accounts.holder_usdc,
                &ctx.accounts.usdc_mint,
                &ctx.accounts.token_program,
                amount,
            )?;
        }
        emit_cpi!(Redeemed {
            series: ctx.accounts.series.key(),
            holder: ctx.accounts.holder.key(),
            shares,
            amount
        });
        Ok(())
    }
}

/// The AMM's collateral rule: `notional × bp / 10 000`, rounded up.
fn collateral_for(notional: u64, coll_bp: u128) -> Result<u64> {
    u64::try_from(
        u128::from(notional)
            .checked_mul(coll_bp)
            .ok_or(VaultError::Overflow)?
            .div_ceil(10_000),
    )
    .map_err(|_| VaultError::Overflow.into())
}

/// Moves USDC out of the series' account, signed by its trader PDA.
fn pay_from_series<'info>(
    series: &Account<'info, Series>,
    trader: &UncheckedAccount<'info>,
    from: &InterfaceAccount<'info, TokenAccount>,
    to: &InterfaceAccount<'info, TokenAccount>,
    mint: &InterfaceAccount<'info, Mint>,
    token_program: &Interface<'info, TokenInterface>,
    amount: u64,
) -> Result<()> {
    let series_key = series.key();
    let seeds: &[&[u8]] = &[
        Series::TRADER_SEED,
        series_key.as_ref(),
        &[series.trader_bump],
    ];
    token_interface::transfer_checked(
        CpiContext::new_with_signer(
            token_program.key(),
            TransferChecked {
                from: from.to_account_info(),
                mint: mint.to_account_info(),
                to: to.to_account_info(),
                authority: trader.to_account_info(),
            },
            &[seeds],
        ),
        amount,
        mint.decimals,
    )
}

// ---------------------------------------------------------------- state

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq, Eq, InitSpace, Debug)]
pub enum SeriesStatus {
    Subscribing,
    Hedged,
    Settled,
    Cancelled,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum CancelReason {
    BelowMinimum,
    Unhedged,
    Authority,
}

#[account]
#[derive(InitSpace)]
pub struct Series {
    pub version: u8,
    pub authority: Pubkey,
    pub pool: Pubkey,
    pub benchmark: Pubkey,
    pub venue: Pubkey,
    pub usdc_mint: Pubkey,
    pub share_mint: Pubkey,
    /// System-owned PDA that is the trader on the swap and the owner on the venue.
    pub trader: Pubkey,
    pub usdc_account: Pubkey,
    pub receipts_account: Pubkey,
    pub series_id: u16,
    pub tenor: u8,
    pub status: SeriesStatus,
    pub created_ts: i64,
    pub subscribe_until_ts: i64,
    pub hedged_ts: i64,
    pub matures_ts: i64,
    /// USDC subscribed and still in the series (before the hedge) or hedged (after it).
    pub deposits: u64,
    pub cap: u64,
    pub min_total: u64,
    pub fee_bp: u16,
    pub fixed_bp: u16,
    pub notional: u64,
    pub collateral: u64,
    pub placed: u64,
    pub receipts: u64,
    pub swap: Pubkey,
    pub client_seed: u64,
    pub settled_assets: u64,
    pub fee_paid: u64,
    pub bump: u8,
    pub trader_bump: u8,
    /// The lowest fixed rate the hedge may commit the series to, in basis points. Set at creation.
    pub min_fixed_bp: u16,
    pub reserved: [u8; 62],
}
impl Series {
    pub const SEED: &'static [u8] = b"series";
    pub const TRADER_SEED: &'static [u8] = b"trader";
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy)]
pub struct CreateSeriesArgs {
    pub series_id: u16,
    pub tenor: u8,
    pub subscribe_seconds: i64,
    pub cap: u64,
    pub min_total: u64,
    pub fee_bp: u16,
    pub client_seed: u64,
    /// The lowest fixed rate the hedge may commit the series to, in basis points.
    pub min_fixed_bp: u16,
}

// ---------------------------------------------------------------- accounts

#[event_cpi]
#[derive(Accounts)]
#[instruction(args: CreateSeriesArgs)]
pub struct CreateSeries<'info> {
    #[account(mut)]
    pub authority: Signer<'info>,
    #[account(
        address = bpf_loader_upgradeable::get_program_data_address(&crate::ID) @ VaultError::NotUpgradeAuthority,
        constraint = program_data.upgrade_authority_address == Some(authority.key()) @ VaultError::NotUpgradeAuthority,
    )]
    pub program_data: Account<'info, ProgramData>,
    #[account(seeds = [b"pool", pool.benchmark.as_ref()], bump = pool.bump, seeds::program = swap_amm::ID)]
    pub pool: Box<Account<'info, AmmPool>>,
    #[account(seeds = [Venue::SEED, venue.benchmark.as_ref()], bump = venue.bump, seeds::program = brink_venue::ID, has_one = usdc_mint, has_one = receipt_mint)]
    pub venue: Box<Account<'info, Venue>>,
    pub usdc_mint: Box<InterfaceAccount<'info, Mint>>,
    pub receipt_mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(init, payer = authority, space = 8 + Series::INIT_SPACE, seeds = [Series::SEED, pool.key().as_ref(), &args.series_id.to_le_bytes()], bump)]
    pub series: Box<Account<'info, Series>>,
    /// CHECK: system-owned PDA, the series' trader; holds only lamports.
    #[account(mut, seeds = [Series::TRADER_SEED, series.key().as_ref()], bump)]
    pub trader: UncheckedAccount<'info>,
    #[account(init, payer = authority, seeds = [b"shares", series.key().as_ref()], bump, mint::decimals = SHARE_DECIMALS, mint::authority = series, mint::token_program = token_program)]
    pub share_mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(init, payer = authority, seeds = [b"usdc", series.key().as_ref()], bump, token::mint = usdc_mint, token::authority = trader, token::token_program = token_program)]
    pub usdc_account: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(init, payer = authority, seeds = [b"receipts", series.key().as_ref()], bump, token::mint = receipt_mint, token::authority = trader, token::token_program = token_program)]
    pub receipts_account: Box<InterfaceAccount<'info, TokenAccount>>,
    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

#[event_cpi]
#[derive(Accounts)]
pub struct Deposit<'info> {
    #[account(mut, seeds = [Series::SEED, series.pool.as_ref(), &series.series_id.to_le_bytes()], bump = series.bump, has_one = usdc_mint, has_one = share_mint, has_one = usdc_account)]
    pub series: Box<Account<'info, Series>>,
    pub depositor: Signer<'info>,
    #[account(mut, constraint = depositor_usdc.owner == depositor.key() @ VaultError::TokenOwner, constraint = depositor_usdc.mint == usdc_mint.key() @ VaultError::Mint)]
    pub depositor_usdc: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, constraint = depositor_shares.owner == depositor.key() @ VaultError::TokenOwner, constraint = depositor_shares.mint == share_mint.key() @ VaultError::Mint)]
    pub depositor_shares: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut)]
    pub usdc_account: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut)]
    pub share_mint: Box<InterfaceAccount<'info, Mint>>,
    pub usdc_mint: Box<InterfaceAccount<'info, Mint>>,
    pub token_program: Interface<'info, TokenInterface>,
}

#[event_cpi]
#[derive(Accounts)]
pub struct Withdraw<'info> {
    #[account(mut, seeds = [Series::SEED, series.pool.as_ref(), &series.series_id.to_le_bytes()], bump = series.bump, has_one = usdc_mint, has_one = share_mint, has_one = usdc_account, has_one = trader)]
    pub series: Box<Account<'info, Series>>,
    /// CHECK: the series' trader PDA, owner of the USDC account; bound by `has_one`.
    pub trader: UncheckedAccount<'info>,
    pub holder: Signer<'info>,
    #[account(mut, constraint = holder_usdc.mint == usdc_mint.key() @ VaultError::Mint)]
    pub holder_usdc: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, constraint = holder_shares.owner == holder.key() @ VaultError::TokenOwner, constraint = holder_shares.mint == share_mint.key() @ VaultError::Mint)]
    pub holder_shares: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut)]
    pub usdc_account: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut)]
    pub share_mint: Box<InterfaceAccount<'info, Mint>>,
    pub usdc_mint: Box<InterfaceAccount<'info, Mint>>,
    pub token_program: Interface<'info, TokenInterface>,
}

#[event_cpi]
#[derive(Accounts)]
pub struct Hedge<'info> {
    #[account(mut, seeds = [Series::SEED, series.pool.as_ref(), &series.series_id.to_le_bytes()], bump = series.bump, has_one = pool, has_one = benchmark, has_one = venue, has_one = usdc_mint, has_one = usdc_account, has_one = receipts_account, has_one = trader)]
    pub series: Box<Account<'info, Series>>,
    /// CHECK: the series' trader PDA; signs the CPIs with the program's seeds.
    #[account(mut)]
    pub trader: UncheckedAccount<'info>,
    pub cranker: Signer<'info>,
    // AMM
    #[account(seeds = [b"global"], bump = amm_global.bump, seeds::program = swap_amm::ID)]
    pub amm_global: Box<Account<'info, AmmGlobal>>,
    #[account(mut)]
    pub pool: Box<Account<'info, AmmPool>>,
    pub benchmark: Box<Account<'info, Benchmark>>,
    /// CHECK: the swap PDA the AMM creates at `["swap", pool, trader, client_seed]`; read back after the CPI.
    #[account(mut, seeds = [b"swap", pool.key().as_ref(), trader.key().as_ref(), &series.client_seed.to_le_bytes()], bump, seeds::program = swap_amm::ID)]
    pub swap: UncheckedAccount<'info>,
    #[account(mut, address = pool.vault)]
    pub pool_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    /// CHECK: the AMM's event authority PDA, checked by the AMM.
    pub amm_event_authority: UncheckedAccount<'info>,
    pub swap_program: Program<'info, SwapAmm>,
    // Venue
    #[account(mut, has_one = receipt_mint @ VaultError::Venue)]
    pub venue: Box<Account<'info, Venue>>,
    #[account(mut, address = venue.reserve)]
    pub venue_reserve: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut)]
    pub receipt_mint: Box<InterfaceAccount<'info, Mint>>,
    /// CHECK: the venue's event authority PDA, checked by the venue program.
    pub venue_event_authority: UncheckedAccount<'info>,
    pub venue_program: Program<'info, BrinkVenue>,
    // Series token accounts
    #[account(mut)]
    pub usdc_account: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut)]
    pub receipts_account: Box<InterfaceAccount<'info, TokenAccount>>,
    pub usdc_mint: Box<InterfaceAccount<'info, Mint>>,
    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

#[event_cpi]
#[derive(Accounts)]
pub struct Cancel<'info> {
    #[account(mut, seeds = [Series::SEED, series.pool.as_ref(), &series.series_id.to_le_bytes()], bump = series.bump)]
    pub series: Box<Account<'info, Series>>,
    pub signer: Signer<'info>,
}

#[event_cpi]
#[derive(Accounts)]
pub struct Settle<'info> {
    #[account(mut, seeds = [Series::SEED, series.pool.as_ref(), &series.series_id.to_le_bytes()], bump = series.bump, has_one = pool, has_one = benchmark, has_one = venue, has_one = usdc_mint, has_one = usdc_account, has_one = receipts_account, has_one = trader, has_one = swap)]
    pub series: Box<Account<'info, Series>>,
    /// CHECK: the series' trader PDA; signs the CPIs with the program's seeds.
    #[account(mut)]
    pub trader: UncheckedAccount<'info>,
    pub cranker: Signer<'info>,
    // AMM
    #[account(seeds = [b"global"], bump = amm_global.bump, seeds::program = swap_amm::ID, has_one = treasury @ VaultError::FeeDestination, has_one = buyback_escrow @ VaultError::FeeDestination)]
    pub amm_global: Box<Account<'info, AmmGlobal>>,
    #[account(mut)]
    pub pool: Box<Account<'info, AmmPool>>,
    pub benchmark: Box<Account<'info, Benchmark>>,
    /// CHECK: the series' swap; closed by the AMM when settled, so it may already be empty. Bound by `has_one`.
    #[account(mut)]
    pub swap: UncheckedAccount<'info>,
    #[account(mut, address = pool.vault)]
    pub pool_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    /// CHECK: the AMM's event authority PDA, checked by the AMM.
    pub amm_event_authority: UncheckedAccount<'info>,
    pub swap_program: Program<'info, SwapAmm>,
    #[account(mut)]
    pub treasury: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut)]
    pub buyback_escrow: Box<InterfaceAccount<'info, TokenAccount>>,
    // Venue
    #[account(mut, has_one = receipt_mint @ VaultError::Venue)]
    pub venue: Box<Account<'info, Venue>>,
    #[account(mut, address = venue.reserve)]
    pub venue_reserve: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut)]
    pub receipt_mint: Box<InterfaceAccount<'info, Mint>>,
    /// CHECK: the venue's event authority PDA, checked by the venue program.
    pub venue_event_authority: UncheckedAccount<'info>,
    pub venue_program: Program<'info, BrinkVenue>,
    // Series token accounts
    #[account(mut)]
    pub usdc_account: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut)]
    pub receipts_account: Box<InterfaceAccount<'info, TokenAccount>>,
    pub usdc_mint: Box<InterfaceAccount<'info, Mint>>,
    pub token_program: Interface<'info, TokenInterface>,
}

#[event_cpi]
#[derive(Accounts)]
pub struct Redeem<'info> {
    #[account(mut, seeds = [Series::SEED, series.pool.as_ref(), &series.series_id.to_le_bytes()], bump = series.bump, has_one = usdc_mint, has_one = share_mint, has_one = usdc_account, has_one = trader)]
    pub series: Box<Account<'info, Series>>,
    /// CHECK: the series' trader PDA, owner of the USDC account; bound by `has_one`.
    pub trader: UncheckedAccount<'info>,
    pub holder: Signer<'info>,
    #[account(mut, constraint = holder_usdc.mint == usdc_mint.key() @ VaultError::Mint)]
    pub holder_usdc: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, constraint = holder_shares.owner == holder.key() @ VaultError::TokenOwner, constraint = holder_shares.mint == share_mint.key() @ VaultError::Mint)]
    pub holder_shares: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut)]
    pub usdc_account: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut)]
    pub share_mint: Box<InterfaceAccount<'info, Mint>>,
    pub usdc_mint: Box<InterfaceAccount<'info, Mint>>,
    pub token_program: Interface<'info, TokenInterface>,
}

// ---------------------------------------------------------------- events

#[event]
pub struct SeriesCreated {
    pub series: Pubkey,
    pub pool: Pubkey,
    pub venue: Pubkey,
    pub series_id: u16,
    pub tenor: u8,
    pub subscribe_until_ts: i64,
    pub cap: u64,
    pub min_total: u64,
    pub fee_bp: u16,
    pub min_fixed_bp: u16,
}
#[event]
pub struct Subscribed {
    pub series: Pubkey,
    pub depositor: Pubkey,
    pub amount: u64,
    pub deposits: u64,
}
#[event]
pub struct Withdrawn {
    pub series: Pubkey,
    pub holder: Pubkey,
    pub shares: u64,
    pub amount: u64,
}
#[event]
pub struct Hedged {
    pub series: Pubkey,
    pub swap: Pubkey,
    pub fixed_bp: u16,
    pub notional: u64,
    pub collateral: u64,
    pub placed: u64,
    pub receipts: u64,
    pub matures_ts: i64,
}
#[event]
pub struct SeriesCancelled {
    pub series: Pubkey,
    pub deposits: u64,
    pub reason: CancelReason,
}
#[event]
pub struct SeriesSettled {
    pub series: Pubkey,
    pub total: u64,
    pub fee: u64,
    pub settled_assets: u64,
    pub deposits: u64,
}
#[event]
pub struct Redeemed {
    pub series: Pubkey,
    pub holder: Pubkey,
    pub shares: u64,
    pub amount: u64,
}

// ---------------------------------------------------------------- errors

#[error_code]
pub enum VaultError {
    #[msg("Only the program's upgrade authority may create a series")]
    NotUpgradeAuthority,
    #[msg("The subscription window must be between one hour and seven days")]
    Window,
    #[msg("The fee on yield may not exceed 20 percent")]
    Fee,
    #[msg("Invalid tenor index")]
    Tenor,
    #[msg("The cap and minimum must be positive, the minimum within the cap, and the cap within the pool's maximum notional")]
    Caps,
    #[msg("The venue is not on the pool's benchmark")]
    VenueBenchmark,
    #[msg("Amount must be positive")]
    Zero,
    #[msg("The series is not in the state this action needs")]
    Status,
    #[msg("The subscription window has closed")]
    WindowClosed,
    #[msg("The subscription window is still open")]
    WindowOpen,
    #[msg("The deposit would exceed the series cap")]
    Cap,
    #[msg("Arithmetic overflow")]
    Overflow,
    #[msg("Could not read the swap the AMM wrote")]
    SwapRead,
    #[msg("Only the authority may cancel before the hedge grace has passed")]
    NotYet,
    #[msg("The series has not matured")]
    NotMatured,
    #[msg("Token account owner mismatch")]
    TokenOwner,
    #[msg("Token mint mismatch")]
    Mint,
    #[msg("Venue accounts do not match the venue")]
    Venue,
    #[msg("Fee destinations must be the AMM's treasury and buyback escrow")]
    FeeDestination,
    #[msg("The hedge would commit the series to a fixed rate below its floor")]
    HedgeRateFloor,
}
