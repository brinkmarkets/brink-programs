//! Swap lifecycle: open → (cancel | settle | liquidate). Every open re-quotes in the current slot with the
//! effective calibration, enforces the caller's limit rate, calls the pool's hooks, moves collateral and the
//! opening fee, and asserts invariants including vault conservation.
use super::{
    fees,
    hooks::{self, HookPayload, Point},
    math::*,
};
use crate::{errors::BrinkError, state::*};
use anchor_lang::prelude::*;
use anchor_spl::token_interface::{
    transfer_checked, Mint, TokenAccount, TokenInterface, TransferChecked,
};
use brink_index::Benchmark;

/// Index program that publishes benchmark accounts.
pub const INDEX_PROGRAM_ID: Pubkey = brink_index::ID;
/// Canonical USDC mints. Devnet first; mainnet after audits.
pub const USDC_MAINNET: Pubkey = pubkey!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
pub const USDC_DEVNET: Pubkey = pubkey!("4zMMC9srt5Ri5X14GAgXhaHii3GnPAEERYPJgZJDncDU");
/// Local test mint (LiteSVM / surfpool). Compiled in only for tests.
#[cfg(feature = "test-mint")]
pub const USDC_TEST: Pubkey = Pubkey::new_from_array([7u8; 32]);
pub fn is_usdc_mint(k: &Pubkey) -> bool {
    #[cfg(feature = "test-mint")]
    if *k == USDC_TEST {
        return true;
    }
    *k == USDC_MAINNET || *k == USDC_DEVNET
}
/// Mark-to-curve loss (bp of collateral) at which a swap becomes liquidatable.
pub const LIQUIDATION_LOSS_BP: u64 = 9_900;
/// Crank bounty for permissionless settle and liquidate: 2 bp of notional, capped, paid from the position's own
/// payout to whoever lands the instruction. A trader who closes their own matured position pays no bounty.
pub const CRANK_BOUNTY_BP: u32 = 2;
/// Bounty cap in USDC base units (25 USDC at six decimals).
pub const CRANK_BOUNTY_CAP: u64 = 25_000_000;

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy)]
pub struct OpenSwapArgs {
    pub leg: LegKind,
    pub tenor: u8,
    pub notional: u64,
    pub limit_rate_bp: u16,
    pub client_seed: u64,
}

/// Applies a queued calibration once its slot has passed. Idempotent.
pub fn apply_pending(pool: &mut Pool, slot: u64) {
    if pool.pending_effective_slot != 0 && slot >= pool.pending_effective_slot {
        pool.params = pool.pending_params;
        pool.pending_effective_slot = 0;
    }
}

/// Reads the benchmark for quoting. New exposure is priced only on a `Fresh` benchmark (`Benchmark::tier`,
/// ADR-010): published, within `max_staleness_slots`, and neither clamped by the band or drift bound nor
/// disputed by the publisher quorum. The index program no longer rejects out-of-band values, it clamps and
/// flags them, so the AMM reads the flag rather than recomputing the band. Exits never call this.
pub(crate) fn benchmark_for_quote(b: &Benchmark, now_slot: u64) -> Result<(u16, u16)> {
    require!(b.published, BrinkError::BenchmarkNotPublished);
    match b.tier(now_slot) {
        brink_index::Tier::Fresh => Ok((b.value_bp, b.ema_bp)),
        brink_index::Tier::Degraded if b.clamped || b.disputed => {
            Err(BrinkError::BenchmarkOutOfBand.into())
        }
        _ => Err(BrinkError::BenchmarkStale.into()),
    }
}

/// Reads the benchmark for an exit mark (`cancel`, `liquidate`). Exits are priced on a `Fresh` or `Degraded`
/// benchmark, clamped or disputed included: a trader must be able to leave, and a liquidation must be able to
/// run, while the publisher set is merely degraded (external scan 1, M-14). Only a `Stale` or unpublished
/// benchmark refuses, and then `settle` at maturity remains available whatever the tier.
fn benchmark_for_exit(b: &Benchmark, now_slot: u64) -> Result<(u16, u16)> {
    require!(b.published, BrinkError::BenchmarkNotPublished);
    match b.tier(now_slot) {
        brink_index::Tier::Fresh | brink_index::Tier::Degraded => Ok((b.value_bp, b.ema_bp)),
        brink_index::Tier::Stale => Err(BrinkError::BenchmarkStale.into()),
    }
}

/// Oldest point of benchmark history still held on chain: the oldest daily fixing in the ring, or the start of
/// the latest segment when no fixing has been recorded. Used by `settle` when a maturity has fallen out of the
/// ring (M-3).
pub fn oldest_known(b: &Benchmark) -> Result<(i64, u128)> {
    if b.fixing_first_day != 0 {
        let oldest_day = b.fixing_first_day.max(
            b.fixing_head_day
                .saturating_sub(brink_index::FIXING_DAYS.saturating_sub(1)),
        );
        if let Some(f) = b.fixing(oldest_day) {
            return Ok((Benchmark::day_start(oldest_day)?, f));
        }
    }
    let seg_len = b
        .unix_ts
        .checked_sub(b.prev_unix_ts)
        .ok_or(BrinkError::Overflow)?
        .max(0);
    let seg_start = b
        .accrual_e18
        .checked_sub(rate_over(b.prev_value_bp, seg_len)?)
        .ok_or(BrinkError::Overflow)?;
    Ok((b.prev_unix_ts, seg_start))
}

/// Accrual at maturity for a swap whose maturity fixing has been evicted from the ring (M-3): the average rate the
/// benchmark realised from the swap's open to the oldest point still on chain, applied over the swap's term.
/// The estimate is drawn from history that contains the whole term, never extrapolated backwards from the
/// latest segment, so a later publication cannot fabricate a gain or push the maturity accrual below the opening
/// accrual: the close is never refused. Flagged `Fallback` in `SwapClosed`.
pub fn fallback_maturity_accrual(
    b: &Benchmark,
    opened_ts: i64,
    matures_ts: i64,
    accrual_start: u128,
) -> Result<u128> {
    let (oldest_ts, oldest_accrual) = oldest_known(b)?;
    let span = oldest_ts
        .checked_sub(opened_ts)
        .ok_or(BrinkError::Overflow)?;
    let term = matures_ts
        .checked_sub(opened_ts)
        .ok_or(BrinkError::Overflow)?;
    if span <= 0 || term <= 0 {
        return Ok(accrual_start);
    }
    let realised = oldest_accrual.saturating_sub(accrual_start);
    let scaled = realised
        .checked_mul(u128::try_from(term.min(span)).map_err(|_| BrinkError::Overflow)?)
        .ok_or(BrinkError::Overflow)?
        .checked_div(u128::try_from(span).map_err(|_| BrinkError::Overflow)?)
        .ok_or(BrinkError::Overflow)?;
    accrual_start
        .checked_add(scaled)
        .ok_or(BrinkError::Overflow.into())
}

/// How the cumulative accrual at an instant was obtained (ADR-017 item 1.2). Emitted in `SwapClosed.fixing_kind`
/// so indexers can count settlements that used anything other than an exact reading.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum FixingKind {
    /// At or after the latest publish: last value held flat (exact between publications).
    Live = 0,
    /// Inside the latest segment: that segment's value removed pro rata (exact; maths patch 0002).
    Segment = 1,
    /// A midnight whose daily fixing is in the benchmark's ring (exact).
    Fixing = 2,
    /// Inside a day whose fixing is in the ring: linear between the day's fixing and the next known point (exact
    /// when the rate was flat over the day; otherwise deterministic, bounded by the intra-day rate change).
    Interpolated = 3,
    /// Older than the ring: the earliest known point extrapolated backwards flat at the previous value
    /// (deterministic given the state; flagged).
    Fallback = 4,
}

fn rate_over(bp: u16, secs: i64) -> Result<u128> {
    u128::from(bp)
        .checked_mul(u128::try_from(secs).map_err(|_| BrinkError::Overflow)?)
        .and_then(|x| x.checked_mul(brink_index::ACCRUAL_SCALE))
        .ok_or(BrinkError::Overflow.into())
}

/// Cumulative accrual at `t`, total over every `t` not before 1970: a settlement is never refused for lack of
/// history (audit F-34, review F-12, maths M-2, simulation S-3). Forward of the last publish the last value is
/// held flat; inside the latest segment the segment's value is removed pro rata; before that the daily fixings
/// ring written by `publish` answers (ADR-006 as amended by ADR-017).
pub fn accrual_lookup(b: &Benchmark, t: i64) -> Result<(u128, FixingKind)> {
    if t >= b.unix_ts {
        let a = b
            .accrual_e18
            .checked_add(rate_over(
                b.value_bp,
                t.checked_sub(b.unix_ts).ok_or(BrinkError::Overflow)?,
            )?)
            .ok_or(BrinkError::Overflow)?;
        return Ok((a, FixingKind::Live));
    }
    // Accrual at the start of the latest segment.
    let seg_len = b
        .unix_ts
        .checked_sub(b.prev_unix_ts)
        .ok_or(BrinkError::Overflow)?
        .max(0);
    let seg_start = b
        .accrual_e18
        .checked_sub(rate_over(b.prev_value_bp, seg_len)?)
        .ok_or(BrinkError::Overflow)?;
    if t >= b.prev_unix_ts {
        let a = seg_start
            .checked_add(rate_over(
                b.prev_value_bp,
                t.checked_sub(b.prev_unix_ts).ok_or(BrinkError::Overflow)?,
            )?)
            .ok_or(BrinkError::Overflow)?;
        return Ok((a, FixingKind::Segment));
    }
    // Before the latest segment: daily fixings.
    let day = Benchmark::day_of(t)?;
    let day_start = Benchmark::day_start(day)?;
    let next_start = day_start
        .checked_add(brink_index::SECONDS_PER_DAY)
        .ok_or(BrinkError::Overflow)?;
    // Nearest known point after `t`: the next day's fixing when the ring holds it, else the segment start.
    let right = match b.fixing(day.checked_add(1).ok_or(BrinkError::Overflow)?) {
        Some(f) if next_start <= b.prev_unix_ts => (next_start, f),
        _ => (b.prev_unix_ts, seg_start),
    };
    match b.fixing(day) {
        Some(f0) if t == day_start => Ok((f0, FixingKind::Fixing)),
        Some(f0) if right.1 >= f0 => {
            let span = u128::try_from(right.0.checked_sub(day_start).ok_or(BrinkError::Overflow)?)
                .map_err(|_| BrinkError::Overflow)?;
            let frac = u128::try_from(t.checked_sub(day_start).ok_or(BrinkError::Overflow)?)
                .map_err(|_| BrinkError::Overflow)?;
            let delta = right
                .1
                .checked_sub(f0)
                .ok_or(BrinkError::Overflow)?
                .checked_mul(frac)
                .ok_or(BrinkError::Overflow)?
                .checked_div(span)
                .ok_or(BrinkError::Overflow)?;
            Ok((
                f0.checked_add(delta).ok_or(BrinkError::Overflow)?,
                FixingKind::Interpolated,
            ))
        }
        _ => {
            let back = rate_over(
                b.prev_value_bp,
                right.0.checked_sub(t).ok_or(BrinkError::Overflow)?,
            )?;
            Ok((right.1.saturating_sub(back), FixingKind::Fallback))
        }
    }
}

/// Cumulative accrual at `now`; see `accrual_lookup`.
pub fn accrual_at(b: &Benchmark, now: i64) -> Result<u128> {
    accrual_lookup(b, now).map(|(a, _)| a)
}

/// Average floating rate (bp) between two accrual readings.
pub fn average_bp(accrual_start: u128, accrual_end: u128, seconds: i64) -> Result<i64> {
    require!(seconds > 0, BrinkError::NotMatured);
    let secs = u128::try_from(seconds).map_err(|_| BrinkError::Overflow)?;
    let num = accrual_end
        .checked_sub(accrual_start)
        .ok_or(BrinkError::Overflow)?;
    let avg = num
        .checked_div(
            brink_index::ACCRUAL_SCALE
                .checked_mul(secs)
                .ok_or(BrinkError::Overflow)?,
        )
        .ok_or(BrinkError::Overflow)?;
    i64::try_from(avg).map_err(|_| BrinkError::Overflow.into())
}

fn quote_fixed(
    pool: &Pool,
    b: &Benchmark,
    slot: u64,
    tenor: vernier::Tenor,
    leg: vernier::Leg,
    notional: u64,
) -> Result<i32> {
    let (spot, ema) = benchmark_for_quote(b, slot)?;
    quote_with(pool, spot, ema, tenor, leg, notional)
}

fn quote_with(
    pool: &Pool,
    spot: u16,
    ema: u16,
    tenor: vernier::Tenor,
    leg: vernier::Leg,
    notional: u64,
) -> Result<i32> {
    quote_with_start(pool, spot, ema, 0, tenor, leg, notional)
}

/// The quote for a swap starting `start_days` ahead: the spot quote when zero, the forward-curve quote otherwise
/// (`vernier::quote_forward`, which only replaces the model term).
fn quote_with_start(
    pool: &Pool,
    spot: u16,
    ema: u16,
    start_days: u16,
    tenor: vernier::Tenor,
    leg: vernier::Leg,
    notional: u64,
) -> Result<i32> {
    let params: vernier::Params = pool.params.into();
    match pool.pricer {
        Pricer::Vernier => {
            let q = if start_days == 0 {
                vernier::quote(spot, ema, tenor, leg, notional, &pool.vernier_pool(), &params)
            } else {
                vernier::quote_forward(
                    spot,
                    ema,
                    start_days,
                    tenor,
                    leg,
                    notional,
                    &pool.vernier_pool(),
                    &params,
                )
            };
            Ok(q.map_err(BrinkError::from)?.fixed_bp)
        }
        Pricer::External { .. } => Err(BrinkError::PricerMismatch.into()),
    }
}

/// Whole days from `now` to a forward's start, rounded up; zero once the start has passed. The unwind of a
/// forward before its start is priced on the curve at this offset.
pub fn days_to_start(now: i64, start_ts: i64) -> Result<u16> {
    let secs = start_ts.checked_sub(now).ok_or(BrinkError::Overflow)?;
    if secs <= 0 {
        return Ok(0);
    }
    let days = secs
        .checked_add(SECONDS_PER_DAY - 1)
        .ok_or(BrinkError::Overflow)?
        .checked_div(SECONDS_PER_DAY)
        .ok_or(BrinkError::Overflow)?;
    u16::try_from(days).map_err(|_| BrinkError::Overflow.into())
}

/// The unwind rate for an exit mark. `Mid` carries no demand term, so the figure cannot be moved by another
/// trader opening and cancelling exposure in the same pool (external scan 1, L-22); it is used for the
/// liquidation trigger and the liquidation and cap-out price. `Quoted` is the full opposite-leg quote the trader
/// would actually trade at, used for a voluntary early cancel.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Unwind {
    Quoted,
    Mid,
}

fn unwind_rate(
    pool: &Pool,
    b: &Benchmark,
    slot: u64,
    start_days: u16,
    tenor: vernier::Tenor,
    leg: vernier::Leg,
    notional: u64,
    how: Unwind,
) -> Result<i32> {
    let (spot, ema) = benchmark_for_exit(b, slot)?;
    match how {
        Unwind::Quoted => quote_with_start(pool, spot, ema, start_days, tenor, leg, notional),
        Unwind::Mid => match pool.pricer {
            Pricer::Vernier => {
                let params: vernier::Params = pool.params.into();
                let mid = if start_days == 0 {
                    vernier::quote_mid(spot, ema, tenor, leg, &params)
                } else {
                    vernier::quote_mid_forward(spot, ema, start_days, tenor, leg, &params)
                };
                Ok(mid.map_err(BrinkError::from)?)
            }
            Pricer::External { .. } => Err(BrinkError::PricerMismatch.into()),
        },
    }
}

/// Event CPI for handlers that do not hold the `ctx` the `emit_cpi!` macro expects (the shared leg functions
/// below, which serve both the single-swap and the basis instructions). Same self-CPI, same signer seeds, same
/// bytes on the wire as the macro.
pub(crate) fn emit_event<E: anchor_lang::Event>(authority: &AccountInfo, e: &E) -> Result<()> {
    use anchor_lang::solana_program::{
        instruction::{AccountMeta, Instruction},
        program::invoke_signed,
    };
    let inner = anchor_lang::Event::data(e);
    let ix_data: Vec<u8> = anchor_lang::event::EVENT_IX_TAG_LE
        .iter()
        .copied()
        .chain(inner)
        .collect();
    let ix = Instruction::new_with_bytes(
        crate::ID,
        &ix_data,
        vec![AccountMeta::new_readonly(*authority.key, true)],
    );
    invoke_signed(
        &ix,
        std::slice::from_ref(authority),
        &[&[b"__event_authority", &[crate::EVENT_AUTHORITY_AND_BUMP.1]]],
    )
    .map_err(Error::from)
}

#[event_cpi]
#[derive(Accounts)]
#[instruction(args: OpenSwapArgs)]
pub struct TraderOpenSwap<'info> {
    /// Read-only: fees accrue on the pool, so the shared account takes no write lock (review F-29, ADR-001).
    #[account(seeds = [b"global"], bump = global.bump, has_one = usdc_mint, has_one = token_program)]
    pub global: Box<Account<'info, Global>>,
    #[account(mut, seeds = [b"pool", pool.benchmark.as_ref()], bump = pool.bump, has_one = benchmark, has_one = vault)]
    pub pool: Box<Account<'info, Pool>>,
    /// Owner check is implicit: `Account<Benchmark>` requires the index program as owner.
    pub benchmark: Box<Account<'info, Benchmark>>,
    #[account(init, payer = trader, space = 8 + Swap::INIT_SPACE, seeds = [b"swap", pool.key().as_ref(), trader.key().as_ref(), &args.client_seed.to_le_bytes()], bump)]
    pub swap: Box<Account<'info, Swap>>,
    #[account(mut)]
    pub trader: Signer<'info>,
    #[account(mut, constraint = trader_usdc.owner == trader.key() @ BrinkError::TokenOwner, constraint = trader_usdc.mint == usdc_mint.key() @ BrinkError::SettlementMint)]
    pub trader_usdc: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut)]
    pub vault: Box<InterfaceAccount<'info, TokenAccount>>,
    pub usdc_mint: Box<InterfaceAccount<'info, Mint>>,
    /// CHECK: validated against pool.hook_program in `hooks::call`.
    pub hook_program: Option<UncheckedAccount<'info>>,
    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

/// The accounts an open shares across its legs: one trader, one settlement account, one event authority.
pub(crate) struct OpenShared<'a, 'info> {
    pub global: &'a Account<'info, Global>,
    pub trader: &'a Signer<'info>,
    pub trader_usdc: &'a InterfaceAccount<'info, TokenAccount>,
    pub usdc_mint: &'a InterfaceAccount<'info, Mint>,
    pub token_program: &'a Interface<'info, TokenInterface>,
    pub event_authority: AccountInfo<'info>,
}

/// The per-pool accounts one leg of an open writes.
pub(crate) struct OpenLeg<'a, 'info> {
    pub pool: &'a mut Account<'info, Pool>,
    pub benchmark: &'a Account<'info, Benchmark>,
    pub swap: &'a mut Account<'info, Swap>,
    pub swap_bump: u8,
    pub vault: &'a mut InterfaceAccount<'info, TokenAccount>,
    pub hook_program: Option<&'a UncheckedAccount<'info>>,
}

/// How a leg is priced: the ordinary quote against its own pool in this slot, or a fixed rate the caller has
/// already quoted (a basis leg, whose rate carries the pair's correlation offset). The limit check applies
/// either way.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Pricing {
    Quote,
    Given(i32),
    /// A forward-starting swap: quoted on the Vernier forward curve for a start this many days
    /// ahead; the leg records `start_ts` and books the forward terms until its start crank.
    Forward(u16),
}

/// The link one leg of a basis swap records: the other leg's key and the flags for this leg.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct Link {
    pub other: Pubkey,
    pub flags: u8,
}

/// What one leg's open settled on, for the caller's event.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Opened {
    pub fixed_bp: u16,
    pub collateral: u64,
}

/// Opens one leg: the whole of what `trader_open_swap` has always done, in the same order, factored so the
/// basis instruction can run it once per pool. Mode gates, the Limited-mode budget, the pending calibration,
/// the notional bounds, the `BeforeOpen` hook, capital, capacity, queue priority, the quote and the limit rate,
/// collateral plus the opening fee in one transfer, the book, the ladder, the events and the `AfterOpen` hook.
/// The caller asserts the pool invariants once the vault has been reloaded.
pub(crate) fn open_leg<'info>(
    sh: &OpenShared<'_, 'info>,
    leg: &mut OpenLeg<'_, 'info>,
    a: &OpenSwapArgs,
    clock: &Clock,
    pricing: Pricing,
    link: Option<Link>,
) -> Result<Opened> {
    let g = sh.global;
    match g.mode {
        OperatingMode::Halted => return Err(BrinkError::Halted.into()),
        OperatingMode::WithdrawOnly => return Err(BrinkError::WithdrawOnly.into()),
        OperatingMode::Limited => {
            // Per swap and per pool per hour: a cap that only applied per swap was split around (S-2).
            require!(a.notional <= g.limited_mode_cap, BrinkError::LimitedModeCap);
        }
        OperatingMode::Normal => {}
    }
    let limited_window_start = if g.mode == OperatingMode::Limited {
        leg.pool
            .charge_limited_budget(clock.unix_timestamp, a.notional, g.limited_mode_cap)?
    } else {
        0
    };
    apply_pending(leg.pool, clock.slot);
    require!(
        a.notional >= leg.pool.min_notional,
        BrinkError::NotionalTooSmall
    );
    require!(
        a.notional <= leg.pool.max_notional,
        BrinkError::NotionalTooLarge
    );
    let payload = HookPayload {
        pool: leg.pool.key(),
        actor: sh.trader.key(),
        amount: 0,
        notional: a.notional,
        leg: Some(a.leg),
        tenor: a.tenor,
    };
    hooks::call(
        leg.pool,
        g.mode,
        leg.hook_program,
        Point::BeforeOpen,
        &payload,
        &[],
    )?;

    let tenor = tenor_from(a.tenor)?;
    let vleg = leg_from(a.leg);
    let pool = &*leg.pool;
    require!(pool.tvl > 0, BrinkError::PoolInvariant);
    // Capacity before price (maths finding M-14): a trade the caps refuse is reported as `LegCap`, whatever
    // the pricing library would have said about it.
    require!(
        a.notional <= vernier::leg_capacity(&pool.vernier_pool(), vleg),
        BrinkError::LegCap
    );
    // An eligible withdrawal epoch has first call on the capacity above the caps' capital floor: a new swap may
    // only take what would remain after the queue is served, so opens cannot keep an epoch unserviceable (M-18).
    super::queue::require_queue_priority(pool, vleg, a.notional, clock.slot)?;
    let forward_days = match pricing {
        Pricing::Forward(d) => {
            // The whole life must sit inside the quoted curve and the maturity ladder.
            require!(
                d.checked_add(tenor.days()).is_some_and(|e| e <= vernier::FORWARD_HORIZON_DAYS),
                BrinkError::ForwardHorizon
            );
            d
        }
        Pricing::Quote | Pricing::Given(_) => 0,
    };
    let fixed_i = match pricing {
        Pricing::Quote => quote_fixed(pool, leg.benchmark, clock.slot, tenor, vleg, a.notional)?,
        Pricing::Forward(d) => {
            let (spot, ema) = benchmark_for_quote(leg.benchmark, clock.slot)?;
            quote_with_start(pool, spot, ema, d, tenor, vleg, a.notional)?
        }
        Pricing::Given(f) => f,
    };
    // Maths finding M-7: a negative receive quote is a model outcome at very low rates, reported as such.
    require!(fixed_i >= 0, BrinkError::QuoteBelowZero);
    let fixed = u16::try_from(fixed_i).map_err(|_| BrinkError::QuoteCeiling)?;
    match vleg {
        vernier::Leg::Pay => require!(fixed <= a.limit_rate_bp, BrinkError::LimitRate),
        vernier::Leg::Receive => require!(fixed >= a.limit_rate_bp, BrinkError::LimitRate),
    }
    let params: vernier::Params = pool.params.into();
    let collateral = vernier::collateral(a.notional, tenor, &params);
    let fee = mul_bp(a.notional, vernier::OPENING_FEE_BP)?;

    // Money first: collateral plus the opening fee to the pool vault in one transfer from the trader. The fee
    // is booked on the pool and swept later (review F-29).
    let decimals = sh.usdc_mint.decimals;
    transfer_checked(
        CpiContext::new(
            sh.token_program.key(),
            TransferChecked {
                from: sh.trader_usdc.to_account_info(),
                mint: sh.usdc_mint.to_account_info(),
                to: leg.vault.to_account_info(),
                authority: sh.trader.to_account_info(),
            },
        ),
        collateral.checked_add(fee).ok_or(BrinkError::Overflow)?,
        decimals,
    )?;

    let pool = &mut *leg.pool;
    match vleg {
        vernier::Leg::Pay => {
            pool.open_pay_notional = pool
                .open_pay_notional
                .checked_add(a.notional)
                .ok_or(BrinkError::Overflow)?
        }
        vernier::Leg::Receive => {
            pool.open_rec_notional = pool
                .open_rec_notional
                .checked_add(a.notional)
                .ok_or(BrinkError::Overflow)?
        }
    }
    pool.collateral_held = pool
        .collateral_held
        .checked_add(collateral)
        .ok_or(BrinkError::Overflow)?;
    match a.leg {
        LegKind::PayFixed => {
            pool.collateral_pay = pool
                .collateral_pay
                .checked_add(collateral)
                .ok_or(BrinkError::Overflow)?
        }
        LegKind::ReceiveFixed => {
            pool.collateral_rec = pool
                .collateral_rec
                .checked_add(collateral)
                .ok_or(BrinkError::Overflow)?
        }
    }
    pool.open_swaps = pool.open_swaps.checked_add(1).ok_or(BrinkError::Overflow)?;
    rebase_utilisation(pool)?;
    if fee > 0 {
        let (b, t) = fees::book(pool, fees::FeeKind::Opening, fee)?;
        emit_event(
            &sh.event_authority,
            &FeeCollected {
                pool: pool.key(),
                kind: fees::FeeKind::Opening,
                fee,
                buyback: b,
                treasury: t,
                seq: pool.event_seq,
            },
        )?;
    }

    let s = &mut *leg.swap;
    s.pool = pool.key();
    s.trader = sh.trader.key();
    s.leg = a.leg;
    s.tenor = a.tenor;
    s.notional = a.notional;
    s.fixed_bp = fixed;
    s.collateral = collateral;
    s.opened_slot = clock.slot;
    s.opened_ts = clock.unix_timestamp;
    // A forward's accrual starts at the UTC midnight `forward_days` ahead; a spot swap's at open.
    let accrual_from = if forward_days > 0 {
        align_up_to_day(
            clock
                .unix_timestamp
                .checked_add(
                    i64::from(forward_days)
                        .checked_mul(SECONDS_PER_DAY)
                        .ok_or(BrinkError::Overflow)?,
                )
                .ok_or(BrinkError::Overflow)?,
        )?
    } else {
        clock.unix_timestamp
    };
    // Maturity is rounded up to the next 00:00 UTC so that settlement reads an exact daily fixing whatever is
    // published afterwards (ADR-006 item 5, ADR-017). The actual term is `tenor.days()` plus the fraction of a
    // day to midnight and settlement pays the actual term.
    s.matures_ts = align_up_to_day(
        accrual_from
            .checked_add(
                i64::from(tenor.days())
                    .checked_mul(SECONDS_PER_DAY)
                    .ok_or(BrinkError::Overflow)?,
            )
            .ok_or(BrinkError::Overflow)?,
    )?;
    // A forward's start reading is taken by `crank_start_forward` once the start has passed; until then the
    // field is zero and the book carries the forward terms.
    s.index_accrual_start = if forward_days > 0 {
        0
    } else {
        accrual_at(leg.benchmark, clock.unix_timestamp)?
    };
    s.start_ts = if forward_days > 0 { accrual_from } else { 0 };
    s.client_seed = a.client_seed;
    s.state = SwapState::Open;
    s.bump = leg.swap_bump;
    s.limited_window_start = limited_window_start;
    match link {
        Some(l) => {
            s.link = l.other;
            s.link_flags = l.flags;
        }
        None => {
            s.link = Pubkey::default();
            s.link_flags = if forward_days > 0 { LINK_FORWARD } else { 0 };
        }
    }
    pool.book_add(&s.book_terms()?)?;
    pool.ladder_add(s.matures_ts)?;

    pool.event_seq = pool.event_seq.checked_add(1).ok_or(BrinkError::Sequence)?;
    emit_event(
        &sh.event_authority,
        &SwapOpened {
            pool: pool.key(),
            swap: s.key(),
            trader: s.trader,
            leg: a.leg,
            tenor: a.tenor,
            notional: a.notional,
            fixed_bp: fixed,
            collateral,
            seq: pool.event_seq,
        },
    )?;
    // The hook observes the booked position, so the swap's bytes are written before the CPI; Anchor would
    // otherwise serialise them only at instruction exit (external scan 1, M-19).
    leg.swap.exit(&crate::ID)?;
    let swap_info = leg.swap.to_account_info();
    hooks::call(
        leg.pool,
        sh.global.mode,
        leg.hook_program,
        Point::AfterOpen,
        &payload,
        &[swap_info],
    )?;
    Ok(Opened {
        fixed_bp: fixed,
        collateral,
    })
}

pub fn open(ctx: Context<TraderOpenSwap>, a: OpenSwapArgs) -> Result<()> {
    let clock = Clock::get()?;
    let swap_bump = ctx.bumps.swap;
    let x = &mut *ctx.accounts;
    let sh = OpenShared {
        global: &x.global,
        trader: &x.trader,
        trader_usdc: &x.trader_usdc,
        usdc_mint: &x.usdc_mint,
        token_program: &x.token_program,
        event_authority: x.event_authority.to_account_info(),
    };
    let mut leg = OpenLeg {
        pool: &mut x.pool,
        benchmark: &x.benchmark,
        swap: &mut x.swap,
        swap_bump,
        vault: &mut x.vault,
        hook_program: x.hook_program.as_ref(),
    };
    open_leg(&sh, &mut leg, &a, &clock, Pricing::Quote, None)?;
    leg.vault.reload()?;
    leg.pool.assert_invariants(leg.vault.amount)
}

/// Accounts shared by every close path. The swap closes to the trader (rent back); USDC goes to `trader_usdc`.
#[event_cpi]
#[derive(Accounts)]
pub struct CloseSwap<'info> {
    /// Read-only: fees accrue on the pool (review F-29, ADR-001).
    #[account(seeds = [b"global"], bump = global.bump, has_one = usdc_mint, has_one = token_program)]
    pub global: Box<Account<'info, Global>>,
    #[account(mut, seeds = [b"pool", pool.benchmark.as_ref()], bump = pool.bump, has_one = benchmark, has_one = vault)]
    pub pool: Box<Account<'info, Pool>>,
    pub benchmark: Box<Account<'info, Benchmark>>,
    #[account(mut, close = trader, has_one = pool, has_one = trader, constraint = swap.state == SwapState::Open @ BrinkError::AlreadySettled)]
    pub swap: Box<Account<'info, Swap>>,
    /// CHECK: rent destination and USDC owner; equality with `swap.trader` enforced by `has_one`.
    #[account(mut)]
    pub trader: UncheckedAccount<'info>,
    /// Trader for `cancel`; anyone for `settle` / `liquidate`.
    pub signer: Signer<'info>,
    #[account(mut, constraint = trader_usdc.owner == trader.key() @ BrinkError::TokenOwner, constraint = trader_usdc.mint == usdc_mint.key() @ BrinkError::SettlementMint)]
    pub trader_usdc: Box<InterfaceAccount<'info, TokenAccount>>,
    /// Where the crank bounty goes on `settle` / `liquidate`. Must belong to the signer. Omit it (or crank your own
    /// position) and no bounty is paid.
    #[account(mut, constraint = cranker_usdc.owner == signer.key() @ BrinkError::TokenOwner, constraint = cranker_usdc.mint == usdc_mint.key() @ BrinkError::SettlementMint)]
    pub cranker_usdc: Option<Box<InterfaceAccount<'info, TokenAccount>>>,
    #[account(mut)]
    pub vault: Box<InterfaceAccount<'info, TokenAccount>>,
    pub usdc_mint: Box<InterfaceAccount<'info, Mint>>,
    pub token_program: Interface<'info, TokenInterface>,
}

/// Bounty for a third-party crank: `CRANK_BOUNTY_BP` of notional, capped at `CRANK_BOUNTY_CAP`.
pub fn crank_bounty(notional: u64) -> Result<u64> {
    Ok(mul_bp(notional, CRANK_BOUNTY_BP)?.min(CRANK_BOUNTY_CAP))
}

/// The accounts a close shares across legs.
pub(crate) struct CloseShared<'a, 'info> {
    pub signer: Pubkey,
    pub trader_usdc: &'a InterfaceAccount<'info, TokenAccount>,
    pub cranker_usdc: Option<&'a InterfaceAccount<'info, TokenAccount>>,
    pub usdc_mint: &'a InterfaceAccount<'info, Mint>,
    pub token_program: &'a Interface<'info, TokenInterface>,
    pub event_authority: AccountInfo<'info>,
}

/// The per-pool accounts one leg of a close writes.
pub(crate) struct CloseLeg<'a, 'info> {
    pub pool: &'a mut Account<'info, Pool>,
    pub benchmark: &'a Account<'info, Benchmark>,
    pub swap: &'a mut Account<'info, Swap>,
    pub vault: &'a mut InterfaceAccount<'info, TokenAccount>,
}

/// Books a close: releases notional and collateral, applies pnl to LP capital, charges the income fee on gains,
/// pays the trader, and emits. `pnl` is already bounded to ±collateral; it is additionally bounded by LP capital.
/// `min_payout`, when given, is checked against the final USDC the trader receives (external scan 1, M-8).
/// Returns that payout. Ends by asserting conservation on the leg's pool.
pub(crate) fn close_leg<'info>(
    sh: &CloseShared<'_, 'info>,
    leg: &mut CloseLeg<'_, 'info>,
    pnl: i64,
    into: fn(i64) -> SwapState,
    bounty_eligible: bool,
    fixing_kind: FixingKind,
    min_payout: Option<u64>,
) -> Result<u64> {
    let pool = &mut *leg.pool;
    let s = &*leg.swap;
    let pnl = if pnl > 0 {
        pnl.min(i64::try_from(pool.tvl).unwrap_or(i64::MAX))
    } else {
        pnl
    };
    let gain = u64::try_from(pnl.max(0)).map_err(|_| BrinkError::Overflow)?;
    let loss = u64::try_from(pnl.min(0).checked_neg().ok_or(BrinkError::Overflow)?)
        .map_err(|_| BrinkError::Overflow)?;
    let fee = u64::try_from(
        u128::from(gain)
            .checked_mul(u128::from(vernier::INCOME_FEE_PCT))
            .ok_or(BrinkError::Overflow)?
            / 100,
    )
    .map_err(|_| BrinkError::Overflow)?;
    let gross = s
        .collateral
        .checked_add(gain)
        .ok_or(BrinkError::Overflow)?
        .checked_sub(loss)
        .ok_or(BrinkError::Overflow)?
        .checked_sub(fee)
        .ok_or(BrinkError::Overflow)?;
    // Bounty: only for a third-party crank that supplied a destination. Paid from the trader's payout first and,
    // when the position has nothing left to pay, from the collateral the LPs collect, so that a fully losing swap
    // is settled as promptly as a winning one (M-3, M-5). Bounded by what the position releases in total.
    let bounty = if bounty_eligible && sh.cranker_usdc.is_some() && sh.signer != s.trader {
        crank_bounty(s.notional)?.min(gross.checked_add(loss).ok_or(BrinkError::Overflow)?)
    } else {
        0
    };
    let bounty_from_payout = bounty.min(gross);
    let bounty_from_lps = bounty
        .checked_sub(bounty_from_payout)
        .ok_or(BrinkError::Overflow)?;
    let payout = gross
        .checked_sub(bounty_from_payout)
        .ok_or(BrinkError::Overflow)?;
    if let Some(min) = min_payout {
        require!(payout >= min, BrinkError::Slippage);
    }

    match s.leg {
        LegKind::PayFixed => {
            pool.open_pay_notional = pool
                .open_pay_notional
                .checked_sub(s.notional)
                .ok_or(BrinkError::Overflow)?
        }
        LegKind::ReceiveFixed => {
            pool.open_rec_notional = pool
                .open_rec_notional
                .checked_sub(s.notional)
                .ok_or(BrinkError::Overflow)?
        }
    }
    pool.collateral_held = pool
        .collateral_held
        .checked_sub(s.collateral)
        .ok_or(BrinkError::Overflow)?;
    match s.leg {
        LegKind::PayFixed => {
            pool.collateral_pay = pool
                .collateral_pay
                .checked_sub(s.collateral)
                .ok_or(BrinkError::Overflow)?
        }
        LegKind::ReceiveFixed => {
            pool.collateral_rec = pool
                .collateral_rec
                .checked_sub(s.collateral)
                .ok_or(BrinkError::Overflow)?
        }
    }
    pool.open_swaps = pool.open_swaps.checked_sub(1).ok_or(BrinkError::Overflow)?;
    pool.book_sub(&s.book_terms()?)?;
    pool.ladder_sub(s.matures_ts)?;
    pool.release_limited_budget(s.limited_window_start, s.notional);
    pool.tvl = pool
        .tvl
        .checked_add(loss)
        .ok_or(BrinkError::Overflow)?
        .checked_sub(gain)
        .ok_or(BrinkError::Overflow)?
        .checked_sub(bounty_from_lps)
        .ok_or(BrinkError::Overflow)?;
    rebase_utilisation(pool)?;

    let benchmark = pool.benchmark;
    let bump = pool.bump;
    let seeds: &[&[u8]] = &[b"pool", benchmark.as_ref(), &[bump]];
    let decimals = sh.usdc_mint.decimals;
    if payout > 0 {
        transfer_checked(
            CpiContext::new_with_signer(
                sh.token_program.key(),
                TransferChecked {
                    from: leg.vault.to_account_info(),
                    mint: sh.usdc_mint.to_account_info(),
                    to: sh.trader_usdc.to_account_info(),
                    authority: leg.pool.to_account_info(),
                },
                &[seeds],
            ),
            payout,
            decimals,
        )?;
    }
    if bounty > 0 {
        let to = sh
            .cranker_usdc
            .ok_or(BrinkError::TokenOwner)?
            .to_account_info();
        transfer_checked(
            CpiContext::new_with_signer(
                sh.token_program.key(),
                TransferChecked {
                    from: leg.vault.to_account_info(),
                    mint: sh.usdc_mint.to_account_info(),
                    to,
                    authority: leg.pool.to_account_info(),
                },
                &[seeds],
            ),
            bounty,
            decimals,
        )?;
    }
    if fee > 0 {
        // The income fee is retained in the vault and booked on the pool (review F-29).
        let pool = &mut *leg.pool;
        let (b, t) = fees::book(pool, fees::FeeKind::Income, fee)?;
        emit_event(
            &sh.event_authority,
            &FeeCollected {
                pool: pool.key(),
                kind: fees::FeeKind::Income,
                fee,
                buyback: b,
                treasury: t,
                seq: pool.event_seq,
            },
        )?;
    }
    let pool = &mut *leg.pool;
    let s = &mut *leg.swap;
    s.state = into(pnl);
    pool.event_seq = pool.event_seq.checked_add(1).ok_or(BrinkError::Sequence)?;
    emit_event(
        &sh.event_authority,
        &SwapClosed {
            pool: pool.key(),
            swap: s.key(),
            state: s.state,
            payout,
            bounty,
            seq: pool.event_seq,
            fixing_kind: fixing_kind as u8,
        },
    )?;
    // No hook on an exit (review F-30, ADR-003): closes are unconditional and observable through `SwapClosed`.
    leg.vault.reload()?;
    // Closes check conservation only: the caps are entry constraints, and a close that pays a gain must never be
    // refused because the remaining capital is thinner than the caps allow (maths finding M-4).
    leg.pool.assert_conservation(leg.vault.amount)?;
    Ok(payout)
}

/// `close_leg` on the single-swap accounts.
fn close(
    ctx: &mut Context<CloseSwap>,
    pnl: i64,
    into: fn(i64) -> SwapState,
    bounty_eligible: bool,
    fixing_kind: FixingKind,
    min_payout: Option<u64>,
) -> Result<()> {
    let x = &mut *ctx.accounts;
    let sh = CloseShared {
        signer: x.signer.key(),
        trader_usdc: &x.trader_usdc,
        cranker_usdc: x.cranker_usdc.as_deref(),
        usdc_mint: &x.usdc_mint,
        token_program: &x.token_program,
        event_authority: x.event_authority.to_account_info(),
    };
    let mut leg = CloseLeg {
        pool: &mut x.pool,
        benchmark: &x.benchmark,
        swap: &mut x.swap,
        vault: &mut x.vault,
    };
    close_leg(
        &sh,
        &mut leg,
        pnl,
        into,
        bounty_eligible,
        fixing_kind,
        min_payout,
    )
    .map(|_| ())
}

/// Early-close valuation: the floating-versus-fixed difference accrued since open (benchmark average over the
/// elapsed seconds against the fixed rate) plus the remaining seconds unwound at the opposite-leg quote. The two
/// legs are summed unbounded and then clamped to `[-bound, +bound]`; `bound` is `u64::MAX` for the liquidation
/// trigger (which needs the unclamped value) and the collateral otherwise. Review finding F-11 (ADR-005).
fn mark(
    pool: &Pool,
    b: &Benchmark,
    s: &Swap,
    slot: u64,
    now: i64,
    bound: u64,
    how: Unwind,
) -> Result<i64> {
    let tenor = tenor_from(s.tenor)?;
    let now = now.min(s.matures_ts);
    // A forward accrues from its start: before it nothing has accrued and the whole term is
    // unwound on the curve at the days still to run to the start.
    let from = s.accrual_from();
    let elapsed = now.checked_sub(from).ok_or(BrinkError::Overflow)?.max(0);
    let remaining = s
        .matures_ts
        .checked_sub(now.max(from))
        .ok_or(BrinkError::Overflow)?
        .max(0);
    let start_days = if s.is_forward() { days_to_start(now, s.start_ts)? } else { 0 };
    let accrued = if elapsed > 0 {
        // `now` is the chain clock, so this reading is always on the live path (exact between publications).
        // A forward whose start crank has not run yet reads its start from the fixings ring here.
        let start_accrual = if s.is_started() { s.index_accrual_start } else { accrual_at(b, from)? };
        pnl_from_accrual(
            s.leg,
            start_accrual,
            accrual_at(b, now)?,
            s.fixed_bp,
            s.notional,
            elapsed,
        )?
    } else {
        0
    };
    let forward = if remaining > 0 && pool.tvl > 0 {
        // The unwind quote is floored at zero so a very low rate never blocks an early close (M-7); the
        // accrued leg does not depend on the quote. With no LP capital there is no book to unwind into
        // (`vernier::quote` reports `EmptyPool`), so the remaining term is valued at par: nothing is owed
        // either way beyond what has accrued, and the close is never blocked (M-4).
        let unwind = unwind_rate(
            pool,
            b,
            slot,
            start_days,
            tenor,
            leg_from(opposite(s.leg)),
            s.notional,
            how,
        )?
        .max(0);
        pnl_secs(
            oriented_diff(s.leg, i64::from(unwind), i64::from(s.fixed_bp))?,
            s.notional,
            remaining,
        )?
    } else {
        0
    };
    clamp_to_collateral(
        accrued.checked_add(forward).ok_or(BrinkError::Overflow)?,
        bound,
    )
}

/// Trader cancels early at the then-current opposite quote. `min_payout` is the trader's slippage floor.
///
/// A leg of a basis swap is refused here (`LinkedLeg`): both legs close together through
/// `trader_cancel_basis_swap`, with one floor on the net payout. The one exception is an orphaned leg, whose
/// partner has already been closed by a permissionless crank (liquidated, capped or settled): the trader passes
/// the partner's key as the first remaining account, and if that account is closed (no data, system-owned) the
/// leg is cancelled on its own. A live partner, or no remaining account, keeps the refusal.
pub fn cancel(mut ctx: Context<CloseSwap>, min_payout: u64) -> Result<()> {
    if ctx.accounts.swap.is_linked() {
        let partner = ctx
            .remaining_accounts
            .first()
            .ok_or(BrinkError::LinkedLeg)?;
        require_keys_eq!(partner.key(), ctx.accounts.swap.link, BrinkError::LinkedLeg);
        require!(
            partner.data_is_empty() && *partner.owner == anchor_lang::system_program::ID,
            BrinkError::LinkedLeg
        );
    }
    let clock = Clock::get()?;
    // A matured swap has nothing left to unwind: cancel is settlement, with the same fixing, the same arithmetic
    // and the same event. The trader pays no bounty on their own position either way (simulation S-3).
    if clock.unix_timestamp >= ctx.accounts.swap.matures_ts {
        return settle_inner(ctx, Some(min_payout));
    }
    require!(
        ctx.accounts.global.mode != OperatingMode::Halted,
        BrinkError::Halted
    );
    require_keys_eq!(
        ctx.accounts.signer.key(),
        ctx.accounts.swap.trader,
        ErrorCode::ConstraintSigner
    );
    apply_pending(&mut ctx.accounts.pool, clock.slot);
    let pnl = mark(
        &ctx.accounts.pool,
        &ctx.accounts.benchmark,
        &ctx.accounts.swap,
        clock.slot,
        clock.unix_timestamp,
        ctx.accounts.swap.collateral,
        Unwind::Quoted,
    )?;
    // The floor is enforced on the final payout inside `close`, after the income fee and the LP-capital bound
    // (M-8); no bounty applies to a trader closing their own position.
    close(
        &mut ctx,
        pnl,
        |p| SwapState::Cancelled { pnl: p },
        false,
        FixingKind::Live,
        Some(min_payout),
    )
}

/// One leg of a trader's early close on the shared accounts: the valuation and booking `cancel` performs after
/// its account checks, with the matured case settling exactly as `cancel` does. The caller has checked the
/// mode and the signer. Returns the leg's payout; `min_payout` is `None` when the caller floors the net of
/// several legs.
pub(crate) fn cancel_leg<'info>(
    sh: &CloseShared<'_, 'info>,
    leg: &mut CloseLeg<'_, 'info>,
    clock: &Clock,
    min_payout: Option<u64>,
) -> Result<u64> {
    if clock.unix_timestamp >= leg.swap.matures_ts {
        return settle_leg(sh, leg, clock, min_payout);
    }
    apply_pending(leg.pool, clock.slot);
    let pnl = mark(
        leg.pool,
        leg.benchmark,
        leg.swap,
        clock.slot,
        clock.unix_timestamp,
        leg.swap.collateral,
        Unwind::Quoted,
    )?;
    close_leg(
        sh,
        leg,
        pnl,
        |p| SwapState::Cancelled { pnl: p },
        false,
        FixingKind::Live,
        min_payout,
    )
}

/// Permissionless: settles a matured swap against the benchmark path over its term. The value is a pure function
/// of the swap and the published history up to maturity; it does not depend on when, or by whom, this runs.
///
/// A leg of a basis swap settles here on its own, like any swap: each leg is fully collateralised in its own
/// pool and valued against its own benchmark, so nothing about the other leg enters its payoff, and the two
/// legs may be settled in either order or by different crankers.
pub fn settle(ctx: Context<CloseSwap>) -> Result<()> {
    settle_inner(ctx, None)
}

fn settle_inner(ctx: Context<CloseSwap>, min_payout: Option<u64>) -> Result<()> {
    let clock = Clock::get()?;
    let x = &mut *ctx.accounts;
    let sh = CloseShared {
        signer: x.signer.key(),
        trader_usdc: &x.trader_usdc,
        cranker_usdc: x.cranker_usdc.as_deref(),
        usdc_mint: &x.usdc_mint,
        token_program: &x.token_program,
        event_authority: x.event_authority.to_account_info(),
    };
    let mut leg = CloseLeg {
        pool: &mut x.pool,
        benchmark: &x.benchmark,
        swap: &mut x.swap,
        vault: &mut x.vault,
    };
    settle_leg(&sh, &mut leg, &clock, min_payout).map(|_| ())
}

/// Settlement of one leg on the shared accounts; see `settle`.
pub(crate) fn settle_leg<'info>(
    sh: &CloseShared<'_, 'info>,
    leg: &mut CloseLeg<'_, 'info>,
    clock: &Clock,
    min_payout: Option<u64>,
) -> Result<u64> {
    apply_pending(leg.pool, clock.slot);
    let s = &*leg.swap;
    require!(clock.unix_timestamp >= s.matures_ts, BrinkError::NotMatured);
    let b = leg.benchmark;
    require!(b.published, BrinkError::BenchmarkNotPublished);
    // Accrual at maturity from the fixings record; accrual after maturity is never included and the lookup is
    // total, so a late settlement is never refused (audit F-34). A maturity older than the fixings ring is valued
    // from the realised average over history that contains the term, never from the latest segment (M-3).
    // A forward settles over [start, maturity]. If its start crank never ran, the start reading
    // is taken from the fixings ring here, so a missed crank never blocks a settlement.
    let from = s.accrual_from();
    let start_accrual = if s.is_started() { s.index_accrual_start } else { accrual_at(b, from)? };
    let (end, kind) = match accrual_lookup(b, s.matures_ts)? {
        (_, FixingKind::Fallback) => (
            fallback_maturity_accrual(b, from, s.matures_ts, start_accrual)?,
            FixingKind::Fallback,
        ),
        other => other,
    };
    let term = s.matures_ts.checked_sub(from).ok_or(BrinkError::Overflow)?;
    let pnl = clamp_to_collateral(
        pnl_from_accrual(s.leg, start_accrual, end, s.fixed_bp, s.notional, term)?,
        s.collateral,
    )?;
    close_leg(
        sh,
        leg,
        pnl,
        |p| SwapState::Settled { pnl: p },
        true,
        kind,
        min_payout,
    )
}

/// Permissionless crank: liquidates a swap within the pre-maturity window or whose mark loss has consumed
/// 99 percent of its collateral. Residual collateral is returned to the trader.
///
/// A leg of a basis swap is liquidated on its own and the other leg is not touched: each leg posted its own
/// collateral to its own pool and is marked against its own benchmark, so one leg's exhaustion or cap-out says
/// nothing about the other's solvency, and the LP protections of each pool (per-leg clamps, cap-out, floors) hold
/// without reference to the pair. The surviving leg keeps its link and remains settleable at maturity by any
/// crank, liquidatable by any crank, and cancellable by its trader as an orphan (see `cancel`).
pub fn liquidate(mut ctx: Context<CloseSwap>) -> Result<()> {
    let clock = Clock::get()?;
    apply_pending(&mut ctx.accounts.pool, clock.slot);
    let s = &ctx.accounts.swap;
    require!(
        clock.unix_timestamp < s.matures_ts,
        BrinkError::AlreadySettled
    );
    // Two triggers, both on the mid unwind so that no other trader's transient exposure can move them (L-22):
    // exhaustion, when the mark loss has consumed 99 percent of the collateral (the pre-maturity window was
    // removed because it let anyone close an in-the-money position for its collateral in the final hours, review
    // finding F-11, ADR-007); and cap-out, when the mark gain has reached the collateral, which is the most the
    // position can ever be paid. Closing a capped position at its full collateral gain keeps the aggregate book
    // mark exact to within the crank's latency (M-4). Maturity is handled by `settle`.
    let unbounded = mark(
        &ctx.accounts.pool,
        &ctx.accounts.benchmark,
        s,
        clock.slot,
        clock.unix_timestamp,
        u64::MAX,
        Unwind::Mid,
    )?;
    let threshold = i64::try_from(mul_bp(
        s.collateral,
        u32::try_from(LIQUIDATION_LOSS_BP).map_err(|_| BrinkError::Overflow)?,
    )?)
    .map_err(|_| BrinkError::Overflow)?;
    let collateral = i64::try_from(s.collateral).map_err(|_| BrinkError::Overflow)?;
    let exhausted = unbounded <= threshold.checked_neg().ok_or(BrinkError::Overflow)?;
    let capped = unbounded >= collateral;
    require!(exhausted || capped, BrinkError::NotLiquidatable);
    let pnl = clamp_to_collateral(i128::from(unbounded), s.collateral)?;
    let into: fn(i64) -> SwapState = if capped {
        |p| SwapState::Capped { pnl: p }
    } else {
        |p| SwapState::Liquidated { pnl: p }
    };
    close(&mut ctx, pnl, into, true, FixingKind::Live, None)
}


/// Test fixture shared with sibling modules' tests: an empty pool with default calibration.
#[cfg(test)]
pub(crate) fn test_pool() -> Pool {

    Pool {
        benchmark: Pubkey::default(),
        share_mint: Pubkey::default(),
        vault: Pubkey::default(),
        hook_program: Pubkey::default(),
        hooks: HookFlags::default(),
        pricer: Pricer::Vernier,
        params: VernierParamsOnChain {
            model_pay_bp: [11, 22, 31, 54],
            model_rec_bp: [9, 12, 14, 19],
            term_bp: [3, 5, 7, 12],
            demand_k_bp: 45,
            demand_cap_bp: 60,
            collateral_bp: [120, 230, 330, 600],
        },
        pending_params: VernierParamsOnChain {
            model_pay_bp: [12, 22, 31, 54],
            model_rec_bp: [9, 12, 14, 19],
            term_bp: [3, 5, 7, 12],
            demand_k_bp: 45,
            demand_cap_bp: 60,
            collateral_bp: [120, 230, 330, 600],
        },
        pending_effective_slot: 1_000,
        tvl: 1,
        collateral_held: 0,
        util_pay_bp: 0,
        util_rec_bp: 0,
        open_pay_notional: 0,
        open_rec_notional: 0,
        open_swaps: 0,
        fees_lifetime: 0,
        event_seq: 0,
        min_notional: 1,
        max_notional: u64::MAX,
        bump: 0,
        fees_buyback_accrued: 0,
        fees_treasury_accrued: 0,
        withdraw_reserved: 0,
        book_pay: BookSide::default(),
        book_rec: BookSide::default(),
        collateral_pay: 0,
        collateral_rec: 0,
        ladder: [0; LADDER_BYTES],
        share_supply: 0,
        queued_shares: 0,
        queue_first_slot: 0,
        limited_window_start: 0,
        limited_window_notional: 0,
        _reserved: [0; 24],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn crank_bounty_is_two_bp_capped() {
        assert_eq!(crank_bounty(0).unwrap(), 0);
        assert_eq!(crank_bounty(1_000_000_000).unwrap(), 200_000); // 1 000 USDC -> 0.20 USDC
        assert_eq!(crank_bounty(100_000_000_000).unwrap(), 20_000_000); // 100 000 USDC -> 20 USDC
        assert_eq!(crank_bounty(u64::MAX / 2).unwrap(), CRANK_BOUNTY_CAP);
    }
    fn pool() -> Pool {
        test_pool()
    }
    #[test]
    fn pending_calibration_applies_once_due() {
        let mut p = pool();
        apply_pending(&mut p, 999);
        assert_eq!(p.params.model_pay_bp[0], 11);
        apply_pending(&mut p, 1_000);
        assert_eq!(p.params.model_pay_bp[0], 12);
        assert_eq!(p.pending_effective_slot, 0);
    }
    #[test]
    fn average_recovers_constant_rate() {
        // 684 bp held for 90 days.
        let secs = 90 * SECONDS_PER_DAY;
        let end = u128::from(684u16) * u128::try_from(secs).unwrap() * brink_index::ACCRUAL_SCALE;
        assert_eq!(average_bp(0, end, secs).unwrap(), 684);
    }
    #[test]
    fn average_rejects_zero_term() {
        assert!(average_bp(0, 1, 0).is_err());
    }

    const DAY: i64 = SECONDS_PER_DAY;
    fn benchmark() -> Benchmark {
        Benchmark {
            registry: Pubkey::default(),
            id: [1; 16],
            source: Pubkey::default(),
            value_bp: 0,
            ema_bp: 0,
            slot: 0,
            unix_ts: 0,
            accrual_e18: 0,
            max_staleness_slots: 1,
            band_bp: 1,
            half_life_slots: 1,
            min_interval_slots: 0,
            published: false,
            publish_count: 0,
            bump: 0,
            prev_value_bp: 0,
            prev_unix_ts: 0,
            version: brink_index::LAYOUT_VERSION,
            max_drift_bp: 400,
            drift_window_slots: 216_000,
            drift_anchor_bp: 0,
            drift_anchor_slot: 0,
            clamped: false,
            disputed: false,
            support: 1,
            observations: [brink_index::Observation::default(); brink_index::MAX_PUBLISHERS],
            ema_milli_bp: 0,
            fixing_first_day: 0,
            fixing_head_day: 0,
            fixings: [0; brink_index::FIXING_BYTES],
            ema_slot: 0,
            accepted_slot: 0,
            _reserved: [0; 12],
        }
    }
    /// The accrual part of `brink_index::publish`, for host tests.
    fn publish(b: &mut Benchmark, now: i64, value_bp: u16) {
        if b.published {
            b.record_fixings(now).unwrap();
            b.accrual_e18 += u128::from(b.value_bp)
                * u128::try_from(now - b.unix_ts).unwrap()
                * brink_index::ACCRUAL_SCALE;
            b.prev_value_bp = b.value_bp;
            b.prev_unix_ts = b.unix_ts;
        } else {
            b.published = true;
            b.prev_value_bp = value_bp;
            b.prev_unix_ts = now;
        }
        b.value_bp = value_bp;
        b.unix_ts = now;
    }
    fn flat(bp: u16, secs: i64) -> u128 {
        u128::from(bp) * u128::try_from(secs).unwrap() * brink_index::ACCRUAL_SCALE
    }

    #[test]
    fn limited_budget_is_per_pool_per_hour() {
        let mut p = pool();
        let cap = 100_000_000_000u64;
        let t = 1_700_000_000i64;
        // Three 100,000 opens in one slot by any traders: only the first fits (S-2).
        p.charge_limited_budget(t, cap, cap).unwrap();
        assert!(p.charge_limited_budget(t, 1, cap).is_err());
        assert!(p.charge_limited_budget(t + 3_599, 1, cap).is_err());
        // The window restarts after a quiet hour; splitting into 60,000 + 60,000 still fails.
        p.charge_limited_budget(t + 3_600, 60_000_000_000, cap)
            .unwrap();
        assert!(p
            .charge_limited_budget(t + 3_601, 60_000_000_000, cap)
            .is_err());
        assert!(p
            .charge_limited_budget(t + 3_601, 40_000_000_000, cap)
            .is_ok());
        assert_eq!(p.limited_window_notional, cap);
    }
    #[test]
    fn accrual_lookup_is_total_and_exact_on_fixings() {
        // Review F-12 / audit F-34: 684 bp flat, swap opened at 09:00 on day 20 000 for 28 days, maturity
        // aligned to midnight of day 20 029; the publisher then publishes every day for 60 more days.
        let mut b = benchmark();
        let t_open = 20_000 * DAY + 9 * 3_600;
        publish(&mut b, t_open - 60, 684);
        let start = accrual_at(&b, t_open).unwrap();
        let matures = align_up_to_day(t_open + 28 * DAY).unwrap();
        assert_eq!(matures, 20_029 * DAY);
        let mut t = t_open;
        for _ in 0..90 {
            t += DAY;
            publish(&mut b, t + 17, 684);
        }
        let (end, kind) = accrual_lookup(&b, matures).unwrap();
        assert_eq!(kind, FixingKind::Fixing);
        assert_eq!(end - start, flat(684, matures - t_open));
        // Settlement value equals the flat-rate value to the unit; a pay-fixed trader at 698 books the loss.
        let pnl = pnl_from_accrual(
            LegKind::PayFixed,
            start,
            end,
            698,
            100_000_000_000,
            matures - t_open,
        )
        .unwrap();
        let expected = -(14i128 * 100_000_000_000 * i128::from(matures - t_open) / 315_360_000_000);
        assert_eq!(pnl, expected);
        // Patch 0002 alone would have refused this (two publishes after maturity); the old code inflated it.
        assert!(matures < b.prev_unix_ts);

        // The other kinds, on the same history.
        assert_eq!(
            accrual_lookup(&b, b.unix_ts + 5).unwrap().1,
            FixingKind::Live
        );
        assert_eq!(
            accrual_lookup(&b, b.prev_unix_ts + 5).unwrap().1,
            FixingKind::Segment
        );
        let (mid, kind) = accrual_lookup(&b, 20_029 * DAY + 7 * 3_600).unwrap();
        assert_eq!(kind, FixingKind::Interpolated);
        assert_eq!(mid, end + flat(684, 7 * 3_600)); // flat rate: interpolation is exact
                                                     // Older than the ring: the ring holds 128 days; day 19 999 is before the first fixing.
        let (old, kind) = accrual_lookup(&b, 19_999 * DAY + 100).unwrap();
        assert_eq!(kind, FixingKind::Fallback);
        assert_eq!(old, 0); // flat backwards extrapolation saturates at zero here
    }

    #[test]
    fn interpolation_is_deterministic_and_bounded_when_the_rate_moved_inside_the_day() {
        let mut b = benchmark();
        let d = 30_000 * DAY;
        publish(&mut b, d - DAY, 500);
        publish(&mut b, d + 6 * 3_600, 500); // 06:00
        publish(&mut b, d + 6 * 3_600 + 60, 900); // rate jumps to 900 at 06:01
        publish(&mut b, d + DAY + 10, 900);
        publish(&mut b, d + DAY + 20, 900);
        publish(&mut b, d + 2 * DAY + 20, 900);
        let f0 = b.fixing(30_000).unwrap();
        let f1 = b.fixing(30_001).unwrap();
        let (noon, kind) = accrual_lookup(&b, d + 12 * 3_600).unwrap();
        assert_eq!(kind, FixingKind::Interpolated);
        // The true value at noon (500 until 06:01, 900 after) and the day-average value bracket each other within
        // the intra-day change: |error| <= (900 - 500) x 12 h.
        let truth = f0 + flat(500, 6 * 3_600 + 60) + flat(900, 6 * 3_600 - 60);
        assert_eq!(noon, f0 + (f1 - f0) / 2);
        assert!(noon.abs_diff(truth) <= flat(400, 12 * 3_600));
        // Same answer whatever is published later.
        publish(&mut b, d + 40 * DAY, 100);
        assert_eq!(
            accrual_lookup(&b, d + 12 * 3_600).unwrap(),
            (noon, FixingKind::Interpolated)
        );
    }

    /// External scan 1, M-3: a maturity older than the fixings ring is valued from the realised average over
    /// history containing the term; later publications cannot turn a loss into a gain or refuse the close.
    #[test]
    fn evicted_maturity_settles_from_realised_history_not_the_latest_segment() {
        let mut b = benchmark();
        let t_open = 20_010 * DAY + 9 * 3_600;
        publish(&mut b, t_open - 60, 684);
        let start = accrual_at(&b, t_open).unwrap();
        let matures = align_up_to_day(t_open + 28 * DAY).unwrap();
        // 684 bp held until day 170 (fixing ring now starts after maturity), then 400 bp published twice.
        let mut t = t_open;
        for _ in 0..160 {
            t += DAY;
            publish(&mut b, t + 17, 684);
        }
        t += DAY;
        publish(&mut b, t, 400);
        publish(&mut b, t + 60, 400);
        assert_eq!(accrual_lookup(&b, matures).unwrap().1, FixingKind::Fallback);
        let end = fallback_maturity_accrual(&b, t_open, matures, start).unwrap();
        // Exactly the flat 684 bp over the term, which is what the trader actually realised.
        assert_eq!(end - start, flat(684, matures - t_open));
        // A pay-fixed trader at 700 books a small loss, not the 1,000 USDC gain the old extrapolation produced.
        let pnl = pnl_from_accrual(
            LegKind::PayFixed,
            start,
            end,
            700,
            100_000_000_000,
            matures - t_open,
        )
        .unwrap();
        assert!(pnl < 0 && pnl > -20_000_000, "{pnl}");
        // Publishing a much higher rate afterwards cannot push the estimate below the opening accrual.
        for _ in 0..130 {
            t += DAY;
            publish(&mut b, t + 17, 2_000);
        }
        let end2 = fallback_maturity_accrual(&b, t_open, matures, start).unwrap();
        assert!(end2 >= start);
        assert!(pnl_from_accrual(
            LegKind::PayFixed,
            start,
            end2,
            700,
            100_000_000_000,
            matures - t_open
        )
        .is_ok());
    }

    /// External scan 1, M-17: a closing swap releases the budget it was charged in the open window only.
    #[test]
    fn limited_budget_is_released_on_close_within_the_window() {
        let mut p = pool();
        let cap = 100_000_000_000u64;
        let t = 1_700_000_000i64;
        let w = p.charge_limited_budget(t, cap, cap).unwrap();
        assert!(p.charge_limited_budget(t + 10, 1, cap).is_err());
        p.release_limited_budget(w, cap);
        assert_eq!(p.limited_window_notional, 0);
        p.charge_limited_budget(t + 20, cap, cap).unwrap();
        // A swap from an earlier window, or one opened in Normal mode (window 0), releases nothing.
        p.release_limited_budget(w - 3_600, cap);
        p.release_limited_budget(0, cap);
        assert_eq!(p.limited_window_notional, cap);
    }

    /// External scan 1, M-5: the maturity ladder counts open swaps by day and flags a matured one until settled.
    #[test]
    fn maturity_ladder_gates_lp_pricing_until_settlement() {
        let mut p = pool();
        let d = 20_100 * DAY;
        p.ladder_add(d).unwrap();
        p.ladder_add(d).unwrap();
        p.ladder_add(d + 90 * DAY).unwrap();
        assert!(!p.has_matured_open(d - 1));
        assert!(p.has_matured_open(d));
        assert!(p.require_no_matured_open(d + 5).is_err());
        p.ladder_sub(d).unwrap();
        assert!(p.has_matured_open(d + 5));
        p.ladder_sub(d).unwrap();
        assert!(!p.has_matured_open(d + 5));
        assert!(p.has_matured_open(d + 90 * DAY));
        // The bucket for day d + 256 aliases day d: it is free once d's swaps have settled.
        p.ladder_add(d + 256 * DAY).unwrap();
        // While d + 90 is still open and matured, a new swap aliasing its bucket is refused.
        assert!(p.ladder_add(d + 90 * DAY + 256 * DAY).is_err());
        p.ladder_sub(d + 90 * DAY).unwrap();
        p.ladder_add(d + 90 * DAY + 256 * DAY).unwrap();
    }

    /// External scan 1, M-4: a gain beyond collateral on one leg is not netted against a loss on the other.
    #[test]
    fn book_mark_clamps_each_leg_to_its_own_collateral() {
        let n = 1_000_000_000_000u64;
        let o = 20_000 * SECONDS_PER_DAY;
        let m = o + 90 * SECONDS_PER_DAY;
        let mut p = pool();
        p.tvl = 100_000_000_000;
        // Pay-fixed at 700 with 1 bp of collateral; receive-fixed at 700 with ample collateral.
        p.book_add(&BookTerms::for_swap(LegKind::PayFixed, n, 0, 700, o, m).unwrap())
            .unwrap();
        p.collateral_pay = 100_000;
        p.book_add(&BookTerms::for_swap(LegKind::ReceiveFixed, n / 2, 0, 700, o, m).unwrap())
            .unwrap();
        p.collateral_rec = 50_000_000_000;
        p.collateral_held = p.collateral_pay + p.collateral_rec;
        let now = o + 30 * SECONDS_PER_DAY;
        let a_now = 800u128 * 30 * 86_400 * brink_index::ACCRUAL_SCALE;
        // Unclamped: pay side is owed 2,465.75 USDC, rec side owes half of that.
        let raw = p.book_value(a_now, 800, now).unwrap();
        assert_eq!(raw, 2_465_753_424 - 1_232_876_712);
        // Clamped: the pay side can collect at most its 0.10 USDC collateral; the rec side's loss stands.
        let clamped = p.book_value_clamped(a_now, 800, now).unwrap();
        assert_eq!(clamped, 100_000 - 1_232_876_712);
        // Withdrawals give no credit for the unrealised loss: effective capital is tvl.
        assert_eq!(
            p.effective_tvl_for_withdraw(a_now, 800, now).unwrap(),
            p.tvl
        );
    }

    /// External scan 1, M-18: an eligible queue reserves the capacity above the caps' floor.
    #[test]
    fn eligible_queue_has_priority_over_new_exposure() {
        let mut p = pool();
        p.tvl = 1_000_000_000_000; // 1 M USDC
        p.share_supply = 1_000_000_000_000_000; // 1 M shares at par
        p.queued_shares = 400_000_000_000_000; // 400 k USDC queued
        p.queue_first_slot = 1_000;
        use super::super::queue::{require_queue_priority, WITHDRAW_EPOCH_SLOTS};
        let early = 1_000 + WITHDRAW_EPOCH_SLOTS - 1;
        let due = 1_000 + WITHDRAW_EPOCH_SLOTS;
        // Before the epoch is eligible, opens are free.
        assert!(require_queue_priority(&p, vernier::Leg::Pay, 900_000_000_000, early).is_ok());
        // Once eligible: 900 k pay notional needs a floor of 900 k / 0.48 > 1 M, leaving the queue nothing.
        assert!(require_queue_priority(&p, vernier::Leg::Pay, 900_000_000_000, due).is_err());
        // 100 k notional: floor about 208 k, capacity about 792 k >= 400 k owed.
        assert!(require_queue_priority(&p, vernier::Leg::Pay, 100_000_000_000, due).is_ok());
        // Nothing queued: no reservation.
        p.queued_shares = 0;
        assert!(require_queue_priority(&p, vernier::Leg::Pay, 900_000_000_000, due).is_ok());
    }
}
