//! The Stacked Treasuries reserve: idle LP capital placed at a `brink_venue` venue that accrues a published
//! Treasury benchmark, so the LPs of a pool earn that base yield under the pool's spread.
//!
//! Money rules. The pool keeps a working balance in its vault for payouts, a governance-set share of LP capital
//! with a band either side. Capital placed at the venue is counted in conservation through `Pool::reserve_placed`
//! (`vault + reserve_placed >= tvl + collateral_held + fees_held + withdraw_reserved`). The crank is permissionless:
//! it realises accrued yield into LP capital first, then places the excess above the band or recalls the shortfall
//! below it. A recall for a short working balance is never delayed by the pause or the interval, so the reserve
//! can never starve a payout for longer than one crank. The bounty is paid from harvested yield only, never from
//! principal, so an unprofitable crank earns nothing and the reserve cannot be drained by cranking it.
//!
//! Accrued yield that no crank has realised yet is still LP capital: `pending_yield` values the pool's receipts
//! at the venue's index reconstructed for the current instant (`Venue::index_at`, no CPI), scaled by what the
//! venue's reserve funds and net of the bounty, and every LP pricing path adds it to the capital it prices on
//! (external scan 2, finding 6). A pool with a reserve therefore requires the reserve account set on every
//! deposit, withdrawal, queue processing and claim (`ReserveAccountsRequired`).
use crate::{errors::BrinkError, state::*};
use anchor_lang::prelude::*;
use anchor_spl::token_interface::{
    transfer_checked, Mint, TokenAccount, TokenInterface, TransferChecked,
};
use brink_index::Benchmark;
use brink_venue::program::BrinkVenue;
use brink_venue::Venue;

/// Smallest yield worth a harvest, USDC 6 dp (one cent).
pub const MIN_HARVEST: u64 = 10_000;
/// Smallest placement or recall, USDC 6 dp (one dollar).
pub const MIN_MOVE: u64 = 1_000_000;
/// Bounty share of harvested yield, in basis points.
pub const BOUNTY_BP: u64 = 100;

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug)]
pub struct EnableReserveArgs {
    pub params: ReserveParams,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug)]
pub struct SetReserveArgs {
    pub params: ReserveParams,
    pub paused: bool,
}

#[event_cpi]
#[derive(Accounts)]
pub struct AdminEnableReserve<'info> {
    #[account(seeds = [b"global"], bump = global.bump, has_one = authority, has_one = usdc_mint, has_one = token_program)]
    pub global: Box<Account<'info, Global>>,
    pub authority: Signer<'info>,
    #[account(mut)]
    pub payer: Signer<'info>,
    #[account(mut, seeds = [b"pool", pool.benchmark.as_ref()], bump = pool.bump)]
    pub pool: Box<Account<'info, Pool>>,
    /// The venue the idle capital is placed at: settles in the pool's USDC and issues the receipt mint below.
    #[account(seeds = [Venue::SEED, venue.benchmark.as_ref()], bump = venue.bump, seeds::program = brink_venue::ID,
        constraint = venue.usdc_mint == usdc_mint.key() @ BrinkError::ReserveVenue,
        constraint = venue.receipt_mint == receipt_mint.key() @ BrinkError::ReserveVenue)]
    pub venue: Box<Account<'info, Venue>>,
    pub receipt_mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(init, payer = payer, space = 8 + PoolReserve::INIT_SPACE, seeds = [b"reserve", pool.key().as_ref()], bump)]
    pub reserve: Box<Account<'info, PoolReserve>>,
    #[account(init, payer = payer, seeds = [b"receipts", pool.key().as_ref()], bump, token::mint = receipt_mint, token::authority = pool, token::token_program = token_program)]
    pub receipts: Box<InterfaceAccount<'info, TokenAccount>>,
    pub usdc_mint: Box<InterfaceAccount<'info, Mint>>,
    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

pub fn enable_reserve(ctx: Context<AdminEnableReserve>, args: EnableReserveArgs) -> Result<()> {
    args.params.validate()?;
    require!(
        ctx.accounts.pool.reserve_active == 0,
        BrinkError::ReserveParams
    );
    let r = &mut ctx.accounts.reserve;
    r.version = 1;
    r.pool = ctx.accounts.pool.key();
    r.venue = ctx.accounts.venue.key();
    r.receipt_mint = ctx.accounts.receipt_mint.key();
    r.receipts = ctx.accounts.receipts.key();
    r.params = args.params;
    r.bump = ctx.bumps.reserve;
    r.last_place_slot = 0;
    let pool = &mut ctx.accounts.pool;
    pool.reserve_active = 1;
    pool.event_seq = pool.event_seq.checked_add(1).ok_or(BrinkError::Sequence)?;
    emit_cpi!(ReserveEnabled {
        pool: pool.key(),
        reserve: ctx.accounts.reserve.key(),
        venue: ctx.accounts.venue.key(),
        params: args.params,
        seq: pool.event_seq,
    });
    Ok(())
}

#[event_cpi]
#[derive(Accounts)]
pub struct AdminSetReserve<'info> {
    #[account(seeds = [b"global"], bump = global.bump, has_one = authority)]
    pub global: Box<Account<'info, Global>>,
    pub authority: Signer<'info>,
    #[account(mut, seeds = [b"pool", pool.benchmark.as_ref()], bump = pool.bump)]
    pub pool: Box<Account<'info, Pool>>,
    #[account(mut, seeds = [b"reserve", pool.key().as_ref()], bump = reserve.bump, has_one = pool)]
    pub reserve: Box<Account<'info, PoolReserve>>,
}

pub fn set_reserve(ctx: Context<AdminSetReserve>, args: SetReserveArgs) -> Result<()> {
    args.params.validate()?;
    let r = &mut ctx.accounts.reserve;
    r.params = args.params;
    r.paused = args.paused;
    let pool = &mut ctx.accounts.pool;
    pool.event_seq = pool.event_seq.checked_add(1).ok_or(BrinkError::Sequence)?;
    emit_cpi!(ReserveConfigured {
        pool: pool.key(),
        params: args.params,
        paused: args.paused,
        seq: pool.event_seq,
    });
    Ok(())
}

#[event_cpi]
#[derive(Accounts)]
pub struct CrankRebalanceReserve<'info> {
    #[account(seeds = [b"global"], bump = global.bump, has_one = usdc_mint, has_one = token_program)]
    pub global: Box<Account<'info, Global>>,
    #[account(mut, seeds = [b"pool", pool.benchmark.as_ref()], bump = pool.bump, has_one = vault)]
    pub pool: Box<Account<'info, Pool>>,
    #[account(mut, seeds = [b"reserve", pool.key().as_ref()], bump = reserve.bump, has_one = pool, has_one = venue, has_one = receipts, has_one = receipt_mint)]
    pub reserve: Box<Account<'info, PoolReserve>>,
    #[account(mut)]
    pub vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut)]
    pub receipts: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, seeds = [Venue::SEED, venue.benchmark.as_ref()], bump = venue.bump, seeds::program = brink_venue::ID,
        constraint = venue.benchmark == venue_benchmark.key() @ BrinkError::ReserveVenue,
        constraint = venue.reserve == venue_reserve.key() @ BrinkError::ReserveVenue)]
    pub venue: Box<Account<'info, Venue>>,
    pub venue_benchmark: Box<Account<'info, Benchmark>>,
    #[account(mut)]
    pub venue_reserve: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut)]
    pub receipt_mint: Box<InterfaceAccount<'info, Mint>>,
    pub usdc_mint: Box<InterfaceAccount<'info, Mint>>,
    pub cranker: Signer<'info>,
    #[account(mut, constraint = cranker_usdc.mint == usdc_mint.key() @ BrinkError::SettlementMint)]
    pub cranker_usdc: Box<InterfaceAccount<'info, TokenAccount>>,
    /// CHECK: the venue program's event authority PDA, checked by the venue program on the CPI.
    pub venue_event_authority: UncheckedAccount<'info>,
    pub venue_program: Program<'info, BrinkVenue>,
    pub token_program: Interface<'info, TokenInterface>,
}

/// What one crank did, for the event and the tests.
#[derive(Default, Debug, Clone, Copy)]
struct Moves {
    harvested: u64,
    bounty: u64,
    placed: u64,
    recalled: u64,
}

#[allow(clippy::too_many_lines)]
pub fn rebalance_reserve(ctx: Context<CrankRebalanceReserve>) -> Result<()> {
    require!(
        ctx.accounts.pool.reserve_active == 1,
        BrinkError::ReserveInactive
    );
    let mode = ctx.accounts.global.mode;
    require!(mode != OperatingMode::Halted, BrinkError::Halted);
    let clock = Clock::get()?;
    super::swap::apply_pending(&mut ctx.accounts.pool, clock.slot);

    // 1. Bring the venue's index to now, then value the position.
    brink_venue::cpi::touch(CpiContext::new(
        ctx.accounts.venue_program.key(),
        brink_venue::cpi::accounts::Touch {
            venue: ctx.accounts.venue.to_account_info(),
            benchmark: ctx.accounts.venue_benchmark.to_account_info(),
            event_authority: ctx.accounts.venue_event_authority.to_account_info(),
            program: ctx.accounts.venue_program.to_account_info(),
        },
    ))?;
    ctx.accounts.venue.reload()?;
    // A venue further behind than its walk bound advances one bound per touch (ADR-020 item 5); the crank
    // values nothing against an index that is not at now.
    require!(
        ctx.accounts.venue.last_ts >= clock.unix_timestamp,
        BrinkError::VenueNeedsTouch
    );
    let index = ctx.accounts.venue.index_e18;
    let held = ctx.accounts.receipts.amount;
    let value = brink_venue::amount_for(held, index)?;
    let placed = ctx.accounts.pool.reserve_placed;
    let accrued = value.saturating_sub(placed);
    let mut m = Moves::default();

    let benchmark = ctx.accounts.pool.benchmark;
    let bump = ctx.accounts.pool.bump;
    let seeds: &[&[u8]] = &[b"pool", benchmark.as_ref(), &[bump]];

    // 2. Harvest: realise the accrued yield into LP capital, when the venue's reserve funds every claim on it so
    //    the redemption is paid in full (the venue pays a short reserve pro rata; a harvest never accepts that).
    if accrued >= MIN_HARVEST {
        let funded = brink_venue::Venue::fully_funded(
            ctx.accounts.receipt_mint.supply,
            index,
            ctx.accounts.venue_reserve.amount,
        )?;
        if funded {
            let burn = brink_venue::receipts_for(accrued, index)?.min(held);
            if burn > 0 {
                let before = ctx.accounts.vault.amount;
                redeem(&ctx, seeds, burn, brink_venue::amount_for(burn, index)?)?;
                ctx.accounts.vault.reload()?;
                m.harvested = ctx
                    .accounts
                    .vault
                    .amount
                    .checked_sub(before)
                    .ok_or(BrinkError::Overflow)?;
            }
        }
    }
    if m.harvested > 0 {
        let cap = ctx.accounts.reserve.params.bounty_cap;
        m.bounty =
            u64::try_from(u128::from(m.harvested).saturating_mul(u128::from(BOUNTY_BP)) / 10_000)
                .unwrap_or(0)
                .min(cap);
        if m.bounty > 0 {
            transfer_checked(
                CpiContext::new_with_signer(
                    ctx.accounts.token_program.key(),
                    TransferChecked {
                        from: ctx.accounts.vault.to_account_info(),
                        mint: ctx.accounts.usdc_mint.to_account_info(),
                        to: ctx.accounts.cranker_usdc.to_account_info(),
                        authority: ctx.accounts.pool.to_account_info(),
                    },
                    &[seeds],
                ),
                m.bounty,
                ctx.accounts.usdc_mint.decimals,
            )?;
        }
        let net = m
            .harvested
            .checked_sub(m.bounty)
            .ok_or(BrinkError::Overflow)?;
        let pool = &mut ctx.accounts.pool;
        pool.tvl = pool.tvl.checked_add(net).ok_or(BrinkError::Overflow)?;
        let r = &mut ctx.accounts.reserve;
        r.yield_realised = r
            .yield_realised
            .checked_add(net)
            .ok_or(BrinkError::Overflow)?;
        r.bounties_paid = r
            .bounties_paid
            .checked_add(m.bounty)
            .ok_or(BrinkError::Overflow)?;
    }

    // 3. The working balance against its target.
    ctx.accounts.vault.reload()?;
    ctx.accounts.receipts.reload()?;
    let working = ctx.accounts.vault.amount;
    let p = &ctx.accounts.pool;
    let params = ctx.accounts.reserve.params;
    let lp = u128::from(p.tvl);
    let share = |bps: u16| -> Result<u64> {
        u64::try_from(lp.saturating_mul(u128::from(bps)) / 10_000)
            .map_err(|_| error!(BrinkError::Overflow))
    };
    let target = share(params.working_bps)?;
    let band = share(params.band_bps)?;
    let ceiling = share(params.max_place_bps)?;
    let liquid_need = p
        .collateral_held
        .checked_add(p.fees_held()?)
        .and_then(|x| x.checked_add(p.withdraw_reserved))
        .and_then(|x| x.checked_add(target))
        .ok_or(BrinkError::Overflow)?;
    // The interval runs from the last placement, not the last crank, so harvests and recalls cannot postpone a
    // placement (external scan 2, finding 12).
    let placing_allowed = !ctx.accounts.reserve.paused
        && !ctx.accounts.venue.paused
        && matches!(mode, OperatingMode::Normal | OperatingMode::Limited)
        && clock
            .slot
            .saturating_sub(ctx.accounts.reserve.last_place_slot)
            >= params.min_interval_slots;

    // The venue refuses a placement while its reserve is short of the yield it owes, since the newcomer's
    // principal would fund earlier holders' yield; the crank rests on that leg rather than failing, so a short
    // venue never stops a harvest or a recall in the same crank.
    ctx.accounts.receipt_mint.reload()?;
    ctx.accounts.venue_reserve.reload()?;
    let venue_funded = brink_venue::Venue::fully_funded(
        ctx.accounts.receipt_mint.supply,
        index,
        ctx.accounts.venue_reserve.amount,
    )?;
    if working > liquid_need.saturating_add(band) && placing_allowed && venue_funded {
        let excess = working
            .checked_sub(liquid_need)
            .ok_or(BrinkError::Overflow)?;
        let room = ceiling.saturating_sub(ctx.accounts.pool.reserve_placed);
        let amount = excess.min(room);
        if amount >= MIN_MOVE {
            let min_receipts = brink_venue::receipts_for(amount, index)?.saturating_sub(1);
            let receipts_before = ctx.accounts.receipts.amount;
            brink_venue::cpi::place(
                CpiContext::new_with_signer(
                    ctx.accounts.venue_program.key(),
                    brink_venue::cpi::accounts::Place {
                        venue: ctx.accounts.venue.to_account_info(),
                        benchmark: ctx.accounts.venue_benchmark.to_account_info(),
                        owner: ctx.accounts.pool.to_account_info(),
                        owner_usdc: ctx.accounts.vault.to_account_info(),
                        owner_receipts: ctx.accounts.receipts.to_account_info(),
                        reserve: ctx.accounts.venue_reserve.to_account_info(),
                        receipt_mint: ctx.accounts.receipt_mint.to_account_info(),
                        usdc_mint: ctx.accounts.usdc_mint.to_account_info(),
                        token_program: ctx.accounts.token_program.to_account_info(),
                        event_authority: ctx.accounts.venue_event_authority.to_account_info(),
                        program: ctx.accounts.venue_program.to_account_info(),
                    },
                    &[seeds],
                ),
                amount,
                min_receipts,
            )?;
            m.placed = amount;
            // Booked at what the receipts received redeem for at this index, not at the USDC sent: the unit
            // the receipt rounding cannot return is charged to LP capital now rather than carried as backing
            // until the last recall writes it off (external scan 2, finding 22).
            ctx.accounts.receipts.reload()?;
            let received = ctx
                .accounts
                .receipts
                .amount
                .checked_sub(receipts_before)
                .ok_or(BrinkError::Overflow)?;
            let backing = brink_venue::amount_for(received, index)?.min(amount);
            let rounding = amount.checked_sub(backing).ok_or(BrinkError::Overflow)?;
            let pool = &mut ctx.accounts.pool;
            pool.reserve_placed = pool
                .reserve_placed
                .checked_add(backing)
                .ok_or(BrinkError::Overflow)?;
            pool.tvl = pool.tvl.checked_sub(rounding).ok_or(BrinkError::Overflow)?;
            let r = &mut ctx.accounts.reserve;
            r.placed_lifetime = r
                .placed_lifetime
                .checked_add(backing)
                .ok_or(BrinkError::Overflow)?;
            r.last_place_slot = clock.slot;
        }
    } else if working.saturating_add(band) < liquid_need {
        // Short: recall from the venue. Never delayed by the pause, the interval or Limited mode.
        let short = liquid_need
            .checked_sub(working)
            .ok_or(BrinkError::Overflow)?;
        let a = RecallAccounts {
            receipts: &ctx.accounts.receipts.to_account_info(),
            venue: &ctx.accounts.venue.to_account_info(),
            venue_benchmark: &ctx.accounts.venue_benchmark.to_account_info(),
            venue_reserve: &ctx.accounts.venue_reserve.to_account_info(),
            receipt_mint: &ctx.accounts.receipt_mint.to_account_info(),
            venue_event_authority: &ctx.accounts.venue_event_authority.to_account_info(),
            venue_program: &ctx.accounts.venue_program.to_account_info(),
            usdc_mint: &ctx.accounts.usdc_mint.to_account_info(),
            token_program: &ctx.accounts.token_program.to_account_info(),
        };
        let moved = recall_core(
            &a,
            &mut ctx.accounts.pool,
            &mut ctx.accounts.reserve,
            &mut ctx.accounts.vault,
            seeds,
            short,
        )?;
        m.recalled = moved.principal;
    }

    require!(
        m.harvested > 0 || m.placed > 0 || m.recalled > 0,
        BrinkError::ReserveBalanced
    );
    let r = &mut ctx.accounts.reserve;
    r.last_rebalance_slot = clock.slot;
    r.rebalances = r.rebalances.saturating_add(1);
    let pool = &mut ctx.accounts.pool;
    super::math::rebase_utilisation(pool)?;
    pool.event_seq = pool.event_seq.checked_add(1).ok_or(BrinkError::Sequence)?;
    ctx.accounts.vault.reload()?;
    emit_cpi!(ReserveRebalanced {
        pool: pool.key(),
        cranker: ctx.accounts.cranker.key(),
        harvested: m.harvested,
        bounty: m.bounty,
        placed: m.placed,
        recalled: m.recalled,
        reserve_placed: pool.reserve_placed,
        working: ctx.accounts.vault.amount,
        tvl: pool.tvl,
        index_e18: index,
        seq: pool.event_seq,
    });
    ctx.accounts
        .pool
        .assert_conservation(ctx.accounts.vault.amount)
}

/// Burns `receipts` at the venue and pays the pool vault; the pool PDA signs as the receipts' owner.
fn redeem(
    ctx: &Context<CrankRebalanceReserve>,
    seeds: &[&[u8]],
    receipts: u64,
    min_amount: u64,
) -> Result<()> {
    brink_venue::cpi::redeem(
        CpiContext::new_with_signer(
            ctx.accounts.venue_program.key(),
            brink_venue::cpi::accounts::Redeem {
                venue: ctx.accounts.venue.to_account_info(),
                benchmark: ctx.accounts.venue_benchmark.to_account_info(),
                owner: ctx.accounts.pool.to_account_info(),
                owner_usdc: ctx.accounts.vault.to_account_info(),
                owner_receipts: ctx.accounts.receipts.to_account_info(),
                reserve: ctx.accounts.venue_reserve.to_account_info(),
                receipt_mint: ctx.accounts.receipt_mint.to_account_info(),
                usdc_mint: ctx.accounts.usdc_mint.to_account_info(),
                token_program: ctx.accounts.token_program.to_account_info(),
                event_authority: ctx.accounts.venue_event_authority.to_account_info(),
                program: ctx.accounts.venue_program.to_account_info(),
            },
            &[seeds],
        ),
        receipts,
        min_amount,
    )
}

/* ------------------------------------------------------------------ inline recall for payout paths */

/// The reserve accounts a payout path is handed as remaining accounts, in this order: reserve, receipts, venue,
/// venue benchmark, venue reserve, receipt mint, venue event authority, venue program. The reserve PDA, its
/// stored account keys and the venue program id are checked here; the venue program checks its own accounts
/// and the token program checks ownership, so no account in the set can be substituted.
pub struct ReserveRemaining<'info> {
    pub reserve: Account<'info, PoolReserve>,
    pub receipts: &'info AccountInfo<'info>,
    pub venue: &'info AccountInfo<'info>,
    pub venue_benchmark: &'info AccountInfo<'info>,
    pub venue_reserve: &'info AccountInfo<'info>,
    pub receipt_mint: &'info AccountInfo<'info>,
    pub venue_event_authority: &'info AccountInfo<'info>,
    pub venue_program: &'info AccountInfo<'info>,
}

/// Reads the reserve set from `infos`, or `None` when the caller supplied no reserve accounts.
pub fn parse_remaining<'info>(
    pool_key: &Pubkey,
    infos: &'info [AccountInfo<'info>],
) -> Result<Option<ReserveRemaining<'info>>> {
    let [reserve_info, receipts, venue, venue_benchmark, venue_reserve, receipt_mint, venue_event_authority, venue_program, ..] =
        infos
    else {
        return Ok(None);
    };
    let (expected, _) = Pubkey::find_program_address(&[b"reserve", pool_key.as_ref()], &crate::ID);
    require_keys_eq!(reserve_info.key(), expected, BrinkError::ReserveVenue);
    let reserve = Account::<PoolReserve>::try_from(reserve_info)?;
    require_keys_eq!(reserve.pool, *pool_key, BrinkError::ReserveVenue);
    require_keys_eq!(receipts.key(), reserve.receipts, BrinkError::ReserveVenue);
    require_keys_eq!(venue.key(), reserve.venue, BrinkError::ReserveVenue);
    require_keys_eq!(
        receipt_mint.key(),
        reserve.receipt_mint,
        BrinkError::ReserveVenue
    );
    require_keys_eq!(
        venue_program.key(),
        brink_venue::ID,
        BrinkError::ReserveVenue
    );
    {
        // The venue's own benchmark and reserve, so a pure read of the index prices against the right history
        // and the right funding; the venue program re-checks both on every CPI.
        let v = Account::<Venue>::try_from(venue)?;
        require_keys_eq!(venue_benchmark.key(), v.benchmark, BrinkError::ReserveVenue);
        require_keys_eq!(venue_reserve.key(), v.reserve, BrinkError::ReserveVenue);
    }
    Ok(Some(ReserveRemaining {
        reserve,
        receipts,
        venue,
        venue_benchmark,
        venue_reserve,
        receipt_mint,
        venue_event_authority,
        venue_program,
    }))
}

/// Reads the reserve set a pool with an active reserve requires, or `None` for a pool without one. The LP
/// pricing paths call this: a pool whose capital is partly at the venue cannot be priced without the venue
/// (external scan 2, finding 6).
pub fn required_remaining<'info>(
    pool: &Account<'info, Pool>,
    infos: &'info [AccountInfo<'info>],
) -> Result<Option<ReserveRemaining<'info>>> {
    if pool.reserve_active != 1 {
        return Ok(None);
    }
    let r = parse_remaining(&pool.key(), infos)?;
    require!(r.is_some(), BrinkError::ReserveAccountsRequired);
    Ok(r)
}

/// Number of accounts in the reserve set; a hook's extra accounts follow it.
pub const REMAINING_LEN: usize = 8;

/// Yield accrued on the pool's receipts and not yet realised, as LP pricing counts it: the receipts valued at
/// the venue's index for `now` (`Venue::index_at`, a pure read of the venue and its benchmark), scaled by the
/// fraction of all claims the venue's reserve funds, less what the pool has booked as placed and less the crank
/// bounty the harvest will pay. Zero for a pool without a reserve or with nothing accrued.
pub fn pending_yield(r: Option<&ReserveRemaining<'_>>, pool: &Pool, now: i64) -> Result<u64> {
    let Some(r) = r else {
        return Ok(0);
    };
    let held = token_amount(r.receipts)?;
    if held == 0 {
        return Ok(0);
    }
    // Both loads are boxed: the benchmark is a 2.7 KB account and this runs inside LP instructions whose own
    // frames are already large (SBF frames are 4 KB).
    let venue = Box::new(Account::<Venue>::try_from(r.venue)?);
    let benchmark = Box::new(Account::<Benchmark>::try_from(r.venue_benchmark)?);
    let index = venue.index_at(&benchmark, now)?;
    let value = brink_venue::amount_for(held, index)?;
    let claims = brink_venue::amount_for(venue.receipts.max(held), index)?;
    let funded = brink_venue::funded_value(value, claims, token_amount(r.venue_reserve)?)?;
    let accrued = funded.saturating_sub(pool.reserve_placed);
    let bounty = u64::try_from(u128::from(accrued).saturating_mul(u128::from(BOUNTY_BP)) / 10_000)
        .unwrap_or(0)
        .min(r.reserve.params.bounty_cap);
    Ok(accrued.saturating_sub(bounty))
}

/// The venue accounts a recall drives, as plain account infos so the crank and the inline payout paths share
/// one implementation.
pub struct RecallAccounts<'a, 'info> {
    pub receipts: &'a AccountInfo<'info>,
    pub venue: &'a AccountInfo<'info>,
    pub venue_benchmark: &'a AccountInfo<'info>,
    pub venue_reserve: &'a AccountInfo<'info>,
    pub receipt_mint: &'a AccountInfo<'info>,
    pub venue_event_authority: &'a AccountInfo<'info>,
    pub venue_program: &'a AccountInfo<'info>,
    pub usdc_mint: &'a AccountInfo<'info>,
    pub token_program: &'a AccountInfo<'info>,
}

/// What a recall moved: USDC received, the principal it is booked against and the yield that came with it.
#[derive(Default, Clone, Copy)]
pub struct Recalled {
    pub received: u64,
    pub principal: u64,
    pub extra: u64,
}

/// Recalls `want` USDC from the venue into the vault: brings the venue's index to now, redeems the receipts for
/// `want` (rounded up by one receipt so the vault is never short by rounding), books what arrives off
/// `reserve_placed` first and any remainder into LP capital as realised yield. When the last receipt is redeemed,
/// any unit `reserve_placed` still carries from placement rounding is written off against LP capital, so the
/// pool's books match its assets exactly. A recall is never refused by the pause or the interval.
pub fn recall_core<'info>(
    a: &RecallAccounts<'_, 'info>,
    pool: &mut Account<'info, Pool>,
    reserve: &mut PoolReserve,
    vault: &mut InterfaceAccount<'info, TokenAccount>,
    seeds: &[&[u8]],
    want: u64,
) -> Result<Recalled> {
    require!(pool.reserve_active == 1, BrinkError::ReserveInactive);
    brink_venue::cpi::touch(CpiContext::new(
        a.venue_program.key(),
        brink_venue::cpi::accounts::Touch {
            venue: a.venue.clone(),
            benchmark: a.venue_benchmark.clone(),
            event_authority: a.venue_event_authority.clone(),
            program: a.venue_program.clone(),
        },
    ))?;
    let index = {
        let data = a.venue.try_borrow_data()?;
        Venue::try_deserialize(&mut &data[..])?.index_e18
    };
    let want = want.min(pool.reserve_placed);
    let held = token_amount(a.receipts)?;
    if want == 0 || held == 0 {
        return Ok(Recalled::default());
    }
    let burn = brink_venue::receipts_for(want, index)?
        .saturating_add(1)
        .min(held);
    let before = vault.amount;
    brink_venue::cpi::redeem(
        CpiContext::new_with_signer(
            a.venue_program.key(),
            brink_venue::cpi::accounts::Redeem {
                venue: a.venue.clone(),
                benchmark: a.venue_benchmark.clone(),
                owner: pool.to_account_info(),
                owner_usdc: vault.to_account_info(),
                owner_receipts: a.receipts.clone(),
                reserve: a.venue_reserve.clone(),
                receipt_mint: a.receipt_mint.clone(),
                usdc_mint: a.usdc_mint.clone(),
                token_program: a.token_program.clone(),
                event_authority: a.venue_event_authority.clone(),
                program: a.venue_program.clone(),
            },
            &[seeds],
        ),
        burn,
        0,
    )?;
    vault.reload()?;
    let received = vault
        .amount
        .checked_sub(before)
        .ok_or(BrinkError::Overflow)?;
    let principal = received.min(pool.reserve_placed);
    let extra = received
        .checked_sub(principal)
        .ok_or(BrinkError::Overflow)?;
    pool.reserve_placed = pool
        .reserve_placed
        .checked_sub(principal)
        .ok_or(BrinkError::Overflow)?;
    pool.tvl = pool.tvl.checked_add(extra).ok_or(BrinkError::Overflow)?;
    if token_amount(a.receipts)? == 0 && pool.reserve_placed > 0 {
        // Nothing left at the venue: the units placement rounding left on the books are written off.
        let dust = pool.reserve_placed;
        pool.reserve_placed = 0;
        pool.tvl = pool.tvl.saturating_sub(dust);
    }
    reserve.recalled_lifetime = reserve
        .recalled_lifetime
        .checked_add(principal)
        .ok_or(BrinkError::Overflow)?;
    reserve.yield_realised = reserve
        .yield_realised
        .checked_add(extra)
        .ok_or(BrinkError::Overflow)?;
    Ok(Recalled {
        received,
        principal,
        extra,
    })
}

/// The amount of an SPL token account from its raw bytes.
fn token_amount(info: &AccountInfo) -> Result<u64> {
    let data = info.try_borrow_data()?;
    let bytes: [u8; 8] = data
        .get(64..72)
        .and_then(|b| b.try_into().ok())
        .ok_or(BrinkError::ReserveVenue)?;
    Ok(u64::from_le_bytes(bytes))
}

/// Inline recall for a payout path, from the remaining-account set. Writes the reserve account back.
pub fn recall_inline<'info>(
    r: &mut ReserveRemaining<'info>,
    pool: &mut Account<'info, Pool>,
    vault: &mut InterfaceAccount<'info, TokenAccount>,
    usdc_mint: &AccountInfo<'info>,
    token_program: &AccountInfo<'info>,
    seeds: &[&[u8]],
    want: u64,
) -> Result<Recalled> {
    let a = RecallAccounts {
        receipts: r.receipts,
        venue: r.venue,
        venue_benchmark: r.venue_benchmark,
        venue_reserve: r.venue_reserve,
        receipt_mint: r.receipt_mint,
        venue_event_authority: r.venue_event_authority,
        venue_program: r.venue_program,
        usdc_mint,
        token_program,
    };
    let moved = recall_core(&a, pool, &mut r.reserve, vault, seeds, want)?;
    if moved.received > 0 {
        r.reserve.rebalances = r.reserve.rebalances.saturating_add(1);
    }
    r.reserve.exit(&crate::ID)?;
    Ok(moved)
}

/// Realises the pool's accrued venue yield into LP capital inside an LP instruction, without a bounty: the
/// receipts worth the accrual at the venue's index are burnt and what the venue pays for them is booked as
/// yield, so the receipts that remain still back `reserve_placed`. An exit priced on yield not yet booked calls
/// this first, so `tvl` carries what the exit takes (review of external scan 2, A-2). Returns the USDC booked.
pub fn harvest_inline<'info>(
    r: &mut ReserveRemaining<'info>,
    pool: &mut Account<'info, Pool>,
    vault: &mut InterfaceAccount<'info, TokenAccount>,
    usdc_mint: &AccountInfo<'info>,
    token_program: &AccountInfo<'info>,
    seeds: &[&[u8]],
    now: i64,
) -> Result<u64> {
    require!(pool.reserve_active == 1, BrinkError::ReserveInactive);
    brink_venue::cpi::touch(CpiContext::new(
        r.venue_program.key(),
        brink_venue::cpi::accounts::Touch {
            venue: r.venue.clone(),
            benchmark: r.venue_benchmark.clone(),
            event_authority: r.venue_event_authority.clone(),
            program: r.venue_program.clone(),
        },
    ))?;
    let venue = {
        let data = r.venue.try_borrow_data()?;
        Venue::try_deserialize(&mut &data[..])?
    };
    require!(venue.last_ts >= now, BrinkError::VenueNeedsTouch);
    let index = venue.index_e18;
    let held = token_amount(r.receipts)?;
    let accrued = brink_venue::amount_for(held, index)?.saturating_sub(pool.reserve_placed);
    let burn = brink_venue::receipts_for(accrued, index)?.min(held);
    if burn == 0 {
        return Ok(0);
    }
    let before = vault.amount;
    brink_venue::cpi::redeem(
        CpiContext::new_with_signer(
            r.venue_program.key(),
            brink_venue::cpi::accounts::Redeem {
                venue: r.venue.clone(),
                benchmark: r.venue_benchmark.clone(),
                owner: pool.to_account_info(),
                owner_usdc: vault.to_account_info(),
                owner_receipts: r.receipts.clone(),
                reserve: r.venue_reserve.clone(),
                receipt_mint: r.receipt_mint.clone(),
                usdc_mint: usdc_mint.clone(),
                token_program: token_program.clone(),
                event_authority: r.venue_event_authority.clone(),
                program: r.venue_program.clone(),
            },
            &[seeds],
        ),
        burn,
        0,
    )?;
    vault.reload()?;
    let received = vault
        .amount
        .checked_sub(before)
        .ok_or(BrinkError::Overflow)?;
    pool.tvl = pool.tvl.checked_add(received).ok_or(BrinkError::Overflow)?;
    r.reserve.yield_realised = r
        .reserve
        .yield_realised
        .checked_add(received)
        .ok_or(BrinkError::Overflow)?;
    if received > 0 {
        r.reserve.rebalances = r.reserve.rebalances.saturating_add(1);
    }
    r.reserve.exit(&crate::ID)?;
    Ok(received)
}

#[event]
pub struct ReserveEnabled {
    pub pool: Pubkey,
    pub reserve: Pubkey,
    pub venue: Pubkey,
    pub params: ReserveParams,
    pub seq: u64,
}

#[event]
pub struct ReserveConfigured {
    pub pool: Pubkey,
    pub params: ReserveParams,
    pub paused: bool,
    pub seq: u64,
}

#[event]
pub struct ReserveRebalanced {
    pub pool: Pubkey,
    pub cranker: Pubkey,
    pub harvested: u64,
    pub bounty: u64,
    pub placed: u64,
    pub recalled: u64,
    pub reserve_placed: u64,
    pub working: u64,
    pub tvl: u64,
    pub index_e18: u128,
    pub seq: u64,
}
