//! Pool-state model: a faithful host-side mirror of the accounting in `programs/swap_amm` and `programs/brink_index`,
//! transcribed instruction by instruction from the source (`instructions/{swap,liquidity,fees,admin,math}.rs`,
//! `brink_index/src/lib.rs`). Every check runs in the same order as the handler so that a rejected action carries
//! the same error name the program would log, and every balance moves by the same integer amount.
//!
//! The model is used twice: the fast layer runs it alone for Monte Carlo; the slow layer runs it next to LiteSVM
//! and asserts field-by-field equality with the on-chain accounts after every action.

use std::collections::HashMap;

use vernier::{Leg, Params, Tenor};

pub const USDC: u64 = 1_000_000;
pub const SECONDS_PER_DAY: i64 = 86_400;
pub const DAYS_PER_YEAR: u128 = 365;
pub const BP: u128 = 10_000;
pub const ACCRUAL_SCALE: u128 = 1_000_000_000_000_000_000;
pub const MAX_RATE_BP: u16 = 30_000;
pub const LIQUIDATION_WINDOW_SECS: i64 = 6 * 3_600;
pub const LIQUIDATION_LOSS_BP: u32 = 9_900;
pub const CRANK_BOUNTY_BP: u32 = 2;
pub const CRANK_BOUNTY_CAP: u64 = 25_000_000;
pub const BUYBACK_BP: u64 = 5_000;
/// Slots per second on the simulated clock (400 ms slots).
pub const SLOTS_PER_DAY: u64 = 216_000;

pub const DEFAULT_PARAMS: Params = vernier::DEFAULT_PARAMS;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Limited,
    WithdrawOnly,
    Halted,
}
impl Mode {
    #[must_use]
    pub fn severity(self) -> u8 {
        match self {
            Mode::Normal => 0,
            Mode::Limited => 1,
            Mode::WithdrawOnly => 2,
            Mode::Halted => 3,
        }
    }
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Mode::Normal => "Normal",
            Mode::Limited => "Limited",
            Mode::WithdrawOnly => "WithdrawOnly",
            Mode::Halted => "Halted",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Actor {
    Authority,
    Guardian,
    Stranger,
}

/// Error names exactly as the programs log them (`Error Code: X` for Anchor errors, the SPL Token message for
/// token-program failures).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Err {
    Halted,
    WithdrawOnly,
    LimitedModeCap,
    BenchmarkNotPublished,
    BenchmarkStale,
    BenchmarkOutOfBand,
    PoolInvariant,
    Conservation,
    LegCap,
    LimitRate,
    Slippage,
    NotionalTooSmall,
    NotionalTooLarge,
    Tenor,
    NotMatured,
    AlreadySettled,
    NotLiquidatable,
    CalibrationStep,
    GuardianScope,
    NothingToSweep,
    Overflow,
    ConstraintSigner,
    /// brink_index
    RateCeiling,
    TooFrequent,
    OutOfBand,
    ClockWentBackwards,
    /// SPL Token: a transfer or burn above the source balance.
    InsufficientFunds,
    /// No such open swap (the on-chain account does not exist).
    NoSuchSwap,
    /// Anchor `init` on a PDA that already exists (system program log).
    AlreadyInUse,
}
impl Err {
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Err::Halted => "Halted",
            Err::WithdrawOnly => "WithdrawOnly",
            Err::LimitedModeCap => "LimitedModeCap",
            Err::BenchmarkNotPublished => "BenchmarkNotPublished",
            Err::BenchmarkStale => "BenchmarkStale",
            Err::BenchmarkOutOfBand => "BenchmarkOutOfBand",
            Err::PoolInvariant => "PoolInvariant",
            Err::Conservation => "Conservation",
            Err::LegCap => "LegCap",
            Err::LimitRate => "LimitRate",
            Err::Slippage => "Slippage",
            Err::NotionalTooSmall => "NotionalTooSmall",
            Err::NotionalTooLarge => "NotionalTooLarge",
            Err::Tenor => "Tenor",
            Err::NotMatured => "NotMatured",
            Err::AlreadySettled => "AlreadySettled",
            Err::NotLiquidatable => "NotLiquidatable",
            Err::CalibrationStep => "CalibrationStep",
            Err::GuardianScope => "GuardianScope",
            Err::NothingToSweep => "NothingToSweep",
            Err::Overflow => "Overflow",
            Err::ConstraintSigner => "ConstraintSigner",
            Err::RateCeiling => "RateCeiling",
            Err::TooFrequent => "TooFrequent",
            Err::OutOfBand => "OutOfBand",
            Err::ClockWentBackwards => "ClockWentBackwards",
            Err::InsufficientFunds => "insufficient funds",
            Err::NoSuchSwap => "AccountNotInitialized",
            Err::AlreadyInUse => "already in use",
        }
    }
}

pub type R<T> = Result<T, Err>;

// ----------------------------------------------------------------------------------------------------------------
// Pure helpers, transcribed from swap_amm/src/instructions/math.rs and fees.rs.
// ----------------------------------------------------------------------------------------------------------------

pub fn mul_bp(x: u64, bp: u32) -> R<u64> {
    u64::try_from(u128::from(x).checked_mul(u128::from(bp)).ok_or(Err::Overflow)? / 10_000).map_err(|_| Err::Overflow)
}
pub fn shares_for(amount: u64, tvl: u64, supply: u64) -> R<u64> {
    if supply == 0 || tvl == 0 {
        return Ok(amount);
    }
    u64::try_from(
        u128::from(amount)
            .checked_mul(u128::from(supply))
            .ok_or(Err::Overflow)?
            .checked_div(u128::from(tvl))
            .ok_or(Err::PoolInvariant)?,
    )
    .map_err(|_| Err::Overflow)
}
pub fn amount_for(shares: u64, tvl: u64, supply: u64) -> R<u64> {
    if supply == 0 {
        return Err(Err::PoolInvariant);
    }
    u64::try_from(
        u128::from(shares)
            .checked_mul(u128::from(tvl))
            .ok_or(Err::Overflow)?
            .checked_div(u128::from(supply))
            .ok_or(Err::PoolInvariant)?,
    )
    .map_err(|_| Err::Overflow)
}
pub fn share_price_e6(tvl: u64, supply: u64) -> u64 {
    if supply == 0 {
        return 1_000_000;
    }
    u64::try_from(u128::from(tvl) * 1_000_000 / u128::from(supply)).unwrap_or(u64::MAX)
}
pub fn pnl_bounded(diff_bp: i64, notional: u64, days: u16, collateral: u64) -> R<i64> {
    let mag = u128::from(diff_bp.unsigned_abs())
        .checked_mul(u128::from(notional))
        .ok_or(Err::Overflow)?
        .checked_mul(u128::from(days))
        .ok_or(Err::Overflow)?
        / (BP * DAYS_PER_YEAR);
    let mag = u64::try_from(mag).unwrap_or(u64::MAX).min(collateral);
    let mag = i64::try_from(mag).map_err(|_| Err::Overflow)?;
    Ok(if diff_bp < 0 { mag.checked_neg().ok_or(Err::Overflow)? } else { mag })
}
/// Unbounded magnitude of the same formula (for the collateral-shortfall metric), saturating.
/// `math::pnl_secs`: unbounded pnl at second resolution, floored.
pub fn pnl_secs(diff_bp: i64, notional: u64, seconds: i64) -> R<i128> {
    let secs = u128::try_from(seconds.max(0)).map_err(|_| Err::Overflow)?;
    let mag = u128::from(diff_bp.unsigned_abs())
        .checked_mul(u128::from(notional))
        .ok_or(Err::Overflow)?
        .checked_mul(secs)
        .ok_or(Err::Overflow)?
        .checked_div(BP.checked_mul(DAYS_PER_YEAR).and_then(|x| x.checked_mul(86_400)).ok_or(Err::Overflow)?)
        .ok_or(Err::Overflow)?;
    let mag = i128::try_from(mag).map_err(|_| Err::Overflow)?;
    Ok(if diff_bp < 0 { mag.checked_neg().ok_or(Err::Overflow)? } else { mag })
}

/// `math::clamp_to_collateral`.
pub fn clamp_to_collateral(value: i128, bound: u64) -> R<i64> {
    let b = i128::from(bound);
    let v = value.clamp(b.checked_neg().ok_or(Err::Overflow)?, b);
    i64::try_from(v).map_err(|_| Err::Overflow)
}

pub fn pnl_unbounded_mag(diff_bp: i64, notional: u64, days: u16) -> u128 {
    u128::from(diff_bp.unsigned_abs()) * u128::from(notional) * u128::from(days) / (BP * DAYS_PER_YEAR)
}
pub fn oriented_diff(leg: Leg, floating_bp: i64, fixed_bp: i64) -> R<i64> {
    let d = floating_bp.checked_sub(fixed_bp).ok_or(Err::Overflow)?;
    Ok(match leg {
        Leg::Pay => d,
        Leg::Receive => d.checked_neg().ok_or(Err::Overflow)?,
    })
}
pub fn days_remaining(now: i64, matures_ts: i64, total_days: u16) -> u16 {
    let secs = matures_ts.saturating_sub(now).max(0);
    let d = (secs.saturating_add(SECONDS_PER_DAY.saturating_sub(1))) / SECONDS_PER_DAY;
    u16::try_from(d).unwrap_or(total_days).min(total_days)
}
pub fn tenor_from(u: u8) -> R<Tenor> {
    Ok(match u {
        0 => Tenor::D28,
        1 => Tenor::D60,
        2 => Tenor::D90,
        3 => Tenor::D180,
        _ => return Err(Err::Tenor),
    })
}
pub fn opposite(l: Leg) -> Leg {
    match l {
        Leg::Pay => Leg::Receive,
        Leg::Receive => Leg::Pay,
    }
}
pub fn split(fee: u64) -> (u64, u64) {
    let buyback = u64::try_from(u128::from(fee).saturating_mul(u128::from(BUYBACK_BP)) / 10_000).unwrap_or(0);
    (buyback, fee.saturating_sub(buyback))
}
pub fn crank_bounty(notional: u64) -> R<u64> {
    Ok(mul_bp(notional, CRANK_BOUNTY_BP)?.min(CRANK_BOUNTY_CAP))
}
pub fn within_step(old: u16, new: u16) -> bool {
    let lo = old / 2;
    let hi = (old.saturating_mul(3) / 2).saturating_add(1);
    new >= lo && new <= hi
}

// ----------------------------------------------------------------------------------------------------------------
// Benchmark (brink_index)
// ----------------------------------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Benchmark {
    pub value_bp: u16,
    pub ema_bp: u16,
    pub slot: u64,
    pub unix_ts: i64,
    pub accrual_e18: u128,
    pub max_staleness_slots: u64,
    pub band_bp: u16,
    pub half_life_slots: u64,
    pub min_interval_slots: u64,
    pub published: bool,
    pub publish_count: u64,
    /// Model-only: every accepted publish as `(unix_ts, accrual_e18 after the publish, value_bp)`, for the true
    /// (non-extrapolated) accrual used by the fairness metrics.
    pub history: Vec<(i64, u128, u16)>,
}

impl Benchmark {
    #[must_use]
    pub fn new(band_bp: u16, max_staleness_slots: u64, half_life_slots: u64, min_interval_slots: u64) -> Self {
        Self {
            value_bp: 0,
            ema_bp: 0,
            slot: 0,
            unix_ts: 0,
            accrual_e18: 0,
            max_staleness_slots,
            band_bp,
            half_life_slots,
            min_interval_slots,
            published: false,
            publish_count: 0,
            history: Vec::new(),
        }
    }
    /// `brink_index::publish`.
    pub fn publish(&mut self, value_bp: u16, slot: u64, ts: i64) -> R<()> {
        if value_bp > MAX_RATE_BP {
            return Err(Err::RateCeiling);
        }
        if self.published {
            if slot.saturating_sub(self.slot) < self.min_interval_slots {
                return Err(Err::TooFrequent);
            }
            if value_bp.abs_diff(self.ema_bp) > self.band_bp {
                return Err(Err::OutOfBand);
            }
            let elapsed = u128::try_from(ts.checked_sub(self.unix_ts).ok_or(Err::ClockWentBackwards)?)
                .map_err(|_| Err::ClockWentBackwards)?;
            let add = u128::from(self.value_bp)
                .checked_mul(elapsed)
                .and_then(|x| x.checked_mul(ACCRUAL_SCALE))
                .ok_or(Err::Overflow)?;
            self.accrual_e18 = self.accrual_e18.checked_add(add).ok_or(Err::Overflow)?;
            let dt = u128::from(slot.saturating_sub(self.slot));
            let hl = u128::from(self.half_life_slots);
            let ema = u128::from(self.ema_bp);
            let num = ema
                .checked_mul(hl)
                .ok_or(Err::Overflow)?
                .checked_add(u128::from(value_bp).checked_mul(dt).ok_or(Err::Overflow)?)
                .ok_or(Err::Overflow)?;
            let new = num.checked_div(hl.checked_add(dt).ok_or(Err::Overflow)?).ok_or(Err::Overflow)?;
            self.ema_bp = u16::try_from(new).map_err(|_| Err::Overflow)?;
        } else {
            self.ema_bp = value_bp;
            self.published = true;
        }
        self.value_bp = value_bp;
        self.slot = slot;
        self.unix_ts = ts;
        self.publish_count += 1;
        self.history.push((ts, self.accrual_e18, value_bp));
        Ok(())
    }
    /// `swap::accrual_at`: last publish extrapolated flat to `now` (clamped at zero elapsed).
    pub fn accrual_at(&self, now: i64) -> R<u128> {
        let elapsed = u128::try_from(now.checked_sub(self.unix_ts).ok_or(Err::Overflow)?.max(0)).map_err(|_| Err::Overflow)?;
        let add = u128::from(self.value_bp)
            .checked_mul(elapsed)
            .and_then(|x| x.checked_mul(ACCRUAL_SCALE))
            .ok_or(Err::Overflow)?;
        self.accrual_e18.checked_add(add).ok_or(Err::Overflow)
    }
    /// Model-only: the true cumulative accrual at `t` from the publish history (piecewise constant, the value
    /// published at or before `t` held until the next publish). This is what the on-chain accrual would be if a
    /// publish had landed exactly at `t`.
    #[must_use]
    pub fn true_accrual_at(&self, t: i64) -> u128 {
        let i = self.history.partition_point(|(ts, _, _)| *ts <= t);
        if i == 0 {
            return 0;
        }
        let (ts, acc, v) = self.history[i - 1];
        acc + u128::from(v) * u128::try_from((t - ts).max(0)).unwrap_or(0) * ACCRUAL_SCALE
    }
    pub fn for_quote(&self, now_slot: u64) -> R<(u16, u16)> {
        if !self.published {
            return Err(Err::BenchmarkNotPublished);
        }
        if now_slot.saturating_sub(self.slot) > self.max_staleness_slots {
            return Err(Err::BenchmarkStale);
        }
        if self.value_bp.abs_diff(self.ema_bp) > self.band_bp {
            return Err(Err::BenchmarkOutOfBand);
        }
        Ok((self.value_bp, self.ema_bp))
    }
}

pub fn average_bp(start: u128, end: u128, seconds: i64) -> R<i64> {
    if seconds <= 0 {
        return Err(Err::NotMatured);
    }
    let secs = u128::try_from(seconds).map_err(|_| Err::Overflow)?;
    let num = end.checked_sub(start).ok_or(Err::Overflow)?;
    let avg = num / (ACCRUAL_SCALE * secs);
    i64::try_from(avg).map_err(|_| Err::Overflow)
}

// ----------------------------------------------------------------------------------------------------------------
// Swap AMM state
// ----------------------------------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Swap {
    pub id: u64,
    pub trader: u8,
    pub leg: Leg,
    pub tenor: u8,
    pub notional: u64,
    pub fixed_bp: u16,
    pub collateral: u64,
    pub opened_slot: u64,
    pub opened_ts: i64,
    pub matures_ts: i64,
    pub index_accrual_start: u128,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CloseKind {
    Cancelled,
    Settled,
    Liquidated,
}

/// What a close did, for the metrics.
#[derive(Clone, Debug)]
pub struct CloseEffect {
    pub kind: CloseKind,
    pub swap: Swap,
    /// pnl as booked (bounded by collateral and, for gains, by LP capital).
    pub pnl: i64,
    /// Unbounded magnitude of the pnl formula before any clamp (for shortfall metrics), signed.
    pub pnl_unbounded: i128,
    pub payout: u64,
    pub bounty: u64,
    pub fee: u64,
    /// Model-only fair value: realised pnl to `now` from the true accrual plus the forward mark (cancel and
    /// liquidate), or the settlement pnl from the true accrual over the exact term (settle), bounded like the
    /// program bounds it. `paid_minus_fair = pnl - fair` is the value transferred by the mark or by extrapolation.
    pub fair_pnl: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Global {
    pub mode: Mode,
    pub buyback_accrued: u64,
    pub treasury_accrued: u64,
    pub buyback_lifetime: u64,
    pub treasury_lifetime: u64,
    pub param_delay_slots: u64,
    pub limited_mode_cap: u64,
    /// Token balances
    pub fee_vault: u64,
    pub treasury_balance: u64,
    pub buyback_balance: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pool {
    pub params: Params,
    pub pending_params: Params,
    pub pending_effective_slot: u64,
    pub tvl: u64,
    pub collateral_held: u64,
    pub util_pay_bp: u16,
    pub util_rec_bp: u16,
    pub open_pay_notional: u64,
    pub open_rec_notional: u64,
    pub open_swaps: u32,
    pub fees_lifetime: u64,
    pub event_seq: u64,
    pub min_notional: u64,
    pub max_notional: u64,
    /// Token state
    pub vault: u64,
    pub share_supply: u64,
}

impl Pool {
    #[must_use]
    pub fn vernier(&self) -> vernier::Pool {
        vernier::Pool { tvl: self.tvl, util_pay_bp: self.util_pay_bp, util_rec_bp: self.util_rec_bp }
    }
    pub fn rebase_utilisation(&mut self) -> R<()> {
        if self.tvl == 0 {
            if self.open_pay_notional != 0 || self.open_rec_notional != 0 {
                return Err(Err::PoolInvariant);
            }
            self.util_pay_bp = 0;
            self.util_rec_bp = 0;
            return Ok(());
        }
        let util = |n: u64| -> R<u16> {
            u16::try_from(u128::from(n).checked_mul(BP).ok_or(Err::Overflow)? / u128::from(self.tvl)).map_err(|_| Err::PoolInvariant)
        };
        self.util_pay_bp = util(self.open_pay_notional)?;
        self.util_rec_bp = util(self.open_rec_notional)?;
        Ok(())
    }
    pub fn assert_invariants(&self, vault_amount: u64) -> R<()> {
        if self.tvl > 0 && !vernier::pool_invariants_hold(&self.vernier()) {
            return Err(Err::PoolInvariant);
        }
        if self.util_pay_bp > 10_000 || self.util_rec_bp > 10_000 {
            return Err(Err::PoolInvariant);
        }
        let expected = self.tvl.checked_add(self.collateral_held).ok_or(Err::Overflow)?;
        if vault_amount < expected {
            return Err(Err::Conservation);
        }
        Ok(())
    }
    pub fn apply_pending(&mut self, slot: u64) {
        if self.pending_effective_slot != 0 && slot >= self.pending_effective_slot {
            self.params = self.pending_params;
            self.pending_effective_slot = 0;
        }
    }
}

/// The whole simulated system: one benchmark, one pool, the global account, LP share holdings and trader cash.
#[derive(Clone, Debug)]
pub struct Model {
    pub slot: u64,
    pub ts: i64,
    pub bench: Benchmark,
    pub global: Global,
    pub pool: Pool,
    pub swaps: Vec<Swap>,
    pub index: HashMap<u64, usize>,
    /// LP share balances by LP index.
    pub lp_shares: Vec<u64>,
    /// Signed cash flow per trader (negative = paid in), for pnl accounting.
    pub trader_cash: Vec<i128>,
    /// Signed cash flow per LP.
    pub lp_cash: Vec<i128>,
    /// Model-only: USDC sent straight to the vault and not yet folded in by `sync_vault`.
    pub unsynced_donations: u64,
}

#[derive(Clone, Debug)]
pub struct Setup {
    pub band_bp: u16,
    pub max_staleness_slots: u64,
    pub half_life_slots: u64,
    pub min_interval_slots: u64,
    pub params: Params,
    pub param_delay_slots: u64,
    pub limited_mode_cap: u64,
    pub min_notional: u64,
    pub max_notional: u64,
    pub n_lps: usize,
    pub n_traders: usize,
    pub start_slot: u64,
    pub start_ts: i64,
}

impl Model {
    #[must_use]
    pub fn new(s: &Setup) -> Self {
        Self {
            slot: s.start_slot,
            ts: s.start_ts,
            bench: Benchmark::new(s.band_bp, s.max_staleness_slots, s.half_life_slots, s.min_interval_slots),
            global: Global {
                mode: Mode::Normal,
                buyback_accrued: 0,
                treasury_accrued: 0,
                buyback_lifetime: 0,
                treasury_lifetime: 0,
                param_delay_slots: s.param_delay_slots,
                limited_mode_cap: s.limited_mode_cap,
                fee_vault: 0,
                treasury_balance: 0,
                buyback_balance: 0,
            },
            pool: Pool {
                params: s.params,
                pending_params: s.params,
                pending_effective_slot: 0,
                tvl: 0,
                collateral_held: 0,
                util_pay_bp: 0,
                util_rec_bp: 0,
                open_pay_notional: 0,
                open_rec_notional: 0,
                open_swaps: 0,
                fees_lifetime: 0,
                event_seq: 0,
                min_notional: s.min_notional,
                max_notional: s.max_notional,
                vault: 0,
                share_supply: 0,
            },
            swaps: Vec::new(),
            index: HashMap::new(),
            lp_shares: vec![0; s.n_lps],
            trader_cash: vec![0; s.n_traders],
            lp_cash: vec![0; s.n_lps],
            unsynced_donations: 0,
        }
    }

    pub fn warp(&mut self, slots: u64, secs: i64) {
        self.slot += slots;
        self.ts += secs;
    }

    pub fn swap(&self, id: u64) -> Option<&Swap> {
        self.index.get(&id).map(|i| &self.swaps[*i])
    }

    fn remove_swap(&mut self, id: u64) -> Swap {
        let i = self.index.remove(&id).expect("indexed");
        let s = self.swaps.swap_remove(i);
        if i < self.swaps.len() {
            let moved = self.swaps[i].id;
            self.index.insert(moved, i);
        }
        s
    }

    // ---- brink_index::publish ----
    pub fn publish(&mut self, value_bp: u16) -> R<()> {
        self.bench.publish(value_bp, self.slot, self.ts)
    }

    // ---- lp_deposit ----
    pub fn deposit(&mut self, lp: usize, amount: u64, min_shares: u64) -> R<u64> {
        // Atomicity: a failed instruction reverts `apply_pending` as well.
        let saved = self.pool.clone();
        let r = self.deposit_inner(lp, amount, min_shares);
        if r.is_err() {
            self.pool = saved;
        }
        r
    }

    fn deposit_inner(&mut self, lp: usize, amount: u64, min_shares: u64) -> R<u64> {
        match self.global.mode {
            Mode::Halted => return Err(Err::Halted),
            Mode::WithdrawOnly => return Err(Err::WithdrawOnly),
            _ => {}
        }
        if amount == 0 {
            return Err(Err::NotionalTooSmall);
        }
        self.pool.apply_pending(self.slot);
        let supply = self.pool.share_supply;
        let shares = shares_for(amount, self.pool.tvl, supply)?;
        if !(shares >= min_shares && shares > 0) {
            return Err(Err::Slippage);
        }
        // Everything below is the post-transfer state; compute and validate before committing.
        let mut pool = self.pool.clone();
        pool.vault = pool.vault.checked_add(amount).ok_or(Err::Overflow)?;
        pool.share_supply = supply.checked_add(shares).ok_or(Err::Overflow)?;
        pool.tvl = pool.tvl.checked_add(amount).ok_or(Err::Overflow)?;
        pool.rebase_utilisation()?;
        pool.event_seq = pool.event_seq.checked_add(1).ok_or(Err::Overflow)?;
        pool.assert_invariants(pool.vault)?;
        self.pool = pool;
        self.lp_shares[lp] += shares;
        self.lp_cash[lp] -= i128::from(amount);
        Ok(shares)
    }

    // ---- lp_withdraw ----
    pub fn withdraw(&mut self, lp: usize, shares: u64, min_amount: u64) -> R<(u64, u64)> {
        // Atomicity: a failed instruction reverts `apply_pending` as well.
        let saved = self.pool.clone();
        let r = self.withdraw_inner(lp, shares, min_amount);
        if r.is_err() {
            self.pool = saved;
        }
        r
    }

    fn withdraw_inner(&mut self, lp: usize, shares: u64, min_amount: u64) -> R<(u64, u64)> {
        if self.global.mode == Mode::Halted {
            return Err(Err::Halted);
        }
        if shares == 0 {
            return Err(Err::NotionalTooSmall);
        }
        self.pool.apply_pending(self.slot);
        let supply = self.pool.share_supply;
        let gross = amount_for(shares, self.pool.tvl, supply)?;
        let open = self.pool.open_swaps > 0;
        let fee = if open { mul_bp(gross, vernier::LP_EXIT_FEE_BP)? } else { 0 };
        let net = gross.checked_sub(fee).ok_or(Err::Overflow)?;
        if !(net >= min_amount && net > 0) {
            return Err(Err::Slippage);
        }
        // burn: the LP must hold the shares
        if self.lp_shares[lp] < shares {
            return Err(Err::InsufficientFunds);
        }
        let mut pool = self.pool.clone();
        pool.share_supply = supply.checked_sub(shares).ok_or(Err::InsufficientFunds)?;
        // transfers from the vault
        pool.vault = pool.vault.checked_sub(net).ok_or(Err::InsufficientFunds)?;
        pool.vault = pool.vault.checked_sub(fee).ok_or(Err::InsufficientFunds)?;
        pool.tvl = pool.tvl.checked_sub(gross).ok_or(Err::Overflow)?;
        pool.rebase_utilisation()?;
        let mut global = self.global.clone();
        if fee > 0 {
            global.fee_vault += fee;
            let (b, t) = split(fee);
            global.buyback_accrued = global.buyback_accrued.checked_add(b).ok_or(Err::Overflow)?;
            global.treasury_accrued = global.treasury_accrued.checked_add(t).ok_or(Err::Overflow)?;
            global.buyback_lifetime = global.buyback_lifetime.checked_add(b).ok_or(Err::Overflow)?;
            global.treasury_lifetime = global.treasury_lifetime.checked_add(t).ok_or(Err::Overflow)?;
            pool.fees_lifetime = pool.fees_lifetime.checked_add(fee).ok_or(Err::Overflow)?;
            pool.event_seq = pool.event_seq.checked_add(1).ok_or(Err::Overflow)?;
        }
        pool.event_seq = pool.event_seq.checked_add(1).ok_or(Err::Overflow)?;
        pool.assert_invariants(pool.vault)?;
        self.pool = pool;
        self.global = global;
        self.lp_shares[lp] -= shares;
        self.lp_cash[lp] += i128::from(net);
        Ok((net, fee))
    }

    pub fn quote_fixed(&self, tenor: Tenor, leg: Leg, notional: u64) -> R<i32> {
        let (spot, ema) = self.bench.for_quote(self.slot)?;
        let q = vernier::quote(spot, ema, tenor, leg, notional, &self.pool.vernier(), &self.pool.params).map_err(|e| match e {
            vernier::VernierError::EmptyPool | vernier::VernierError::MalformedUtilisation => Err::PoolInvariant,
            // A spot quote never prices a forward, so the horizon error cannot arise here; it is mapped rather
            // than ignored so the model stays exhaustive over the engine's errors.
            vernier::VernierError::Overflow
            | vernier::VernierError::CorrelationOutOfRange
            | vernier::VernierError::ForwardHorizon => Err::Overflow,
        })?;
        Ok(q.fixed_bp)
    }

    // ---- trader_open_swap ----
    #[allow(clippy::too_many_arguments)]
    pub fn open(&mut self, trader: usize, id: u64, leg: Leg, tenor_ix: u8, notional: u64, limit_bp: u16) -> R<Swap> {
        // Atomicity: a failed instruction reverts `apply_pending` as well.
        let saved = self.pool.clone();
        let r = self.open_inner(trader, id, leg, tenor_ix, notional, limit_bp);
        if r.is_err() {
            self.pool = saved;
        }
        r
    }

    fn open_inner(&mut self, trader: usize, id: u64, leg: Leg, tenor_ix: u8, notional: u64, limit_bp: u16) -> R<Swap> {
        match self.global.mode {
            Mode::Halted => return Err(Err::Halted),
            Mode::WithdrawOnly => return Err(Err::WithdrawOnly),
            Mode::Limited => {
                if notional > self.global.limited_mode_cap {
                    return Err(Err::LimitedModeCap);
                }
            }
            Mode::Normal => {}
        }
        // Anchor `init` fails before the handler when the PDA already exists.
        if self.index.contains_key(&id) {
            return Err(Err::AlreadyInUse);
        }
        self.pool.apply_pending(self.slot);
        if notional < self.pool.min_notional {
            return Err(Err::NotionalTooSmall);
        }
        if notional > self.pool.max_notional {
            return Err(Err::NotionalTooLarge);
        }
        let tenor = tenor_from(tenor_ix)?;
        if self.pool.tvl == 0 {
            return Err(Err::PoolInvariant);
        }
        let fixed_i = self.quote_fixed(tenor, leg, notional)?;
        let fixed = u16::try_from(fixed_i).map_err(|_| Err::Overflow)?;
        match leg {
            Leg::Pay => {
                if fixed > limit_bp {
                    return Err(Err::LimitRate);
                }
            }
            Leg::Receive => {
                if fixed < limit_bp {
                    return Err(Err::LimitRate);
                }
            }
        }
        if notional > vernier::leg_capacity(&self.pool.vernier(), leg) {
            return Err(Err::LegCap);
        }
        let collateral = vernier::collateral(notional, tenor, &self.pool.params);
        let fee = mul_bp(notional, vernier::OPENING_FEE_BP)?;
        let mut pool = self.pool.clone();
        pool.vault = pool.vault.checked_add(collateral).ok_or(Err::Overflow)?;
        match leg {
            Leg::Pay => pool.open_pay_notional = pool.open_pay_notional.checked_add(notional).ok_or(Err::Overflow)?,
            Leg::Receive => pool.open_rec_notional = pool.open_rec_notional.checked_add(notional).ok_or(Err::Overflow)?,
        }
        pool.collateral_held = pool.collateral_held.checked_add(collateral).ok_or(Err::Overflow)?;
        pool.open_swaps = pool.open_swaps.checked_add(1).ok_or(Err::Overflow)?;
        pool.rebase_utilisation()?;
        let mut global = self.global.clone();
        if fee > 0 {
            global.fee_vault += fee;
            let (b, t) = split(fee);
            global.buyback_accrued = global.buyback_accrued.checked_add(b).ok_or(Err::Overflow)?;
            global.treasury_accrued = global.treasury_accrued.checked_add(t).ok_or(Err::Overflow)?;
            global.buyback_lifetime = global.buyback_lifetime.checked_add(b).ok_or(Err::Overflow)?;
            global.treasury_lifetime = global.treasury_lifetime.checked_add(t).ok_or(Err::Overflow)?;
            pool.fees_lifetime = pool.fees_lifetime.checked_add(fee).ok_or(Err::Overflow)?;
            pool.event_seq = pool.event_seq.checked_add(1).ok_or(Err::Overflow)?;
        }
        let matures_ts = self
            .ts
            .checked_add(i64::from(tenor.days()).checked_mul(SECONDS_PER_DAY).ok_or(Err::Overflow)?)
            .ok_or(Err::Overflow)?;
        let start = self.bench.accrual_at(self.ts)?;
        pool.event_seq = pool.event_seq.checked_add(1).ok_or(Err::Overflow)?;
        pool.assert_invariants(pool.vault)?;
        let s = Swap {
            id,
            trader: trader as u8,
            leg,
            tenor: tenor_ix,
            notional,
            fixed_bp: fixed,
            collateral,
            opened_slot: self.slot,
            opened_ts: self.ts,
            matures_ts,
            index_accrual_start: start,
        };
        self.pool = pool;
        self.global = global;
        self.trader_cash[trader] -= i128::from(collateral) + i128::from(fee);
        self.index.insert(id, self.swaps.len());
        self.swaps.push(s.clone());
        Ok(s)
    }

    /// `swap::mark`: accrued since open (benchmark average over the elapsed seconds against the fixed rate) plus
    /// the remaining seconds unwound at the opposite-leg quote, summed unbounded and clamped to `[-bound, bound]`.
    /// Returns `(clamped, unclamped)`.
    fn mark(&self, s: &Swap, bound: u64) -> R<(i64, i128)> {
        let tenor = tenor_from(s.tenor)?;
        let now = self.ts.min(s.matures_ts);
        let elapsed = now.checked_sub(s.opened_ts).ok_or(Err::Overflow)?.max(0);
        let remaining = s.matures_ts.checked_sub(now).ok_or(Err::Overflow)?.max(0);
        let accrued = if elapsed > 0 {
            let avg = average_bp(s.index_accrual_start, self.bench.accrual_at(now)?, elapsed)?;
            pnl_secs(oriented_diff(s.leg, avg, i64::from(s.fixed_bp))?, s.notional, elapsed)?
        } else {
            0
        };
        let forward = if remaining > 0 {
            let unwind = self.quote_fixed(tenor, opposite(s.leg), s.notional)?;
            pnl_secs(oriented_diff(s.leg, i64::from(unwind), i64::from(s.fixed_bp))?, s.notional, remaining)?
        } else {
            0
        };
        let total = accrued.checked_add(forward).ok_or(Err::Overflow)?;
        Ok((clamp_to_collateral(total, bound)?, total))
    }

    /// Model-only fair value for an early close: realised to now from the true (continuous) accrual plus the same
    /// forward unwind as the program, clamped to collateral. The difference to `mark` is therefore the error
    /// introduced by the publish cadence (held-rate accrual between publishes), not by the valuation method.
    fn fair_early(&self, s: &Swap, mark_unbounded: i128) -> i64 {
        let now = self.ts.min(s.matures_ts);
        let elapsed = (now - s.opened_ts).max(0);
        let accrued_true: i128 = if elapsed > 0 {
            let end = self.bench.true_accrual_at(now);
            let start = self.bench.true_accrual_at(s.opened_ts);
            let avg = average_bp(start.min(end), end, elapsed).unwrap_or(0);
            let diff = oriented_diff(s.leg, avg, i64::from(s.fixed_bp)).unwrap_or(0);
            pnl_secs(diff, s.notional, elapsed).unwrap_or(0)
        } else {
            0
        };
        let accrued_mark: i128 = if elapsed > 0 {
            let avg = average_bp(s.index_accrual_start, self.bench.accrual_at(now).unwrap_or(s.index_accrual_start), elapsed).unwrap_or(0);
            let diff = oriented_diff(s.leg, avg, i64::from(s.fixed_bp)).unwrap_or(0);
            pnl_secs(diff, s.notional, elapsed).unwrap_or(0)
        } else {
            0
        };
        let forward = mark_unbounded - accrued_mark;
        let c = i128::from(s.collateral);
        (accrued_true + forward).clamp(-c, c) as i64
    }

    /// Model-only fair settlement from the true accrual over exactly `[opened_ts, matures_ts]`.
    fn fair_settle(&self, s: &Swap) -> R<i64> {
        let tenor = tenor_from(s.tenor)?;
        let end = self.bench.true_accrual_at(s.matures_ts);
        let avg = average_bp(s.index_accrual_start.min(end), end, s.matures_ts - s.opened_ts)?;
        pnl_bounded(oriented_diff(s.leg, avg, i64::from(s.fixed_bp))?, s.notional, tenor.days(), s.collateral)
    }

    /// `swap::close`. `cranker`: `Some(k)` when a third party `k` supplied a bounty destination.
    fn close(&mut self, id: u64, pnl_in: i64, pnl_unbounded: i128, fair: i64, kind: CloseKind, bounty_eligible: bool, cranker: Option<u8>) -> R<CloseEffect> {
        let s = self.swap(id).ok_or(Err::NoSuchSwap)?.clone();
        let pool = &self.pool;
        let pnl = if pnl_in > 0 { pnl_in.min(i64::try_from(pool.tvl).unwrap_or(i64::MAX)) } else { pnl_in };
        let gain = u64::try_from(pnl.max(0)).map_err(|_| Err::Overflow)?;
        let loss = u64::try_from(pnl.min(0).checked_neg().ok_or(Err::Overflow)?).map_err(|_| Err::Overflow)?;
        let fee = u64::try_from(u128::from(gain) * u128::from(vernier::INCOME_FEE_PCT) / 100).map_err(|_| Err::Overflow)?;
        let gross = s
            .collateral
            .checked_add(gain)
            .ok_or(Err::Overflow)?
            .checked_sub(loss)
            .ok_or(Err::Overflow)?
            .checked_sub(fee)
            .ok_or(Err::Overflow)?;
        let third_party = matches!(cranker, Some(k) if k != s.trader);
        let bounty = if bounty_eligible && third_party { crank_bounty(s.notional)?.min(gross) } else { 0 };
        let payout = gross.checked_sub(bounty).ok_or(Err::Overflow)?;
        let mut pool = self.pool.clone();
        match s.leg {
            Leg::Pay => pool.open_pay_notional = pool.open_pay_notional.checked_sub(s.notional).ok_or(Err::Overflow)?,
            Leg::Receive => pool.open_rec_notional = pool.open_rec_notional.checked_sub(s.notional).ok_or(Err::Overflow)?,
        }
        pool.collateral_held = pool.collateral_held.checked_sub(s.collateral).ok_or(Err::Overflow)?;
        pool.open_swaps = pool.open_swaps.checked_sub(1).ok_or(Err::Overflow)?;
        pool.tvl = pool.tvl.checked_add(loss).ok_or(Err::Overflow)?.checked_sub(gain).ok_or(Err::Overflow)?;
        pool.rebase_utilisation()?;
        // transfers out of the vault, in program order: payout, bounty, fee
        pool.vault = pool.vault.checked_sub(payout).ok_or(Err::InsufficientFunds)?;
        pool.vault = pool.vault.checked_sub(bounty).ok_or(Err::InsufficientFunds)?;
        pool.vault = pool.vault.checked_sub(fee).ok_or(Err::InsufficientFunds)?;
        let mut global = self.global.clone();
        if fee > 0 {
            global.fee_vault += fee;
            let (b, t) = split(fee);
            global.buyback_accrued = global.buyback_accrued.checked_add(b).ok_or(Err::Overflow)?;
            global.treasury_accrued = global.treasury_accrued.checked_add(t).ok_or(Err::Overflow)?;
            global.buyback_lifetime = global.buyback_lifetime.checked_add(b).ok_or(Err::Overflow)?;
            global.treasury_lifetime = global.treasury_lifetime.checked_add(t).ok_or(Err::Overflow)?;
            pool.fees_lifetime = pool.fees_lifetime.checked_add(fee).ok_or(Err::Overflow)?;
            pool.event_seq = pool.event_seq.checked_add(1).ok_or(Err::Overflow)?;
        }
        pool.event_seq = pool.event_seq.checked_add(1).ok_or(Err::Overflow)?;
        pool.assert_invariants(pool.vault)?;
        self.pool = pool;
        self.global = global;
        self.trader_cash[usize::from(s.trader)] += i128::from(payout);
        if let Some(k) = cranker {
            if bounty > 0 {
                self.trader_cash[usize::from(k)] += i128::from(bounty);
            }
        }
        let removed = self.remove_swap(id);
        Ok(CloseEffect { kind, swap: removed, pnl, pnl_unbounded, payout, bounty, fee, fair_pnl: fair })
    }

    // ---- trader_cancel_swap ----
    pub fn cancel(&mut self, signer: u8, id: u64, min_payout: u64) -> R<CloseEffect> {
        // Atomicity: a failed instruction reverts `apply_pending` as well.
        let saved = self.pool.clone();
        let r = self.cancel_inner(signer, id, min_payout);
        if r.is_err() {
            self.pool = saved;
        }
        r
    }

    fn cancel_inner(&mut self, signer: u8, id: u64, min_payout: u64) -> R<CloseEffect> {
        let s = self.swap(id).ok_or(Err::NoSuchSwap)?.clone();
        if self.global.mode == Mode::Halted {
            return Err(Err::Halted);
        }
        if signer != s.trader {
            return Err(Err::ConstraintSigner);
        }
        self.pool.apply_pending(self.slot);
        let (pnl, unb) = self.mark(&s, s.collateral)?;
        let est = i64::try_from(s.collateral).map_err(|_| Err::Overflow)?.checked_add(pnl).ok_or(Err::Overflow)?;
        if est < i64::try_from(min_payout).map_err(|_| Err::Overflow)? {
            return Err(Err::Slippage);
        }
        let fair = self.fair_early(&s, unb);
        self.close(id, pnl, unb, fair, CloseKind::Cancelled, false, None)
    }

    // ---- crank_settle_swap ----
    pub fn settle(&mut self, id: u64, cranker: Option<u8>) -> R<CloseEffect> {
        // Atomicity: a failed instruction reverts `apply_pending` as well.
        let saved = self.pool.clone();
        let r = self.settle_inner(id, cranker);
        if r.is_err() {
            self.pool = saved;
        }
        r
    }

    fn settle_inner(&mut self, id: u64, cranker: Option<u8>) -> R<CloseEffect> {
        let s = self.swap(id).ok_or(Err::NoSuchSwap)?.clone();
        self.pool.apply_pending(self.slot);
        if self.ts < s.matures_ts {
            return Err(Err::NotMatured);
        }
        if !self.bench.published {
            return Err(Err::BenchmarkNotPublished);
        }
        let end = self.bench.accrual_at(s.matures_ts.min(self.ts))?;
        let avg = average_bp(s.index_accrual_start, end, s.matures_ts.checked_sub(s.opened_ts).ok_or(Err::Overflow)?)?;
        let tenor = tenor_from(s.tenor)?;
        let diff = oriented_diff(s.leg, avg, i64::from(s.fixed_bp))?;
        let pnl = pnl_bounded(diff, s.notional, tenor.days(), s.collateral)?;
        let mag = pnl_unbounded_mag(diff, s.notional, tenor.days()) as i128;
        let unb = if diff < 0 { -mag } else { mag };
        let fair = self.fair_settle(&s)?;
        self.close(id, pnl, unb, fair, CloseKind::Settled, true, cranker)
    }

    // ---- crank_liquidate_swap ----
    pub fn liquidate(&mut self, id: u64, cranker: Option<u8>) -> R<CloseEffect> {
        // Atomicity: a failed instruction reverts `apply_pending` as well.
        let saved = self.pool.clone();
        let r = self.liquidate_inner(id, cranker);
        if r.is_err() {
            self.pool = saved;
        }
        r
    }

    fn liquidate_inner(&mut self, id: u64, cranker: Option<u8>) -> R<CloseEffect> {
        let s = self.swap(id).ok_or(Err::NoSuchSwap)?.clone();
        self.pool.apply_pending(self.slot);
        if self.ts >= s.matures_ts {
            return Err(Err::AlreadySettled);
        }
        // Exhaustion is the only trigger (the pre-maturity window was removed, ADR-007).
        let (unbounded, unb_mag) = self.mark(&s, u64::MAX)?;
        let threshold = i64::try_from(mul_bp(s.collateral, LIQUIDATION_LOSS_BP)?).map_err(|_| Err::Overflow)?;
        let exhausted = unbounded <= threshold.checked_neg().ok_or(Err::Overflow)?;
        if !exhausted {
            return Err(Err::NotLiquidatable);
        }
        let pnl = clamp_to_collateral(i128::from(unbounded), s.collateral)?;
        let fair = self.fair_early(&s, unb_mag);
        self.close(id, pnl, unb_mag, fair, CloseKind::Liquidated, true, cranker)
    }

    /// Direct SPL transfer into the vault, bypassing the program.
    pub fn donate(&mut self, amount: u64) {
        self.pool.vault += amount;
        self.unsynced_donations += amount;
    }

    // ---- sync_vault ----
    pub fn sync_vault(&mut self) -> R<u64> {
        let accounted = self.pool.tvl.checked_add(self.pool.collateral_held).ok_or(Err::Overflow)?;
        let surplus = self.pool.vault.checked_sub(accounted).ok_or(Err::Conservation)?;
        if surplus == 0 {
            return Err(Err::NothingToSweep);
        }
        let mut pool = self.pool.clone();
        pool.tvl = pool.tvl.checked_add(surplus).ok_or(Err::Overflow)?;
        pool.rebase_utilisation()?;
        pool.event_seq = pool.event_seq.checked_add(1).ok_or(Err::Overflow)?;
        pool.assert_invariants(pool.vault)?;
        self.pool = pool;
        self.unsynced_donations = 0;
        Ok(surplus)
    }

    // ---- sweep_fees ----
    pub fn sweep(&mut self) -> R<(u64, u64)> {
        let g = &mut self.global;
        let (b, t) = (g.buyback_accrued, g.treasury_accrued);
        if !(b > 0 || t > 0) {
            return Err(Err::NothingToSweep);
        }
        if g.fee_vault < b.checked_add(t).ok_or(Err::Overflow)? {
            return Err(Err::InsufficientFunds);
        }
        g.buyback_accrued = 0;
        g.treasury_accrued = 0;
        g.fee_vault -= b + t;
        g.buyback_balance += b;
        g.treasury_balance += t;
        Ok((b, t))
    }

    // ---- admin_set_mode ----
    pub fn set_mode(&mut self, by: Actor, mode: Mode) -> R<()> {
        let g = &mut self.global;
        match by {
            Actor::Authority => g.mode = mode,
            Actor::Guardian => {
                if mode.severity() <= g.mode.severity() {
                    return Err(Err::GuardianScope);
                }
                if mode.severity() > Mode::WithdrawOnly.severity() {
                    return Err(Err::GuardianScope);
                }
                g.mode = mode;
            }
            Actor::Stranger => return Err(Err::ConstraintSigner),
        }
        Ok(())
    }

    // ---- admin_queue_calibration ----
    pub fn queue_calibration(&mut self, p: Params) -> R<u64> {
        let cur = self.pool.params;
        let tables = [(cur.model_pay_bp, p.model_pay_bp), (cur.model_rec_bp, p.model_rec_bp), (cur.term_bp, p.term_bp), (cur.collateral_bp, p.collateral_bp)];
        for (old, new) in &tables {
            if !old.iter().zip(new.iter()).all(|(o, n)| within_step(*o, *n)) {
                return Err(Err::CalibrationStep);
            }
        }
        if !(within_step(cur.demand_k_bp, p.demand_k_bp) && within_step(cur.demand_cap_bp, p.demand_cap_bp)) {
            return Err(Err::CalibrationStep);
        }
        if !p.collateral_bp.iter().all(|c| *c > 0) {
            return Err(Err::CalibrationStep);
        }
        let effective = self.slot.checked_add(self.global.param_delay_slots).ok_or(Err::Overflow)?;
        self.pool.pending_params = p;
        self.pool.pending_effective_slot = effective;
        Ok(effective)
    }

    /// Sum of open notional per leg and collateral from the swap list (for the accounting invariant).
    #[must_use]
    pub fn sums(&self) -> (u64, u64, u64, u32) {
        let mut pay = 0u64;
        let mut rec = 0u64;
        let mut col = 0u64;
        for s in &self.swaps {
            match s.leg {
                Leg::Pay => pay += s.notional,
                Leg::Receive => rec += s.notional,
            }
            col += s.collateral;
        }
        (pay, rec, col, self.swaps.len() as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> Setup {
        Setup {
            band_bp: 300,
            max_staleness_slots: 2_000,
            half_life_slots: 10_000,
            min_interval_slots: 0,
            params: DEFAULT_PARAMS,
            param_delay_slots: 432_000,
            limited_mode_cap: 100_000 * USDC,
            min_notional: 1_000 * USDC,
            max_notional: 50_000_000 * USDC,
            n_lps: 2,
            n_traders: 2,
            start_slot: 1_000,
            start_ts: 1_790_000_000,
        }
    }

    /// Reproduces `lifecycle_deposit_open_settle_with_income_fee_and_sweep` from tests/svm/tests/e2e.rs.
    #[test]
    fn lifecycle_matches_e2e() {
        let mut m = Model::new(&setup());
        m.publish(684).unwrap();
        m.deposit(0, 10_000_000 * USDC, 10_000_000 * USDC).unwrap();
        assert_eq!(m.pool.share_supply, 10_000_000 * USDC);
        let s = m.open(0, 1, Leg::Pay, 2, 1_000_000 * USDC, 727).unwrap();
        assert_eq!(s.fixed_bp, 727);
        assert_eq!(s.collateral, 33_000 * USDC);
        assert_eq!(m.pool.util_pay_bp, 1_000);
        assert_eq!(m.global.fee_vault, 500 * USDC);
        assert_eq!(m.global.buyback_accrued, 250 * USDC);
        m.warp(45 * SLOTS_PER_DAY, 45 * SECONDS_PER_DAY);
        m.publish(900).unwrap();
        m.warp(45 * SLOTS_PER_DAY, 45 * SECONDS_PER_DAY);
        m.publish(900).unwrap();
        m.ts -= 1;
        assert_eq!(m.settle(1, Some(1)).unwrap_err(), Err::NotMatured);
        m.ts += 1;
        let e = m.settle(1, Some(1)).unwrap();
        let pnl = 65u128 * 1_000_000 * u128::from(USDC) * 90 / (10_000 * 365);
        assert_eq!(u128::from(e.payout), 33_000 * u128::from(USDC) + pnl - pnl / 10 - 25 * u128::from(USDC));
        assert_eq!(e.bounty, 25 * USDC);
        assert_eq!(u128::from(m.pool.tvl), 10_000_000 * u128::from(USDC) - pnl);
        assert_eq!(m.pool.vault, m.pool.tvl);
        let (b, t) = m.sweep().unwrap();
        assert_eq!(u128::from(b + t), 500 * u128::from(USDC) + pnl / 10);
        assert_eq!(m.sweep().unwrap_err(), Err::NothingToSweep);
        let shares = m.pool.share_supply;
        m.withdraw(0, shares, 0).unwrap();
        assert_eq!(m.pool.tvl, 0);
        assert_eq!(m.pool.vault, 0);
    }

    #[test]
    fn limit_and_caps_match_e2e() {
        let mut m = Model::new(&setup());
        m.publish(684).unwrap();
        m.deposit(0, 1_000_000 * USDC, 0).unwrap();
        assert_eq!(m.open(0, 1, Leg::Pay, 2, 100_000 * USDC, 700).unwrap_err(), Err::LimitRate);
        assert_eq!(m.open(0, 2, Leg::Receive, 2, 100_000 * USDC, 659).unwrap_err(), Err::LimitRate);
        m.open(0, 2, Leg::Receive, 2, 100_000 * USDC, 658).unwrap();
        assert_eq!(m.open(0, 3, Leg::Receive, 0, 400_000 * USDC, 0).unwrap_err(), Err::LegCap);
        assert_eq!(m.open(0, 4, Leg::Pay, 4, 10_000 * USDC, 9_999).unwrap_err(), Err::Tenor);
        assert_eq!(m.open(0, 5, Leg::Pay, 0, 10 * USDC, 9_999).unwrap_err(), Err::NotionalTooSmall);
        let shares = m.pool.share_supply;
        assert_eq!(m.withdraw(0, shares * 9 / 10, 0).unwrap_err(), Err::PoolInvariant);
        let (net, fee) = m.withdraw(0, 100_000 * USDC, 0).unwrap();
        assert_eq!(net, 100_000 * USDC - 500 * USDC);
        assert_eq!(fee, 500 * USDC);
        assert_eq!(m.global.buyback_accrued + m.global.treasury_accrued, 550 * USDC);
    }

    #[test]
    fn settlement_clamps_both_legs() {
        let mut m = Model::new(&setup());
        m.publish(684).unwrap();
        m.deposit(0, 10_000_000 * USDC, 0).unwrap();
        let n = 100_000 * USDC;
        m.open(0, 1, Leg::Pay, 0, n, 9_999).unwrap();
        m.open(0, 2, Leg::Receive, 0, n, 0).unwrap();
        let tvl0 = m.pool.tvl;
        for _ in 0..8 {
            m.warp(SLOTS_PER_DAY, SECONDS_PER_DAY);
            let e = m.bench.ema_bp;
            m.publish(e + 300).unwrap();
        }
        m.warp(20 * SLOTS_PER_DAY, 20 * SECONDS_PER_DAY);
        let v = m.bench.value_bp;
        m.publish(v).unwrap();
        let a = m.settle(1, None).unwrap();
        let b = m.settle(2, None).unwrap();
        let c = 1_200 * USDC;
        assert_eq!(a.payout + b.payout, c + c - c / 10);
        assert_eq!(m.pool.tvl, tvl0);
        assert_eq!(m.pool.open_swaps, 0);
    }
}
