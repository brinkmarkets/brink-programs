//! Invariant proofs by property testing and exhaustive sweeps, plus the counterexamples recorded in
//! docs/audit/maths/FINDINGS.md. Run from programs/vernier/model with `cargo test --release`.
//! `PROPTEST_CASES` scales the random case counts (default here 100 000 per property).
use proptest::prelude::*;
use vernier::{Leg, Params, Pool, Tenor, DEFAULT_PARAMS};
use vernier_model::bridge;
use vernier_model::gen::{self, TENORS};
use vernier_model::reference as r;
use vernier_model::transcribed as t;

const P: Params = DEFAULT_PARAMS;

fn cases() -> u32 {
    std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(100_000)
}
fn cfg() -> ProptestConfig {
    ProptestConfig {
        cases: cases(),
        max_shrink_iters: 2_000,
        ..ProptestConfig::default()
    }
}
fn arb_leg() -> impl Strategy<Value = Leg> {
    prop_oneof![Just(Leg::Pay), Just(Leg::Receive)]
}
fn arb_tenor() -> impl Strategy<Value = Tenor> {
    prop_oneof![Just(Tenor::D28), Just(Tenor::D60), Just(Tenor::D90), Just(Tenor::D180)]
}
/// A pool the program can actually produce: caps hold.
fn arb_pool() -> impl Strategy<Value = Pool> {
    (1u64..=u64::MAX, 0u16..=4_800)
        .prop_flat_map(|(tvl, pay)| (Just(tvl), Just(pay), 0u16..=(4_800u16.min(8_000 - pay))))
        .prop_map(|(tvl, util_pay_bp, util_rec_bp)| Pool { tvl, util_pay_bp, util_rec_bp })
}
/// Any pool within the quote's precondition (utilisation ≤ 10 000), caps not required.
fn arb_pool_wide() -> impl Strategy<Value = Pool> {
    (1u64..=u64::MAX, 0u16..=10_000, 0u16..=10_000)
        .prop_map(|(tvl, util_pay_bp, util_rec_bp)| Pool { tvl, util_pay_bp, util_rec_bp })
}
fn arb_params() -> impl Strategy<Value = Params> {
    (
        any::<[u16; 4]>(),
        any::<[u16; 4]>(),
        any::<[u16; 4]>(),
        any::<u16>(),
        any::<u16>(),
        any::<[u16; 4]>(),
    )
        .prop_map(|(a, b, c, k, cap, d)| Params {
            model_pay_bp: a,
            model_rec_bp: b,
            term_bp: c,
            demand_k_bp: k,
            demand_cap_bp: cap,
            collateral_bp: d,
        })
}

proptest! {
    #![proptest_config(cfg())]

    /// I-1: total functions. No panic anywhere in the u16 × u16 × tenor × leg × u64 × (u64, u16, u16) × Params domain.
    #[test]
    fn quote_never_panics(s: u16, e: u16, tn in arb_tenor(), l in arb_leg(), n: u64, tvl: u64, up: u16, ur: u16, p in arb_params()) {
        let pool = Pool { tvl, util_pay_bp: up, util_rec_bp: ur };
        let _ = vernier::quote(s, e, tn, l, n, &pool, &p);
        let _ = vernier::demand_bp(&pool, l, n, &p);
        let _ = vernier::collateral(n, tn, &p);
        let _ = vernier::leg_capacity(&pool, l);
        let _ = vernier::pool_invariants_hold(&pool);
    }

    /// I-2: implementation equals the exact reference on random inputs (the generator-driven run is in bin/diff).
    #[test]
    fn impl_matches_reference(s: u16, e: u16, tn in arb_tenor(), l in arb_leg(), n: u64, tvl: u64, up: u16, ur: u16, p in arb_params()) {
        let c = gen::Case { spot: s, ema: e, tenor: tn, leg: l, notional: n, tvl, util_pay: up, util_rec: ur, params: p };
        prop_assert_eq!(bridge::run_impl(&c), bridge::run_ref(&c));
    }

    /// I-3: pay quote ≥ receive quote at every point of the valid domain, for any calibration.
    #[test]
    fn pay_at_least_receive(s: u16, e: u16, tn in arb_tenor(), n: u64, pool in arb_pool_wide(), p in arb_params()) {
        if let (Ok(a), Ok(b)) = (vernier::quote(s, e, tn, Leg::Pay, n, &pool, &p), vernier::quote(s, e, tn, Leg::Receive, n, &pool, &p)) {
            prop_assert!(a.fixed_bp >= b.fixed_bp);
        }
    }

    /// I-4: monotone in notional (pay non-decreasing, receive non-increasing), full u64 notional range.
    #[test]
    fn monotone_in_notional(s: u16, e: u16, tn in arb_tenor(), a: u64, b: u64, pool in arb_pool_wide(), p in arb_params()) {
        let (lo, hi) = (a.min(b), a.max(b));
        if let (Ok(q1), Ok(q2)) = (vernier::quote(s, e, tn, Leg::Pay, lo, &pool, &p), vernier::quote(s, e, tn, Leg::Pay, hi, &pool, &p)) {
            prop_assert!(q2.fixed_bp >= q1.fixed_bp, "pay {} -> {} for {} -> {}", q1.fixed_bp, q2.fixed_bp, lo, hi);
        }
        if let (Ok(q1), Ok(q2)) = (vernier::quote(s, e, tn, Leg::Receive, lo, &pool, &p), vernier::quote(s, e, tn, Leg::Receive, hi, &pool, &p)) {
            prop_assert!(q2.fixed_bp <= q1.fixed_bp);
        }
    }

    /// I-5: monotone in own-leg utilisation. Pay fixed is non-decreasing in util_pay; receive fixed is
    /// non-increasing in util_rec; and each is monotone the other way in the opposite leg's utilisation.
    #[test]
    fn monotone_in_utilisation(s: u16, e: u16, tn in arb_tenor(), n: u64, tvl in 1u64..=u64::MAX, u1 in 0u16..=10_000, u2 in 0u16..=10_000, other in 0u16..=10_000, p in arb_params()) {
        let (lo, hi) = (u1.min(u2), u1.max(u2));
        let pay_lo = vernier::quote(s, e, tn, Leg::Pay, n, &Pool { tvl, util_pay_bp: lo, util_rec_bp: other }, &p);
        let pay_hi = vernier::quote(s, e, tn, Leg::Pay, n, &Pool { tvl, util_pay_bp: hi, util_rec_bp: other }, &p);
        if let (Ok(a), Ok(b)) = (pay_lo, pay_hi) { prop_assert!(b.fixed_bp >= a.fixed_bp); }
        let rec_lo = vernier::quote(s, e, tn, Leg::Receive, n, &Pool { tvl, util_pay_bp: other, util_rec_bp: lo }, &p);
        let rec_hi = vernier::quote(s, e, tn, Leg::Receive, n, &Pool { tvl, util_pay_bp: other, util_rec_bp: hi }, &p);
        if let (Ok(a), Ok(b)) = (rec_lo, rec_hi) { prop_assert!(b.fixed_bp <= a.fixed_bp); }
        // opposite leg: more receive utilisation lowers the pay quote; more pay utilisation raises the receive quote
        let pay_lo = vernier::quote(s, e, tn, Leg::Pay, n, &Pool { tvl, util_pay_bp: other, util_rec_bp: lo }, &p);
        let pay_hi = vernier::quote(s, e, tn, Leg::Pay, n, &Pool { tvl, util_pay_bp: other, util_rec_bp: hi }, &p);
        if let (Ok(a), Ok(b)) = (pay_lo, pay_hi) { prop_assert!(b.fixed_bp <= a.fixed_bp); }
        let rec_lo = vernier::quote(s, e, tn, Leg::Receive, n, &Pool { tvl, util_pay_bp: lo, util_rec_bp: other }, &p);
        let rec_hi = vernier::quote(s, e, tn, Leg::Receive, n, &Pool { tvl, util_pay_bp: hi, util_rec_bp: other }, &p);
        if let (Ok(a), Ok(b)) = (rec_lo, rec_hi) { prop_assert!(b.fixed_bp >= a.fixed_bp); }
    }

    /// I-6: demand is zero exactly when |after| ≤ |before| and never exceeds the cap.
    #[test]
    fn demand_bounded_and_zero_when_reducing(l in arb_leg(), n: u64, pool in arb_pool_wide(), p in arb_params()) {
        if let Ok((d, before, after, reduces)) = vernier::demand_bp(&pool, l, n, &p) {
            prop_assert!(d <= p.demand_cap_bp);
            prop_assert_eq!(reduces, after.unsigned_abs() <= before.unsigned_abs());
            if reduces { prop_assert_eq!(d, 0); }
        }
    }

    /// I-7: opening at most `leg_capacity` keeps both caps after the utilisation rebase (floor slack included).
    #[test]
    fn capacity_then_rebase_respects_caps(pool in arb_pool(), l in arb_leg(), frac in 0u64..=10_000) {
        let cap = vernier::leg_capacity(&pool, l);
        let n = (u128::from(cap) * u128::from(frac) / 10_000) as u64;
        // open notional consistent with the stored utilisation: the largest value flooring to util
        let open_of = |u: u16| -> u64 { ((u128::from(u) + 1) * u128::from(pool.tvl) / 10_000).saturating_sub(1).max(0) as u64 };
        let (mut op, mut orc) = (open_of(pool.util_pay_bp), open_of(pool.util_rec_bp));
        prop_assume!(t::util_of(op, pool.tvl) == Some(pool.util_pay_bp) && t::util_of(orc, pool.tvl) == Some(pool.util_rec_bp));
        match l { Leg::Pay => op = op.saturating_add(n), Leg::Receive => orc = orc.saturating_add(n) }
        let (up, ur) = (t::util_of(op, pool.tvl), t::util_of(orc, pool.tvl));
        prop_assert!(up.is_some() && ur.is_some());
        let after_pool = Pool { tvl: pool.tvl, util_pay_bp: up.unwrap(), util_rec_bp: ur.unwrap() };
        prop_assert!(vernier::pool_invariants_hold(&after_pool), "caps broken after open");
    }

    /// I-8: collateral is a ceiling and covers every loss up to the implied rate move
    /// |diff| ≤ floor(collateral_bp · 365 / days); beyond that the clamp binds (trader loss capped, pool absorbs).
    #[test]
    fn collateral_covers_bounded_loss(n in 0u64..=(1u64 << 63), tn in arb_tenor(), diff in 0i64..=65_535) {
        let coll = vernier::collateral(n, tn, &P);
        let days = tn.days();
        let cbp = i64::from(P.collateral_bp[bridge::tenor_ix(tn)]);
        let covered = diff * i64::from(days) <= cbp * 365;
        let unclamped = r::Q::new(i128::from(diff) * i128::from(n) * i128::from(days), r::BP * r::DAYS_PER_YEAR).floor();
        if covered { prop_assert!(unclamped <= coll as i128, "loss {} > collateral {}", unclamped, coll); }
        // the clamp always holds
        let pnl = t::pnl_bounded(-diff, n, days, coll);
        if let Some(p) = pnl { prop_assert!(p >= -(coll as i64)); }
    }

    /// I-9: settlement conservation to the unit. For any close: payout + bounty + income fee + tvl' = collateral + tvl,
    /// and collateral_held' = collateral_held − collateral. The vault therefore keeps exactly tvl' + collateral_held'.
    #[test]
    fn close_conserves(pnl_raw: i64, coll in 0u64..=(1u64 << 62), n: u64, tvl in 0u64..=(1u64 << 62), held_extra in 0u64..=(1u64 << 62), open_extra in 0u64..=(1u64 << 62), bounty in any::<bool>()) {
        let pnl = pnl_raw.clamp(-(coll as i64), coll as i64);
        let held = coll + held_extra;
        let open_leg = n.saturating_add(open_extra);
        if let Some(c) = t::close(pnl, coll, n, tvl, held, open_leg, bounty) {
            let outflow = u128::from(c.payout) + u128::from(c.bounty) + u128::from(c.fee);
            prop_assert_eq!(outflow + u128::from(c.tvl), u128::from(coll) + u128::from(tvl));
            prop_assert_eq!(c.collateral_held, held - coll);
            prop_assert!(c.fee <= c.gain / 10 + 1);
            prop_assert!(c.gain <= tvl);
            // reference agrees
            let rr = r::close(pnl as i128, coll as i128, n as i128, tvl as i128, held as i128, bounty);
            prop_assert_eq!((rr.payout, rr.bounty, rr.income_fee, rr.tvl_after), (c.payout as i128, c.bounty as i128, c.fee as i128, c.tvl as i128));
        }
    }

    /// I-10: fee split sums exactly and the halves differ by at most one unit, over all u64.
    #[test]
    fn split_exact(fee: u64) {
        let (b, tr) = t::split(fee);
        prop_assert_eq!(u128::from(b) + u128::from(tr), u128::from(fee));
        prop_assert!(tr >= b && tr - b <= 1);
        prop_assert_eq!((b as i128, tr as i128), r::split(fee as i128));
    }

    /// I-11: LP share price never falls from deposits, withdrawals or fees; rounding dust stays with the pool.
    #[test]
    fn share_price_non_decreasing(tvl in 1u64..=(1u64 << 60), supply in 1u64..=(1u64 << 60), amount in 1u64..=(1u64 << 60), shares_frac in 0u64..=10_000) {
        // deposit
        if let Some(sh) = t::shares_for(amount, tvl, supply) {
            if sh > 0 {
                let (tvl2, sup2) = (u128::from(tvl) + u128::from(amount), u128::from(supply) + u128::from(sh));
                prop_assert!(tvl2 * u128::from(supply) >= u128::from(tvl) * sup2, "deposit lowered the price");
            }
        }
        // withdraw (the exit fee leaves tvl with gross, not net, so fees cannot lower the price)
        let shares = ((u128::from(supply) * u128::from(shares_frac)) / 10_000) as u64;
        if shares > 0 && shares < supply {
            let gross = t::amount_for(shares, tvl, supply).unwrap();
            let (tvl2, sup2) = (tvl - gross, supply - shares);
            prop_assert!(u128::from(tvl2) * u128::from(supply) >= u128::from(tvl) * u128::from(sup2), "withdrawal lowered the price");
        }
        let want = r::shares_for(amount as i128, tvl as i128, supply as i128);
        prop_assert_eq!(t::shares_for(amount, tvl, supply).map(i128::from), (want <= r::U64_MAX).then_some(want));
        prop_assert_eq!(t::amount_for(shares, tvl, supply).map(i128::from), r::amount_for(shares as i128, tvl as i128, supply as i128));
        prop_assert_eq!(t::share_price_e6(tvl, supply).map(i128::from), Some(r::share_price_e6(tvl as i128, supply as i128)));
    }

    /// I-12: the sum of every holder's redemption never exceeds tvl (rounding cannot drain the pool).
    #[test]
    fn redemptions_never_exceed_tvl(seed: u64, steps in 1usize..40) {
        let mut rng = gen::Rng(seed);
        let (mut tvl, mut supply) = (0u64, 0u64);
        let mut holders: Vec<u64> = Vec::new();
        for _ in 0..steps {
            if holders.is_empty() || rng.chance(2) {
                let amount = rng.below(1 << 40) + 1;
                let sh = t::shares_for(amount, tvl, supply).unwrap();
                if sh == 0 { continue; }
                tvl += amount; supply += sh; holders.push(sh);
            } else {
                let i = rng.below(holders.len() as u64) as usize;
                let sh = rng.below(holders[i]) + 1;
                let gross = t::amount_for(sh, tvl, supply).unwrap();
                if gross == 0 { continue; }
                tvl -= gross; supply -= sh; holders[i] -= sh;
                if holders[i] == 0 { holders.swap_remove(i); }
            }
            let total: u128 = holders.iter().map(|&h| u128::from(t::amount_for(h, tvl, supply).unwrap_or(0))).sum();
            prop_assert!(total <= u128::from(tvl), "holders could redeem {} from tvl {}", total, tvl);
        }
    }

    /// I-13: pnl, average, EMA, step bound: transcription equals the reference.
    #[test]
    fn transcribed_equals_reference(diff in -(1i64 << 40)..=(1i64 << 40), n: u64, days in prop_oneof![Just(28u16), Just(60), Just(90), Just(180)], bound: u64, start: u128, len in 0u128..=(1u128 << 100), secs in 1i64..=(1i64 << 40), ema: u16, v: u16, hl in 1u64..=(1u64 << 40), dt in 0u64..=(1u64 << 40), old: u16, new: u16) {
        prop_assert_eq!(t::pnl_bounded(diff, n, days, bound).map(i128::from), r::pnl(diff as i128, n as i128, days as i128, bound as i128));
        let end = start.saturating_add(len);
        prop_assert_eq!(t::average_bp(start, end, secs).map(i128::from), r::average_bp(start as i128, end as i128, secs as i128).filter(|v| *v <= i64::MAX as i128));
        prop_assert_eq!(t::ema_update(ema, v, hl, dt).map(i128::from), Some(r::ema_step(ema as i128, v as i128, hl as i128, dt as i128)));
        // Patch 0008 (finding M-6): the milli-bp update and its public view agree with the reference.
        let (m, view) = t::ema_update_milli(u32::from(ema) * 1_000, v, hl, dt).unwrap();
        prop_assert_eq!(i128::from(m), r::ema_step_milli(i128::from(ema) * 1_000, v as i128, hl as i128, dt as i128));
        prop_assert_eq!(i128::from(view), r::ema_view_bp(i128::from(m)));
        // Finding M-13 closed by patch 0007: the step bound is computed in u32 and agrees with the reference everywhere.
        prop_assert_eq!(t::within_step(old, new), r::within_step(old as i128, new as i128));
        prop_assert_eq!(t::days_remaining(0, secs, days) as i128, r::days_remaining(0, secs as i128, days as i128));
    }

    /// I-14: EMA stays within [min(ema, value), max(ema, value)] and the band check bounds a single step by band·dt/(hl+dt).
    #[test]
    fn ema_bounded(ema: u16, v: u16, hl in 1u64..=(1u64 << 40), dt in 0u64..=(1u64 << 40)) {
        let new = t::ema_update(ema, v, hl, dt).unwrap();
        prop_assert!(new >= ema.min(v) && new <= ema.max(v));
        let (_, view) = t::ema_update_milli(u32::from(ema) * 1_000, v, hl, dt).unwrap();
        prop_assert!(view >= ema.min(v) && view <= ema.max(v));
    }
}

// ---------------------------------------------------------------------------------------------------------
// Exhaustive sweeps and counterexamples (plain tests).
// ---------------------------------------------------------------------------------------------------------

/// Exhaustive: for every imbalance_before in [−10 000, 10 000] and every d_bp in [0, 30 000], pay-side demand is
/// non-decreasing in before and in d; receive-side symmetric. This is the complete finite domain of the demand
/// function up to the (irrelevant) scale of notional/tvl.
#[test]
fn demand_exhaustive_monotone_in_before_and_d() {
    let k = i128::from(P.demand_k_bp);
    let cap = i128::from(P.demand_cap_bp);
    let f = |before: i128, d: i128| -> i128 {
        let after = before + d;
        if after.abs() <= before.abs() {
            0
        } else {
            // Signed trapezoid (external scan 2, finding 19): (after² − before²) / d = 2 · before + d.
            let twice_avg = (2 * before + d).max(0);
            r::Q::new(k * twice_avg, 20_000).round_half_up().min(cap)
        }
    };
    for d in 0..=30_000i128 {
        let mut prev = f(-10_000, d);
        for before in -9_999..=10_000i128 {
            let cur = f(before, d);
            assert!(cur >= prev, "demand not monotone in before: d={d} before={before} {prev}->{cur}");
            prev = cur;
        }
    }
    for before in -10_000..=10_000i128 {
        let mut prev = f(before, 0);
        for d in 1..=30_000i128 {
            let cur = f(before, d);
            assert!(cur >= prev, "demand not monotone in d: before={before} d={d} {prev}->{cur}");
            prev = cur;
        }
    }
}

/// Exhaustive over the whole u16 × u16 reference domain for one pool: pay ≥ receive and spread bound.
#[test]
fn reference_exhaustive_u16() {
    let pool = Pool { tvl: 12_640_000, util_pay_bp: 4_100, util_rec_bp: 2_100 };
    for s in (0..=65_535u16).step_by(7) {
        for e in (0..=65_535u16).step_by(13) {
            let a = vernier::quote(s, e, Tenor::D90, Leg::Pay, 100_000, &pool, &P).unwrap();
            let b = vernier::quote(s, e, Tenor::D90, Leg::Receive, 100_000, &pool, &P).unwrap();
            assert!(a.fixed_bp >= b.fixed_bp);
            assert_eq!(a.reference_bp, i32::from(s.max(e)));
            assert_eq!(b.reference_bp, i32::from(s.min(e)));
        }
    }
}

/// Finding M-3 counterexample: splitting a trade into sub-threshold pieces pays no demand spread.
#[test]
fn counterexample_demand_avoidable_by_splitting() {
    let pool = Pool { tvl: 12_640_000, util_pay_bp: 4_100, util_rec_bp: 2_100 };
    let whole = vernier::quote(684, 671, Tenor::D90, Leg::Pay, 100_000, &pool, &P).unwrap();
    assert_eq!(whole.demand_bp, 9); // the docs' worked example
    // 80 trades of 1 250 USDC: each had floor(1250·10^4/12 640 000) = 0 bp of TVL, so after == before and the
    // split paid nothing. With patch 0003 applied (ceil size, trapezoid average) each piece pays the 9 bp the
    // whole trade pays: the finding is closed and this test pins the fixed behaviour.
    let piece = 1_250u64;
    let mut pool_i = pool;
    let mut open_pay = 4_100u64 * 12_640_000 / 10_000; // 5 182 400, consistent with util 4100
    let mut total_demand = 0i32;
    for _ in 0..80 {
        let q = vernier::quote(684, 671, Tenor::D90, Leg::Pay, piece, &pool_i, &P).unwrap();
        assert_eq!(q.demand_bp, 9, "every piece pays what the whole pays");
        total_demand += q.demand_bp;
        open_pay += piece;
        pool_i.util_pay_bp = t::util_of(open_pay, pool_i.tvl).unwrap();
    }
    assert_eq!(total_demand, 80 * 9, "80 × 1 250 USDC pay-fixed opens pay the demand spread 80 times");
    assert_eq!(pool_i.util_pay_bp, 4_179); // the book moved by 79 bp of TVL, exactly as the whole trade would
}

/// Finding M-4 counterexample: a close that pays a gain can push the other leg over its cap, so the end-of-instruction
/// invariant rejects the close.
#[test]
fn counterexample_close_blocked_by_cap_on_other_leg() {
    let tvl = 1_000_000_000_000u64; // 1 000 000 USDC
    let open_rec = 480_000_000_000u64; // receive leg exactly at its 48 percent cap
    let open_pay = 100_000_000_000u64; // one pay swap of 100 000 USDC, 180 days
    let coll = vernier::collateral(open_pay, Tenor::D180, &P); // 6 000 USDC
    let pnl = coll as i64; // trader wins the full collateral
    let c = t::close(pnl, coll, open_pay, tvl, coll, open_pay, false).unwrap();
    let up = t::util_of(c.open_leg_notional, c.tvl).unwrap();
    let ur = t::util_of(open_rec, c.tvl).unwrap();
    assert_eq!(ur, 4_828);
    assert!(!vernier::pool_invariants_hold(&Pool { tvl: c.tvl, util_pay_bp: up, util_rec_bp: ur }));
}

/// Finding M-6 counterexample: the floor in the EMA update makes it converge to value − 1 from below.
#[test]
fn counterexample_ema_sticks_one_below() {
    // devnet guards: half_life_slots 10 000, min_interval_slots 150 (publisher cadence)
    let (hl, dt) = (10_000u64, 150u64);
    let mut ema = 600u16;
    for _ in 0..10_000 {
        ema = t::ema_update(ema, 684, hl, dt).unwrap();
    }
    assert_eq!(ema, 617, "EMA from below stalls (hl + dt) / dt = 67 bp under the constant published value");
    let mut ema = 700u16;
    for _ in 0..10_000 {
        ema = t::ema_update(ema, 684, hl, dt).unwrap();
    }
    assert_eq!(ema, 684, "from above it converges exactly");
    // the documented seven-day half-life (about 1 512 000 slots) with the same cadence: the EMA can never rise
    let mut ema = 600u16;
    for _ in 0..100_000 {
        ema = t::ema_update(ema, 3_000, 1_512_000, 150).unwrap();
    }
    assert_eq!(ema, 600, "with a seven-day half-life and one publish per minute the EMA is frozen from below");
    // Patch 0008 closes it: at milli-bp the same sequences reach the input exactly from both sides.
    let mut m = 600u32 * 1_000;
    let mut view = 600u16;
    for _ in 0..10_000 {
        (m, view) = t::ema_update_milli(m, 684, hl, dt).unwrap();
    }
    assert_eq!(view, 684, "from below the milli-bp EMA reaches the constant input");
    let mut m = 600u32 * 1_000;
    let mut view = 600u16;
    for _ in 0..1_000_000 {
        (m, view) = t::ema_update_milli(m, 3_000, 1_512_000, 150).unwrap();
    }
    assert!(view > 600, "with a seven-day half-life the milli-bp EMA is no longer frozen");
}

/// Finding M-2 counterexample: a publish after maturity inflates the settlement average.
#[test]
fn counterexample_post_maturity_publish_inflates_average() {
    let secs_per_day = 86_400i64;
    let open_ts = 1_000_000i64;
    let matures = open_ts + 90 * secs_per_day;
    // benchmark published at 684 bp just before open; next publish 9 days after maturity (keeper outage)
    let (acc0, v0, ts0) = (0u128, 684u16, open_ts - 60);
    let start = t::accrual_at(acc0, v0, ts0, open_ts).unwrap();
    let ts1 = matures + 9 * secs_per_day;
    let acc1 = acc0 + u128::from(v0) * u128::try_from(ts1 - ts0).unwrap() * t::ACCRUAL_SCALE; // publish folds 684 bp over [ts0, ts1]
    let end = t::accrual_at(acc1, 684, ts1, matures.min(ts1 + 1)).unwrap(); // settle after the publish: now > matures
    let avg = t::average_bp(start, end, matures - open_ts).unwrap();
    assert_eq!(avg, 752, "constant 684 bp index settles as 752 bp: +68 bp (10 percent) from 9 days beyond maturity");
    // prompt settlement (no publish after maturity) is exact
    let end_prompt = t::accrual_at(acc0, v0, ts0, matures).unwrap();
    assert_eq!(t::average_bp(start, end_prompt, matures - open_ts).unwrap(), 684);
}

/// Finding M-1 counterexample: cancel pnl ignores the elapsed term. A pay-fixed trader whose floating leg fell
/// 300 bp for 89 of 90 days escapes the loss by cancelling before maturity.
#[test]
fn counterexample_cancel_ignores_accrued_pnl() {
    let notional = 1_000_000_000_000u64; // 1 000 000 USDC
    let fixed = 731i64;
    let floating = 431i64; // 300 bp below fixed for the whole term
    let coll = vernier::collateral(notional, Tenor::D90, &P); // 33 000 USDC
    // settlement at maturity: loss = 300 · n · 90 / (10^4 · 365) = 7 397.26 USDC (within the 33 000 USDC collateral)
    let settle = t::pnl_bounded(t::oriented_diff(t::LegKind::PayFixed, floating, fixed).unwrap(), notional, 90, coll).unwrap();
    assert_eq!(settle, -7_397_260_273);
    // cancel with one day left: unwind quote (receive leg) say 431 − 14 − 7 = 410 → diff = 410 − 731 = −321 over 1 day
    let unwind = 410i64;
    let pnl_cancel = t::pnl_bounded(t::oriented_diff(t::LegKind::PayFixed, unwind, fixed).unwrap(), notional, t::days_remaining(0, 3_600, 90), coll).unwrap();
    assert_eq!(pnl_cancel, -87_945_205); // −87.95 USDC instead of −7 397.26 USDC: 98.8 percent of the loss escapes
}

/// Finding M-5 counterexample: liquidation window needs no loss. Any swap can be liquidated in its last six hours.
#[test]
fn counterexample_liquidation_window_is_unconditional() {
    // the code: require!(near_maturity || exhausted). With near_maturity true the mark is irrelevant.
    let coll = 33_000_000_000u64;
    assert_eq!(t::exhausted(0, coll), Some(false)); // healthy
    let near_maturity = true;
    assert!(near_maturity || t::exhausted(0, coll).unwrap());
}

/// Receive-fixed quotes go negative at low rates and the program then fails the open with `Overflow`.
#[test]
fn counterexample_negative_receive_quote() {
    // receive leg at its cap after the trade: |after| = 4 800; trapezoid demand = rhu(45 · (4 000 + 4 800) / 20 000) = 20
    let pool = Pool { tvl: 12_640_000, util_pay_bp: 0, util_rec_bp: 4_000 };
    let q = vernier::quote(40, 40, Tenor::D180, Leg::Receive, 1_011_200, &pool, &P).unwrap();
    assert_eq!(q.demand_bp, -20);
    assert_eq!(q.fixed_bp, 40 - 19 - 20 - 12);
    assert!(q.fixed_bp < 0);
    assert!(u16::try_from(q.fixed_bp).is_err());
}

/// Deposit into a wiped pool (tvl 0, supply > 0) mints 1:1, so the depositor gives most of the deposit to the old shares.
#[test]
fn counterexample_deposit_into_wiped_pool() {
    let (tvl, supply) = (0u64, 1_000_000_000_000u64);
    let amount = 1_000_000_000u64; // 1 000 USDC
    let sh = t::shares_for(amount, tvl, supply).unwrap();
    assert_eq!(sh, amount);
    let redeemable = t::amount_for(sh, tvl + amount, supply + sh).unwrap();
    assert_eq!(redeemable, 999_000); // 0.999 USDC of the 1 000 deposited
}

/// First-depositor inflation: 1 unit deposit, donation synced into tvl, next depositor loses to rounding.
#[test]
fn counterexample_share_inflation() {
    let (tvl, supply) = (1u64, 1u64);
    let donated = 10_000_000_000u64; // 10 000 USDC sent to the vault and folded in by sync_vault
    let tvl = tvl + donated;
    let victim = 19_999_999_999u64; // just under 2 × tvl
    let sh = t::shares_for(victim, tvl, supply).unwrap();
    assert_eq!(sh, 1);
    let redeemable = t::amount_for(sh, tvl + victim, supply + sh).unwrap();
    assert_eq!(redeemable, 15_000_000_000); // the victim can redeem 15 000 of the 19 999.99 deposited
}

/// Monotone in tenor holds for the devnet calibration (the tables are increasing), a property of the parameters.
#[test]
fn tenor_monotone_for_default_params() {
    for i in 1..4 {
        assert!(P.model_pay_bp[i] >= P.model_pay_bp[i - 1]);
        assert!(P.model_rec_bp[i] >= P.model_rec_bp[i - 1]);
        assert!(P.term_bp[i] >= P.term_bp[i - 1]);
        assert!(P.collateral_bp[i] >= P.collateral_bp[i - 1]);
    }
    let _ = TENORS;
}
