//! Invariant checker and metrics. Checks run after every action; a failure records the seed, tick, action index
//! and a description so that the path can be replayed with `brink_sim_fast replay --seed S`.

use std::collections::BTreeMap;

use crate::model::{CloseEffect, CloseKind, Err, Model, Swap};
use crate::scenario::Action;

/// Every invariant the harness asserts. Numbering is stable and used in the report.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Inv {
    /// I1: no payment out of the vault or the fee vault ever fails (cash never negative).
    CashNonNegative,
    /// I2: vault == tvl + collateral_held + unsynced donations (exact conservation).
    Conservation,
    /// I3: per-leg utilisation <= 48 percent, total <= 80 percent whenever tvl > 0.
    UtilisationCaps,
    /// I4: stored utilisation equals floor(open notional * 10_000 / tvl).
    UtilisationRebase,
    /// I5: open notional per leg, collateral held and open count equal the sums over open swaps.
    OpenAccounting,
    /// I6: fee vault == buyback accrued + treasury accrued; lifetimes sum to the pool's fee lifetime; every booked
    /// fee splits to the unit with the remainder to the treasury.
    FeeSplit,
    /// I7: share supply == sum of LP holdings.
    ShareSupply,
    /// I8: a settle attempt on a matured swap with a published benchmark never fails (liveness).
    SettlementLive,
    /// I9: trader payout + bounty + income fee <= collateral + min(collateral, tvl before): no trader is paid more
    /// than collateral plus the capped gain.
    PayoutBound,
    /// I10: LP capital falls by at most the closed swap's collateral per close.
    LpLossBound,
    /// I11: a queued calibration never remains pending past its effective slot after a pool-touching action.
    PendingApplies,
    /// I12: the pool event sequence strictly increases on every accepted pool-touching action.
    EventSeq,
    /// I13: no deposit is accepted while tvl == 0 and share supply > 0 (dead shares would capture the deposit).
    NoDeadShareDilution,
    /// I14: a rejected action leaves the model state unchanged (atomicity of the mirror).
    RejectedIsNoop,
    /// I15: every quote used for an open sits within model + demand cap + term of its reference and fits u16.
    QuoteBounded,
}

impl Inv {
    pub const ALL: [Inv; 15] = [
        Inv::CashNonNegative,
        Inv::Conservation,
        Inv::UtilisationCaps,
        Inv::UtilisationRebase,
        Inv::OpenAccounting,
        Inv::FeeSplit,
        Inv::ShareSupply,
        Inv::SettlementLive,
        Inv::PayoutBound,
        Inv::LpLossBound,
        Inv::PendingApplies,
        Inv::EventSeq,
        Inv::NoDeadShareDilution,
        Inv::RejectedIsNoop,
        Inv::QuoteBounded,
    ];
    #[must_use]
    pub fn id(self) -> &'static str {
        match self {
            Inv::CashNonNegative => "I1",
            Inv::Conservation => "I2",
            Inv::UtilisationCaps => "I3",
            Inv::UtilisationRebase => "I4",
            Inv::OpenAccounting => "I5",
            Inv::FeeSplit => "I6",
            Inv::ShareSupply => "I7",
            Inv::SettlementLive => "I8",
            Inv::PayoutBound => "I9",
            Inv::LpLossBound => "I10",
            Inv::PendingApplies => "I11",
            Inv::EventSeq => "I12",
            Inv::NoDeadShareDilution => "I13",
            Inv::RejectedIsNoop => "I14",
            Inv::QuoteBounded => "I15",
        }
    }
    #[must_use]
    pub fn describe(self) -> &'static str {
        match self {
            Inv::CashNonNegative => "cash never negative: every vault and fee-vault payment succeeds",
            Inv::Conservation => "vault == tvl + collateral_held + unsynced donations",
            Inv::UtilisationCaps => "per-leg utilisation <= 48 pct and total <= 80 pct while tvl > 0",
            Inv::UtilisationRebase => "stored utilisation == floor(open notional * 1e4 / tvl)",
            Inv::OpenAccounting => "open notional, collateral held and open count equal the sums over open swaps",
            Inv::FeeSplit => "fee vault == accrued halves; lifetimes sum to fees; each split conserves to the unit",
            Inv::ShareSupply => "share supply == sum of LP holdings",
            Inv::SettlementLive => "a matured swap with a published benchmark always settles when cranked",
            Inv::PayoutBound => "payout + bounty + fee <= collateral + min(collateral, tvl before)",
            Inv::LpLossBound => "LP capital falls by at most the closed swap's collateral per close",
            Inv::PendingApplies => "a due calibration is applied by the next pool-touching action",
            Inv::EventSeq => "pool event sequence strictly increases on accepted pool actions",
            Inv::NoDeadShareDilution => "no deposit accepted while tvl == 0 and share supply > 0",
            Inv::RejectedIsNoop => "a rejected action leaves state unchanged",
            Inv::QuoteBounded => "open quote within reference +/- (model + demand cap + term) and fits u16",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Violation {
    pub inv: Inv,
    pub seed: u64,
    pub tick: u64,
    pub action_index: u64,
    pub action: String,
    pub detail: String,
}

/// Outcome of applying one action to the model.
#[derive(Clone, Debug)]
pub enum Effect {
    Warped,
    Published,
    Deposited { shares: u64 },
    Withdrawn { net: u64, fee: u64 },
    Opened(Swap),
    Closed(CloseEffect),
    Donated,
    Synced { surplus: u64 },
    Swept { buyback: u64, treasury: u64 },
    ModeSet,
    Calibrated { effective_slot: u64 },
}

pub type Outcome = Result<Effect, Err>;

/// Snapshot of the cheap-to-compare state, for the atomicity check.
#[derive(Clone, PartialEq, Eq, Debug)]
struct Snap {
    pool: crate::model::Pool,
    global: crate::model::Global,
    bench_value: u16,
    bench_acc: u128,
    bench_slot: u64,
    n_swaps: usize,
    supply_sum: u64,
}

fn snap(m: &Model) -> Snap {
    Snap {
        pool: m.pool.clone(),
        global: m.global.clone(),
        bench_value: m.bench.value_bp,
        bench_acc: m.bench.accrual_e18,
        bench_slot: m.bench.slot,
        n_swaps: m.swaps.len(),
        supply_sum: m.lp_shares.iter().sum(),
    }
}

/// Per-seed statistics. Merged across seeds by `Totals`.
#[derive(Clone, Debug, Default)]
pub struct SeedStats {
    pub seed: u64,
    pub attempted: u64,
    pub accepted: u64,
    pub by_kind_attempted: BTreeMap<&'static str, u64>,
    pub by_kind_accepted: BTreeMap<&'static str, u64>,
    pub by_error: BTreeMap<String, u64>,
    pub inv_pass: BTreeMap<Inv, u64>,
    pub inv_fail: BTreeMap<Inv, u64>,
    pub violations: Vec<Violation>,
    pub quotes_evaluated: u64,
    // economics
    pub initial_tvl: u64,
    pub share_price_min_e6: u64,
    pub share_price_peak_e6: u64,
    pub share_price_final_e6: u64,
    pub max_drawdown_bp: u64,
    pub gains_paid_from_tvl: u128,
    pub losses_booked_to_tvl: u128,
    pub fees_total: u128,
    pub max_single_gain_bp_of_tvl: u64,
    pub max_collateral_shortfall: u128,
    pub max_collateral_shortfall_bp_notional: u64,
    /// Shortfall on trader losses (the LP absorbs the excess beyond collateral): count and sum.
    pub lp_absorbed_shortfall_closes: u64,
    pub lp_absorbed_shortfall_sum: u128,
    /// Shortfall on trader gains (the trader's gain is clamped to collateral): count and sum.
    pub trader_clamped_gain_closes: u64,
    pub trader_clamped_gain_sum: u128,
    /// Deposits made while share supply was zero but tvl positive (dead capital captured by the depositor).
    pub dead_capital_captures: u64,
    pub dead_capital_captured_sum: u128,
    pub clamped_closes: u64,
    pub closes: u64,
    pub max_util_pay: u16,
    pub max_util_rec: u16,
    pub max_util_total: u32,
    pub stale_quote_rejections: u64,
    /// Sum over early closes (cancel, liquidate) of booked pnl minus fair pnl (positive: trader overpaid).
    pub early_close_transfer: i128,
    pub early_close_transfer_abs_max: u128,
    pub early_closes: u64,
    pub near_maturity_liquidations: u64,
    /// Sum over settlements of booked pnl minus fair pnl (positive: trader overpaid by extrapolation).
    pub settle_transfer: i128,
    pub settle_transfer_abs_max: u128,
    pub settle_mismatches: u64,
    pub settlements: u64,
    pub tvl_hit_zero: bool,
    pub withdraw_cap_rejections: u64,
}

impl SeedStats {
    #[must_use]
    pub fn new(seed: u64) -> Self {
        let mut s = Self { seed, share_price_min_e6: u64::MAX, ..Default::default() };
        for i in Inv::ALL {
            s.inv_pass.insert(i, 0);
            s.inv_fail.insert(i, 0);
        }
        s
    }
    fn pass(&mut self, i: Inv) {
        *self.inv_pass.entry(i).or_default() += 1;
    }
    fn fail(&mut self, i: Inv, tick: u64, idx: u64, a: &Action, detail: String) {
        *self.inv_fail.entry(i).or_default() += 1;
        if self.violations.len() < 8 {
            self.violations.push(Violation { inv: i, seed: self.seed, tick, action_index: idx, action: format!("{a:?}"), detail });
        }
    }
    fn check(&mut self, i: Inv, ok: bool, tick: u64, idx: u64, a: &Action, detail: impl FnOnce() -> String) {
        if ok {
            self.pass(i);
        } else {
            self.fail(i, tick, idx, a, detail());
        }
    }
}

/// Applies one action to the model, records the outcome and runs the checks.
pub fn apply_and_check(m: &mut Model, a: &Action, st: &mut SeedStats, tick: u64, idx: u64) -> Outcome {
    let before = snap(m);
    let tvl_before = m.pool.tvl;
    let tvl_before_action = m.pool.tvl;
    let supply_before = m.pool.share_supply;
    let seq_before = m.pool.event_seq;
    let touches_pool = !matches!(a, Action::Warp { .. } | Action::Publish { .. } | Action::Sweep | Action::SetMode { .. } | Action::Donate { .. });
    let out: Outcome = match a {
        Action::Warp { slots, secs } => {
            m.warp(*slots, *secs);
            Ok(Effect::Warped)
        }
        Action::Publish { value_bp } => m.publish(*value_bp).map(|_| Effect::Published),
        Action::Deposit { lp, amount, min_shares } => m.deposit(usize::from(*lp), *amount, *min_shares).map(|shares| Effect::Deposited { shares }),
        Action::Withdraw { lp, shares, min_amount } => m.withdraw(usize::from(*lp), *shares, *min_amount).map(|(net, fee)| Effect::Withdrawn { net, fee }),
        Action::Open { trader, id, leg, tenor, notional, limit_bp } => {
            st.quotes_evaluated += 1;
            m.open(usize::from(*trader), *id, *leg, *tenor, *notional, *limit_bp).map(Effect::Opened)
        }
        Action::Cancel { signer, id, min_payout } => {
            st.quotes_evaluated += 1;
            m.cancel(*signer, *id, *min_payout).map(Effect::Closed)
        }
        Action::Settle { id, cranker } => m.settle(*id, *cranker).map(Effect::Closed),
        Action::Liquidate { id, cranker } => {
            st.quotes_evaluated += 1;
            m.liquidate(*id, *cranker).map(Effect::Closed)
        }
        Action::Donate { amount } => {
            m.donate(*amount);
            Ok(Effect::Donated)
        }
        Action::SyncVault => m.sync_vault().map(|surplus| Effect::Synced { surplus }),
        Action::Sweep => m.sweep().map(|(buyback, treasury)| Effect::Swept { buyback, treasury }),
        Action::SetMode { by, mode } => m.set_mode(*by, *mode).map(|_| Effect::ModeSet),
        Action::QueueCalibration { params } => m.queue_calibration(*params).map(|effective_slot| Effect::Calibrated { effective_slot }),
    };

    if !matches!(a, Action::Warp { .. }) {
        st.attempted += 1;
        *st.by_kind_attempted.entry(a.kind()).or_default() += 1;
    }
    match &out {
        Ok(_) => {
            if !matches!(a, Action::Warp { .. }) {
                st.accepted += 1;
                *st.by_kind_accepted.entry(a.kind()).or_default() += 1;
            }
        }
        Err(e) => {
            *st.by_error.entry(format!("{}:{}", a.kind(), e.name())).or_default() += 1;
            if matches!(e, Err::BenchmarkStale) {
                st.stale_quote_rejections += 1;
            }
            if matches!(a, Action::Withdraw { .. }) && matches!(e, Err::PoolInvariant) {
                st.withdraw_cap_rejections += 1;
            }
            // I14 atomicity: a rejected action must not change state. (Warp always succeeds.)
            let after = snap(m);
            st.check(Inv::RejectedIsNoop, before == after, tick, idx, a, || "state changed after a rejected action".to_string());
            // I1: an unexpected insufficient-funds on a vault-side payment.
            let vault_payment = matches!(a, Action::Settle { .. } | Action::Liquidate { .. } | Action::Cancel { .. } | Action::Sweep);
            if vault_payment {
                st.check(Inv::CashNonNegative, !matches!(e, Err::InsufficientFunds), tick, idx, a, || format!("payment failed: {e:?}"));
            }
            // I8 liveness: a matured swap with a published benchmark must settle.
            if let Action::Settle { id, .. } = a {
                if let Some(s) = m.swap(*id) {
                    if m.ts >= s.matures_ts && m.bench.published {
                        st.fail(Inv::SettlementLive, tick, idx, a, format!("settle of matured swap rejected with {e:?}; swap {s:?}"));
                    }
                }
            }
            return out;
        }
    }

    // ---- state invariants after an accepted action ----
    let p = &m.pool;
    st.check(Inv::Conservation, p.vault == p.tvl + p.collateral_held + m.unsynced_donations, tick, idx, a, || {
        format!("vault {} tvl {} collateral {} donations {}", p.vault, p.tvl, p.collateral_held, m.unsynced_donations)
    });
    let caps_ok = p.tvl == 0 || (p.util_pay_bp <= 4_800 && p.util_rec_bp <= 4_800 && u32::from(p.util_pay_bp) + u32::from(p.util_rec_bp) <= 8_000);
    st.check(Inv::UtilisationCaps, caps_ok, tick, idx, a, || format!("util pay {} rec {} tvl {}", p.util_pay_bp, p.util_rec_bp, p.tvl));
    let rebase_ok = if p.tvl == 0 {
        p.util_pay_bp == 0 && p.util_rec_bp == 0
    } else {
        u128::from(p.util_pay_bp) == u128::from(p.open_pay_notional) * 10_000 / u128::from(p.tvl) && u128::from(p.util_rec_bp) == u128::from(p.open_rec_notional) * 10_000 / u128::from(p.tvl)
    };
    st.check(Inv::UtilisationRebase, rebase_ok, tick, idx, a, || format!("util pay {} rec {} open pay {} rec {} tvl {}", p.util_pay_bp, p.util_rec_bp, p.open_pay_notional, p.open_rec_notional, p.tvl));
    let (sp, sr, sc, n) = m.sums();
    st.check(Inv::OpenAccounting, sp == p.open_pay_notional && sr == p.open_rec_notional && sc == p.collateral_held && n == p.open_swaps, tick, idx, a, || {
        format!("sums pay {sp} rec {sr} col {sc} n {n} vs pool {} {} {} {}", p.open_pay_notional, p.open_rec_notional, p.collateral_held, p.open_swaps)
    });
    let g = &m.global;
    st.check(Inv::FeeSplit, g.fee_vault == g.buyback_accrued + g.treasury_accrued && g.buyback_lifetime + g.treasury_lifetime == p.fees_lifetime && g.treasury_lifetime >= g.buyback_lifetime, tick, idx, a, || {
        format!("fee vault {} accrued {} + {} lifetimes {} + {} fees {}", g.fee_vault, g.buyback_accrued, g.treasury_accrued, g.buyback_lifetime, g.treasury_lifetime, p.fees_lifetime)
    });
    let supply_sum: u64 = m.lp_shares.iter().sum();
    st.check(Inv::ShareSupply, supply_sum == p.share_supply, tick, idx, a, || format!("supply {} holdings {}", p.share_supply, supply_sum));
    if touches_pool {
        // `sync_vault` and `admin_queue_calibration` do not call `apply_pending` on chain.
        if !matches!(a, Action::QueueCalibration { .. } | Action::SyncVault) {
            st.check(Inv::PendingApplies, p.pending_effective_slot == 0 || m.slot < p.pending_effective_slot, tick, idx, a, || format!("pending slot {} now {}", p.pending_effective_slot, m.slot));
        }
        if !matches!(a, Action::QueueCalibration { .. }) {
            st.check(Inv::EventSeq, p.event_seq > seq_before, tick, idx, a, || format!("seq {} -> {}", seq_before, p.event_seq));
        }
    }

    // ---- effect-specific checks and metrics ----
    match &out {
        Ok(Effect::Deposited { .. }) => {
            st.check(Inv::NoDeadShareDilution, !(before.pool.tvl == 0 && before.pool.share_supply > 0), tick, idx, a, || {
                format!("deposit accepted with tvl 0 and supply {}", before.pool.share_supply)
            });
        }
        Ok(Effect::Opened(s)) => {
            let params = &m.pool.params;
            let t = usize::from(s.tenor);
            let (spot, ema) = (m.bench.value_bp, m.bench.ema_bp);
            let (reference, model) = match s.leg {
                vernier::Leg::Pay => (spot.max(ema), params.model_pay_bp[t]),
                vernier::Leg::Receive => (spot.min(ema), params.model_rec_bp[t]),
            };
            let bound = i32::from(model) + i32::from(params.demand_cap_bp) + i32::from(params.term_bp[t]);
            let within = (i32::from(s.fixed_bp) - i32::from(reference)).abs() <= bound;
            let side_ok = match s.leg {
                vernier::Leg::Pay => s.fixed_bp >= reference,
                vernier::Leg::Receive => s.fixed_bp <= reference,
            };
            st.check(Inv::QuoteBounded, within && side_ok, tick, idx, a, || format!("fixed {} reference {reference} bound {bound}", s.fixed_bp));
        }
        Ok(Effect::Closed(c)) => {
            st.closes += 1;
            st.pass(Inv::CashNonNegative);
            if c.kind == CloseKind::Settled {
                st.pass(Inv::SettlementLive);
            }
            let gain = u64::try_from(c.pnl.max(0)).unwrap_or(0);
            let loss = u64::try_from((-c.pnl).max(0)).unwrap_or(0);
            let cap = c.swap.collateral + c.swap.collateral.min(tvl_before);
            st.check(Inv::PayoutBound, c.payout + c.bounty + c.fee <= cap, tick, idx, a, || format!("payout {} bounty {} fee {} cap {cap}", c.payout, c.bounty, c.fee));
            st.check(Inv::LpLossBound, m.pool.tvl + c.swap.collateral >= tvl_before, tick, idx, a, || format!("tvl {} -> {} collateral {}", tvl_before, m.pool.tvl, c.swap.collateral));
            st.check(Inv::FeeSplit, c.fee == gain * u64::from(vernier::INCOME_FEE_PCT) / 100, tick, idx, a, || format!("income fee {} on gain {gain}", c.fee));
            st.gains_paid_from_tvl += u128::from(gain);
            st.losses_booked_to_tvl += u128::from(loss);
            st.fees_total += u128::from(c.fee);
            if tvl_before > 0 {
                let bp = u64::try_from(u128::from(gain) * 10_000 / u128::from(tvl_before)).unwrap_or(u64::MAX);
                st.max_single_gain_bp_of_tvl = st.max_single_gain_bp_of_tvl.max(bp);
            }
            let unb = c.pnl_unbounded.unsigned_abs();
            if unb > u128::from(c.swap.collateral) {
                st.clamped_closes += 1;
                let short = unb - u128::from(c.swap.collateral);
                if c.pnl_unbounded < 0 {
                    st.lp_absorbed_shortfall_closes += 1;
                    st.lp_absorbed_shortfall_sum += short;
                } else {
                    st.trader_clamped_gain_closes += 1;
                    st.trader_clamped_gain_sum += short;
                }
                if short > st.max_collateral_shortfall {
                    st.max_collateral_shortfall = short;
                    st.max_collateral_shortfall_bp_notional = u64::try_from(short * 10_000 / u128::from(c.swap.notional.max(1))).unwrap_or(u64::MAX);
                }
            }
            let transfer = i128::from(c.pnl) - i128::from(c.fair_pnl);
            match c.kind {
                CloseKind::Settled => {
                    st.settlements += 1;
                    st.settle_transfer += transfer;
                    st.settle_transfer_abs_max = st.settle_transfer_abs_max.max(transfer.unsigned_abs());
                    if transfer != 0 {
                        st.settle_mismatches += 1;
                    }
                }
                CloseKind::Cancelled | CloseKind::Liquidated => {
                    st.early_closes += 1;
                    st.early_close_transfer += transfer;
                    st.early_close_transfer_abs_max = st.early_close_transfer_abs_max.max(transfer.unsigned_abs());
                    if c.kind == CloseKind::Liquidated && m.ts >= c.swap.matures_ts - crate::model::LIQUIDATION_WINDOW_SECS {
                        st.near_maturity_liquidations += 1;
                    }
                }
            }
        }
        Ok(Effect::Withdrawn { fee, .. }) => {
            st.fees_total += u128::from(*fee);
        }
        Ok(Effect::Swept { buyback, treasury }) => {
            st.pass(Inv::CashNonNegative);
            st.check(Inv::FeeSplit, *treasury == before.global.treasury_accrued && *buyback == before.global.buyback_accrued && m.global.fee_vault == 0, tick, idx, a, || format!("sweep {buyback} {treasury}"));
        }
        _ => {}
    }
    if let (Action::Deposit { .. }, Ok(Effect::Deposited { .. })) = (a, &out) {
        if supply_before == 0 && tvl_before_action > 0 {
            st.dead_capital_captures += 1;
            st.dead_capital_captured_sum += u128::from(tvl_before_action);
        }
    }
    if m.pool.share_supply == 0 {
        // every LP has left: a new share-price epoch starts with the next deposit
        st.share_price_peak_e6 = 0;
    }
    if let Ok(Effect::Opened(_)) = &out {
        st.fees_total += u128::from(crate::model::mul_bp(match a { Action::Open { notional, .. } => *notional, _ => 0 }, vernier::OPENING_FEE_BP).unwrap_or(0));
    }

    // share price path
    // Share price path, ignored below 1,000 USDC of capital where unit rounding dominates.
    let price = crate::model::share_price_e6(m.pool.tvl, m.pool.share_supply);
    if m.pool.share_supply > 0 && m.pool.tvl >= 1_000 * crate::model::USDC {
        st.share_price_min_e6 = st.share_price_min_e6.min(price);
        if price > st.share_price_peak_e6 {
            st.share_price_peak_e6 = price;
        } else if st.share_price_peak_e6 > 0 {
            let dd = (st.share_price_peak_e6 - price) as u128 * 10_000 / st.share_price_peak_e6 as u128;
            st.max_drawdown_bp = st.max_drawdown_bp.max(dd as u64);
        }
        st.share_price_final_e6 = price;
    }
    if m.pool.tvl == 0 && m.pool.share_supply > 0 {
        st.tvl_hit_zero = true;
    }
    st.max_util_pay = st.max_util_pay.max(m.pool.util_pay_bp);
    st.max_util_rec = st.max_util_rec.max(m.pool.util_rec_bp);
    st.max_util_total = st.max_util_total.max(u32::from(m.pool.util_pay_bp) + u32::from(m.pool.util_rec_bp));
    out
}
