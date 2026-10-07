//! Differential test: `vernier` against the exact-arithmetic reference model in `model/src/reference.rs`
//! (included by path so this crate gains no dependency). Run with `cargo test -p vernier --release`.
//! `MODEL_DIFF_CASES` sets the seeded loop size (default 2 000 000); `PROPTEST_CASES` the random properties.
#![allow(
    clippy::unwrap_used,
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation
)]

#[path = "../model/src/reference.rs"]
mod reference;

use proptest::prelude::*;
use reference as r;
use vernier::{Leg, Params, Pool, Tenor, VernierError, DEFAULT_PARAMS};

const TENORS: [Tenor; 4] = [Tenor::D28, Tenor::D60, Tenor::D90, Tenor::D180];

fn side(l: Leg) -> r::Side {
    match l {
        Leg::Pay => r::Side::PayFixed,
        Leg::Receive => r::Side::ReceiveFixed,
    }
}
fn cal(p: &Params) -> r::Calibration {
    let w = |a: [u16; 4]| {
        [
            i128::from(a[0]),
            i128::from(a[1]),
            i128::from(a[2]),
            i128::from(a[3]),
        ]
    };
    r::Calibration {
        model_pay_bp: w(p.model_pay_bp),
        model_rec_bp: w(p.model_rec_bp),
        term_bp: w(p.term_bp),
        demand_k_bp: i128::from(p.demand_k_bp),
        demand_cap_bp: i128::from(p.demand_cap_bp),
        collateral_bp: w(p.collateral_bp),
    }
}
fn err(e: VernierError) -> r::RefError {
    match e {
        VernierError::EmptyPool => r::RefError::EmptyPool,
        VernierError::MalformedUtilisation => r::RefError::MalformedUtilisation,
        // The reference models single-swap quotes only; the basis offset and forward horizon bounds cannot arise here.
        VernierError::Overflow
        | VernierError::CorrelationOutOfRange
        | VernierError::ForwardHorizon => r::RefError::Overflow,
    }
}

/// Compares every output of `quote`, `collateral` and `leg_capacity` with the reference. Returns a description on mismatch.
fn compare(s: u16, e: u16, t: usize, l: Leg, n: u64, pool: &Pool, p: &Params) -> Option<String> {
    let c = cal(p);
    let (tvl, up, ur) = (
        i128::from(pool.tvl),
        i128::from(pool.util_pay_bp),
        i128::from(pool.util_rec_bp),
    );
    let got = vernier::quote(s, e, TENORS[t], l, n, pool, p);
    let want = r::quote(
        i128::from(s),
        i128::from(e),
        t,
        side(l),
        i128::from(n),
        tvl,
        up,
        ur,
        &c,
    );
    let same = match (&got, &want) {
        (Ok(a), Ok(b)) => {
            i128::from(a.fixed_bp) == b.fixed_bp
                && i128::from(a.reference_bp) == b.reference_bp
                && i128::from(a.model_bp) == b.model_bp
                && i128::from(a.demand_bp) == b.demand_bp
                && i128::from(a.term_bp) == b.term_bp
                && i128::from(a.imbalance_before_bp) == b.imbalance_before_bp
                && i128::from(a.imbalance_after_bp) == b.imbalance_after_bp
                && a.reduces_imbalance == b.reduces_imbalance
        }
        (Err(a), Err(b)) => err(*a) == *b,
        _ => false,
    };
    if !same {
        return Some(format!(
            "quote s={s} e={e} t={t} l={l:?} n={n} pool={pool:?} p={p:?}\n  got {got:?}\n  want {want:?}"
        ));
    }
    let coll = i128::from(vernier::collateral(n, TENORS[t], p));
    if coll != r::collateral(i128::from(n), t, &c) {
        return Some(format!("collateral n={n} t={t} got {coll}"));
    }
    let cap = i128::from(vernier::leg_capacity(pool, l));
    if cap != r::leg_capacity(tvl, up, ur, side(l)) {
        return Some(format!("capacity pool={pool:?} l={l:?} got {cap}"));
    }
    None
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// Seeded loop with structured boundaries: thresholds of floor(n·10^4/tvl), the rounding half, the cap, i32 limits.
#[test]
fn seeded_structured_sweep_matches_reference() {
    let cases: u64 = std::env::var("MODEL_DIFF_CASES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(2_000_000);
    let mut rng = Rng(0x5EED);
    let tvls = [
        1u64,
        2,
        3,
        9_999,
        10_000,
        10_001,
        12_640_000,
        1_000_000_000_000,
        (1 << 53) + 1,
        1 << 63,
        u64::MAX,
    ];
    let rates = [0u16, 1, 19, 20, 90, 91, 671, 684, 29_999, 30_000, 65_535];
    let mut mismatches = 0u64;
    for i in 0..cases {
        let tvl = if i % 2 == 0 {
            tvls[rng.below(tvls.len() as u64) as usize]
        } else {
            rng.next() | 1
        };
        let up = (rng.below(10_002)) as u16;
        let ur = (rng.below(10_002)) as u16;
        let before = i64::from(up) - i64::from(ur);
        let n = match rng.below(5) {
            0 => rng.next(),
            1 => {
                let m = rng.below(20_001);
                ((u128::from(tvl) * u128::from(m) / 10_000) as u64)
                    .wrapping_add(rng.below(3))
                    .wrapping_sub(1)
            }
            2 => {
                let target = i64::try_from(rng.below(30_000)).unwrap_or(i64::MAX) - before;
                (u128::from(target.unsigned_abs()) * u128::from(tvl) / 10_000) as u64
            }
            3 => {
                let d = u64::from(i32::MAX as u32) + rng.below(5) - 2;
                (u128::from(d) * u128::from(tvl) / 10_000).min(u128::from(u64::MAX)) as u64
            }
            _ => rng.below(50_000_000),
        };
        let s = if i % 3 == 0 {
            rates[rng.below(rates.len() as u64) as usize]
        } else {
            rng.next() as u16
        };
        let e = if i % 5 == 0 {
            rates[rng.below(rates.len() as u64) as usize]
        } else {
            rng.next() as u16
        };
        let t = rng.below(4) as usize;
        let l = if rng.below(2) == 0 {
            Leg::Pay
        } else {
            Leg::Receive
        };
        let p = if rng.below(8) == 0 {
            Params {
                model_pay_bp: [
                    rng.next() as u16,
                    rng.next() as u16,
                    rng.next() as u16,
                    rng.next() as u16,
                ],
                model_rec_bp: [
                    rng.next() as u16,
                    rng.next() as u16,
                    rng.next() as u16,
                    rng.next() as u16,
                ],
                term_bp: [
                    rng.next() as u16,
                    rng.next() as u16,
                    rng.next() as u16,
                    rng.next() as u16,
                ],
                demand_k_bp: rng.next() as u16,
                demand_cap_bp: rng.next() as u16,
                collateral_bp: [
                    rng.next() as u16,
                    rng.next() as u16,
                    rng.next() as u16,
                    rng.next() as u16,
                ],
            }
        } else {
            DEFAULT_PARAMS
        };
        let pool = Pool {
            tvl,
            util_pay_bp: up,
            util_rec_bp: ur,
        };
        if let Some(msg) = compare(s, e, t, l, n, &pool, &p) {
            mismatches += 1;
            if mismatches <= 10 {
                eprintln!("MISMATCH #{i}: {msg}");
            }
        }
    }
    assert_eq!(mismatches, 0, "{mismatches} mismatches in {cases} cases");
}

proptest! {
    #![proptest_config(ProptestConfig { cases: std::env::var("PROPTEST_CASES").ok().and_then(|s| s.parse().ok()).unwrap_or(200_000), ..ProptestConfig::default() })]

    #[test]
    fn random_full_domain_matches_reference(s: u16, e: u16, t in 0usize..4, pay: bool, n: u64, tvl: u64, up: u16, ur: u16, p in (any::<[u16;4]>(), any::<[u16;4]>(), any::<[u16;4]>(), any::<u16>(), any::<u16>(), any::<[u16;4]>())) {
        let p = Params { model_pay_bp: p.0, model_rec_bp: p.1, term_bp: p.2, demand_k_bp: p.3, demand_cap_bp: p.4, collateral_bp: p.5 };
        let pool = Pool { tvl, util_pay_bp: up, util_rec_bp: ur };
        let l = if pay { Leg::Pay } else { Leg::Receive };
        if let Some(msg) = compare(s, e, t, l, n, &pool, &p) { prop_assert!(false, "{}", msg); }
    }

    /// No free lunch and both monotonicities, on the full domain and any calibration.
    #[test]
    fn order_properties(s: u16, e: u16, t in 0usize..4, a: u64, b: u64, tvl in 1u64..=u64::MAX, u1 in 0u16..=10_000, u2 in 0u16..=10_000, ur in 0u16..=10_000) {
        let p = &DEFAULT_PARAMS;
        let (lo, hi) = (a.min(b), a.max(b));
        let pool = Pool { tvl, util_pay_bp: u1, util_rec_bp: ur };
        if let (Ok(x), Ok(y)) = (vernier::quote(s, e, TENORS[t], Leg::Pay, lo, &pool, p), vernier::quote(s, e, TENORS[t], Leg::Receive, lo, &pool, p)) {
            prop_assert!(x.fixed_bp >= y.fixed_bp);
        }
        if let (Ok(x), Ok(y)) = (vernier::quote(s, e, TENORS[t], Leg::Pay, lo, &pool, p), vernier::quote(s, e, TENORS[t], Leg::Pay, hi, &pool, p)) {
            prop_assert!(y.fixed_bp >= x.fixed_bp);
        }
        if let (Ok(x), Ok(y)) = (vernier::quote(s, e, TENORS[t], Leg::Receive, lo, &pool, p), vernier::quote(s, e, TENORS[t], Leg::Receive, hi, &pool, p)) {
            prop_assert!(y.fixed_bp <= x.fixed_bp);
        }
        let (ulo, uhi) = (u1.min(u2), u1.max(u2));
        let p_lo = vernier::quote(s, e, TENORS[t], Leg::Pay, lo, &Pool { tvl, util_pay_bp: ulo, util_rec_bp: ur }, p);
        let p_hi = vernier::quote(s, e, TENORS[t], Leg::Pay, lo, &Pool { tvl, util_pay_bp: uhi, util_rec_bp: ur }, p);
        if let (Ok(x), Ok(y)) = (p_lo, p_hi) { prop_assert!(y.fixed_bp >= x.fixed_bp); }
    }
}
