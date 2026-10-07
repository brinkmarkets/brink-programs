//! Property tests (host only) for the pure arithmetic the handlers rely on: fee split conservation, bounded
//! and overflow-free behaviour at the maximum notional, monotonicity of the demand spread in imbalance, and the
//! direction of every rounding step. These run under `cargo test -p swap_amm`; they are not compiled into the
//! program.
#![cfg(test)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation
)]

use super::{
    admin,
    fees::{split, BUYBACK_BP},
    math::*,
    swap::{crank_bounty, CRANK_BOUNTY_BP, CRANK_BOUNTY_CAP},
};
use crate::state::*;
use anchor_lang::prelude::Pubkey;
use proptest::prelude::*;

/// An empty pool with the default calibration, for utilisation properties.
fn blank_pool(tvl: u64) -> Pool {
    let params = VernierParamsOnChain {
        model_pay_bp: vernier::DEFAULT_PARAMS.model_pay_bp,
        model_rec_bp: vernier::DEFAULT_PARAMS.model_rec_bp,
        term_bp: vernier::DEFAULT_PARAMS.term_bp,
        demand_k_bp: vernier::DEFAULT_PARAMS.demand_k_bp,
        demand_cap_bp: vernier::DEFAULT_PARAMS.demand_cap_bp,
        collateral_bp: vernier::DEFAULT_PARAMS.collateral_bp,
    };
    Pool {
        benchmark: Pubkey::default(),
        share_mint: Pubkey::default(),
        vault: Pubkey::default(),
        hook_program: Pubkey::default(),
        hooks: HookFlags::default(),
        pricer: Pricer::Vernier,
        params,
        pending_params: params,
        pending_effective_slot: 0,
        tvl,
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
        reserve_placed: 0,
        reserve_active: 0,
        _reserved: [0; 15],
    }
}

/// Largest notional any pool can accept: `max_notional` is a `u64`, so the arithmetic must be total on `u64`.
const MAX_NOTIONAL: u64 = u64::MAX;
/// Largest rate difference the index can produce: ceiling 30 000 bp either way.
const MAX_DIFF_BP: i64 = brink_index::MAX_RATE_BP as i64;

fn arb_leg() -> impl Strategy<Value = LegKind> {
    prop_oneof![Just(LegKind::PayFixed), Just(LegKind::ReceiveFixed)]
}
fn arb_tenor() -> impl Strategy<Value = vernier::Tenor> {
    prop_oneof![
        Just(vernier::Tenor::D28),
        Just(vernier::Tenor::D60),
        Just(vernier::Tenor::D90),
        Just(vernier::Tenor::D180)
    ]
}
fn arb_params() -> impl Strategy<Value = vernier::Params> {
    (
        proptest::array::uniform4(0u16..=2_000),
        proptest::array::uniform4(0u16..=2_000),
        proptest::array::uniform4(0u16..=2_000),
        0u16..=10_000,
        0u16..=10_000,
        proptest::array::uniform4(1u16..=10_000),
    )
        .prop_map(
            |(model_pay_bp, model_rec_bp, term_bp, demand_k_bp, demand_cap_bp, collateral_bp)| {
                vernier::Params {
                    model_pay_bp,
                    model_rec_bp,
                    term_bp,
                    demand_k_bp,
                    demand_cap_bp,
                    collateral_bp,
                }
            },
        )
}
/// A pool whose utilisation respects the caps (the only states reachable through the handlers).
fn arb_pool() -> impl Strategy<Value = vernier::Pool> {
    (1u64..=MAX_NOTIONAL, 0u16..=4_800)
        .prop_flat_map(|(tvl, pay)| (Just(tvl), Just(pay), 0u16..=(4_800u16.min(8_000 - pay))))
        .prop_map(|(tvl, util_pay_bp, util_rec_bp)| vernier::Pool {
            tvl,
            util_pay_bp,
            util_rec_bp,
        })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100_000))]

    // ---------------- fee split: 50/50 buyback and treasury ----------------

    /// Conservation: every fee is fully allocated, `buyback + treasury == fee`, for the whole `u64` range.
    #[test]
    fn split_conserves_for_all_u64(fee in any::<u64>()) {
        let (b, t) = split(fee);
        prop_assert_eq!(b.checked_add(t), Some(fee));
    }

    /// The split is one half each; the remainder (at most one base unit) goes to the treasury, never lost.
    #[test]
    fn split_is_half_with_remainder_to_treasury(fee in any::<u64>()) {
        let (b, t) = split(fee);
        prop_assert_eq!(u128::from(b), u128::from(fee) * u128::from(BUYBACK_BP) / 10_000);
        prop_assert!(t >= b);
        prop_assert!(t - b <= 1);
    }

    /// Booking the same total in one piece or in two pieces never allocates more than the total to either side,
    /// and never loses a unit in aggregate (sum of the two sides equals the total).
    #[test]
    fn split_is_subadditive_per_side_and_conserving_in_aggregate((a, b) in any::<u64>().prop_flat_map(|a| (Just(a), 0u64..=(u64::MAX - a)))) {
        let total = a + b;
        let (ba, ta) = split(a);
        let (bb, tb) = split(b);
        let (bt, tt) = split(total);
        // buyback floors, so two pieces round down at most twice: within one unit of the single split.
        prop_assert!(ba + bb <= bt);
        prop_assert!(bt - (ba + bb) <= 1);
        prop_assert!(ta + tb >= tt);
        prop_assert_eq!(ba + bb + ta + tb, bt + tt);
    }

    // ---------------- overflow freedom at the maximum notional ----------------

    /// Every fee and bounty helper is total on the whole `u64` notional range.
    #[test]
    fn fee_helpers_never_overflow(notional in any::<u64>()) {
        prop_assert!(mul_bp(notional, vernier::OPENING_FEE_BP).is_ok());
        prop_assert!(mul_bp(notional, vernier::LP_EXIT_FEE_BP).is_ok());
        prop_assert!(mul_bp(notional, CRANK_BOUNTY_BP).is_ok());
        prop_assert!(crank_bounty(notional).unwrap() <= CRANK_BOUNTY_CAP);
        prop_assert!(crank_bounty(notional).unwrap() <= mul_bp(notional, CRANK_BOUNTY_BP).unwrap());
    }

    /// Collateral never overflows and is always at least the exact ratio (ceiling), for every parameter set.
    #[test]
    fn collateral_is_ceiling_and_total(notional in any::<u64>(), t in arb_tenor(), p in arb_params()) {
        let c = vernier::collateral(notional, t, &p);
        let exact = u128::from(notional) * u128::from(p.collateral_bp[t as usize]);
        // Either the exact ceiling, or saturated at u64::MAX when the exact value does not fit.
        if exact.div_ceil(10_000) <= u128::from(u64::MAX) {
            prop_assert_eq!(u128::from(c), exact.div_ceil(10_000));
        } else {
            prop_assert_eq!(c, u64::MAX);
        }
        prop_assert!(u128::from(c) * 10_000 >= exact.min(u128::from(u64::MAX) * 10_000));
    }

    /// `pnl_bounded` is total for the maximum notional, the maximum rate difference and the longest tenor, is
    /// bounded by collateral in magnitude and is odd in the rate difference (no asymmetric rounding by sign).
    #[test]
    // `collateral` is bounded to `i64::MAX`: above it `pnl_bounded` returns `Overflow` (review finding on totality
    // of the signed conversion; unreachable with a `u64` notional at six decimals and collateral at most 100 percent).
    fn pnl_is_total_bounded_and_odd(diff in -MAX_DIFF_BP..=MAX_DIFF_BP, notional in any::<u64>(), days in 0u16..=180, collateral in 0u64..=(i64::MAX as u64)) {
        let p = pnl_bounded(diff, notional, days, collateral).unwrap();
        let n = pnl_bounded(-diff, notional, days, collateral).unwrap();
        prop_assert_eq!(p, -n);
        prop_assert!(p.unsigned_abs() <= collateral);
        let sign_ok = if diff < 0 { p <= 0 } else { p >= 0 };
        prop_assert!(sign_ok);
        // Exact magnitude, floored: the pool never pays more than the exact gain; a loss is never overstated.
        let exact = u128::from(diff.unsigned_abs()) * u128::from(notional) * u128::from(days) / (10_000 * 365);
        prop_assert_eq!(u128::from(p.unsigned_abs()), exact.min(u128::from(collateral)).min(u128::from(u64::MAX)));
    }

    /// Quoting at the largest notional a pool can hold never errors for a well-formed pool.
    #[test]
    fn quote_is_total_at_max_notional(pool in arb_pool(), s in 0u16..=30_000, e in 0u16..=30_000, t in arb_tenor(), leg in arb_leg(), p in arb_params()) {
        let vleg = leg_from(leg);
        let cap = vernier::leg_capacity(&pool, vleg);
        let q = vernier::quote(s, e, t, vleg, cap, &pool, &p);
        prop_assert!(q.is_ok(), "quote at leg capacity failed: {:?}", q);
        // Beyond roughly 214 748 x TVL the utilisation delta leaves `i32` and the quote errs (a clean rejection,
        // see the review's informational note on the error label); the handler also rejects such trades at LegCap.
        if u128::from(cap) * 10_000 / u128::from(pool.tvl) < u128::from(u16::MAX) {
            let q = vernier::quote(s, e, t, vleg, cap.saturating_add(1), &pool, &p);
            prop_assert!(q.is_ok(), "quote one unit past leg capacity failed: {:?}", q);
        }
    }

    // ---------------- demand spread monotone in imbalance ----------------

    /// Holding the pool fixed, a larger trade on the imbalance-increasing side never pays a smaller demand
    /// spread, and the spread is capped.
    #[test]
    fn demand_monotone_in_imbalance(pool in arb_pool(), leg in arb_leg(), a in any::<u64>(), extra in any::<u64>(), p in arb_params()) {
        let b = a.saturating_add(extra);
        let vleg = leg_from(leg);
        let (da, _, after_a, _) = vernier::demand_bp(&pool, vleg, a, &p).unwrap();
        let (db, _, after_b, _) = vernier::demand_bp(&pool, vleg, b, &p).unwrap();
        prop_assert!(after_b.unsigned_abs() >= after_a.unsigned_abs() || after_b.signum() != after_a.signum() || da == 0 || db == 0 || db >= da);
        prop_assert!(da <= p.demand_cap_bp && db <= p.demand_cap_bp);
        // Past the point where the trade increases |imbalance|, the spread is monotone in |after|.
        if after_a.signum() == after_b.signum() && after_b.unsigned_abs() >= after_a.unsigned_abs() && da > 0 {
            prop_assert!(db >= da, "demand fell from {da} to {db} while |imbalance| rose from {} to {}", after_a.unsigned_abs(), after_b.unsigned_abs());
        }
    }

    /// Across pool states with the same TVL, a larger starting imbalance on the same side never lowers the demand
    /// spread charged for the same trade.
    #[test]
    fn demand_monotone_in_starting_imbalance(tvl in 1u64..=MAX_NOTIONAL, pay in 0u16..=4_800, step in 0u16..=4_800, n in any::<u64>(), p in arb_params()) {
        let pay2 = pay.saturating_add(step).min(4_800);
        let p1 = vernier::Pool { tvl, util_pay_bp: pay, util_rec_bp: 0 };
        let p2 = vernier::Pool { tvl, util_pay_bp: pay2, util_rec_bp: 0 };
        let (d1, ..) = vernier::demand_bp(&p1, vernier::Leg::Pay, n, &p).unwrap();
        let (d2, ..) = vernier::demand_bp(&p2, vernier::Leg::Pay, n, &p).unwrap();
        prop_assert!(d2 >= d1);
    }

    // ---------------- rounding direction ----------------

    /// Shares are floored: an LP never receives more shares than the exact price gives.
    #[test]
    fn shares_round_against_depositor(amount in any::<u64>(), tvl in 1u64..=MAX_NOTIONAL, supply in 1u64..=MAX_NOTIONAL) {
        if let Ok(s) = shares_for(amount, tvl, supply) {
            let exact = u128::from(amount) * (u128::from(supply) + VIRTUAL_SHARES) / (u128::from(tvl) + VIRTUAL_TVL);
            prop_assert_eq!(u128::from(s), exact);
            // Round trip at the same price never returns more than was put in (dust stays with the pool).
            if let Ok(back) = amount_for(s, tvl, supply) {
                prop_assert!(back <= amount);
            }
        }
    }

    /// Withdrawals are floored: an LP never receives more than the exact share of TVL.
    #[test]
    fn withdrawals_round_against_lp(shares in any::<u64>(), tvl in any::<u64>(), supply in 1u64..=MAX_NOTIONAL) {
        if let Ok(a) = amount_for(shares, tvl, supply) {
            let exact = u128::from(shares) * (u128::from(tvl) + VIRTUAL_TVL) / (u128::from(supply) + VIRTUAL_SHARES);
            prop_assert_eq!(u128::from(a), exact);
            prop_assert!(u128::from(a) * (u128::from(supply) + VIRTUAL_SHARES) <= u128::from(shares) * (u128::from(tvl) + VIRTUAL_TVL));
        }
    }

    /// `mul_bp` floors by less than one base unit: fees never exceed the exact amount and are short by < 1 unit.
    #[test]
    fn mul_bp_floors_by_less_than_one_unit(x in any::<u64>(), bp in 0u32..=10_000) {
        let f = mul_bp(x, bp).unwrap();
        let exact = u128::from(x) * u128::from(bp);
        prop_assert!(u128::from(f) * 10_000 <= exact);
        prop_assert!(exact - u128::from(f) * 10_000 < 10_000);
    }

    /// Utilisation is floored (bp of TVL) and never exceeds 10 000 for notional at or below TVL.
    #[test]
    fn utilisation_rebase_is_floor(tvl in 1u64..=MAX_NOTIONAL, pay_frac in 0u64..=4_800, rec_frac in 0u64..=4_800) {
        let mut pool = blank_pool(tvl);
        pool.open_pay_notional = u64::try_from(u128::from(tvl) * u128::from(pay_frac) / 10_000).unwrap();
        pool.open_rec_notional = u64::try_from(u128::from(tvl) * u128::from(rec_frac) / 10_000).unwrap();
        rebase_utilisation(&mut pool).unwrap();
        prop_assert!(u64::from(pool.util_pay_bp) <= pay_frac);
        prop_assert!(u64::from(pool.util_rec_bp) <= rec_frac);
        prop_assert_eq!(u128::from(pool.util_pay_bp), u128::from(pool.open_pay_notional) * 10_000 / u128::from(tvl));
    }

    /// Days remaining rounds up (the trader pays for a started day on cancel) and is clamped to the tenor.
    #[test]
    fn days_remaining_rounds_up_and_is_clamped(now in 0i64..=i64::MAX / 4, matures in 0i64..=i64::MAX / 4, total in 1u16..=180) {
        let d = days_remaining(now, matures, total);
        prop_assert!(d <= total);
        let secs = matures.saturating_sub(now).max(0);
        let exact_ceil = u128::try_from(secs).unwrap().div_ceil(86_400);
        prop_assert_eq!(u128::from(d), exact_ceil.min(u128::from(total)));
    }

    /// Calibration step bound over the whole `u16` domain (maths finding M-13): the identity is always allowed,
    /// and the program agrees with the reference `floor(old / 2) <= new <= floor(3 old / 2) + 1` everywhere.
    #[test]
    fn calibration_step_contains_identity_and_is_bounded(old in any::<u16>(), new in any::<u16>()) {
        prop_assert!(admin::within_step(old, old));
        let reference = u32::from(new) >= u32::from(old) / 2 && u32::from(new) <= u32::from(old) * 3 / 2 + 1;
        prop_assert_eq!(admin::within_step(old, new), reference);
    }
}
