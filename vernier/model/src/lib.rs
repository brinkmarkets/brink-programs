//! Audit model crate for the Brink maths. See `docs/audit/maths/SPEC.md`.
//!
//! * `reference`  : exact rational reference model written from the SPEC.
//! * `transcribed`: line-for-line transcription of the swap_amm / brink_index integer maths.
//! * `gen`        : deterministic structured-plus-random input generator shared by the drivers and tests.
//! * `bridge`     : conversions between `vernier` types and the model's integers, and the comparison itself.

pub mod reference;
pub mod transcribed;

pub mod gen {
    //! SplitMix64 driven generator. Cases are reproducible from (seed, index).
    use vernier::{Leg, Params, Tenor};

    #[derive(Clone, Copy, Debug)]
    pub struct Rng(pub u64);
    impl Rng {
        pub fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }
        pub fn below(&mut self, n: u64) -> u64 {
            if n == 0 {
                0
            } else {
                self.next() % n
            }
        }
        pub fn pick<T: Copy>(&mut self, xs: &[T]) -> T {
            xs[self.below(xs.len() as u64) as usize]
        }
        pub fn chance(&mut self, one_in: u64) -> bool {
            self.below(one_in) == 0
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Case {
        pub spot: u16,
        pub ema: u16,
        pub tenor: Tenor,
        pub leg: Leg,
        pub notional: u64,
        pub tvl: u64,
        pub util_pay: u16,
        pub util_rec: u16,
        pub params: Params,
    }

    pub const TENORS: [Tenor; 4] = [Tenor::D28, Tenor::D60, Tenor::D90, Tenor::D180];

    const RATES: &[u16] = &[
        0, 1, 2, 3, 9, 10, 11, 12, 13, 14, 18, 19, 20, 30, 31, 32, 50, 60, 61, 90, 91, 92, 100, 126, 127, 128,
        500, 671, 684, 999, 1_000, 2_999, 3_000, 9_999, 10_000, 29_999, 30_000, 30_001, 32_767, 32_768,
        65_408, 65_409, 65_410, 65_470, 65_471, 65_472, 65_534, 65_535,
    ];
    const TVLS: &[u64] = &[
        1, 2, 3, 5, 7, 9, 10, 99, 100, 9_999, 10_000, 10_001, 12_640_000, 1_000_000_000, 999_999_999_999,
        1_000_000_000_000, 1 << 32, (1 << 53) - 1, 1 << 53, (1 << 53) + 1, 1 << 62, (1 << 63) - 1, 1 << 63,
        u64::MAX - 1, u64::MAX,
    ];
    const UTILS: &[u16] = &[
        0, 1, 2, 99, 100, 101, 2_100, 2_399, 2_400, 2_401, 3_199, 3_200, 3_201, 4_000, 4_100, 4_799, 4_800,
        4_801, 5_000, 7_999, 8_000, 8_001, 9_999, 10_000, 10_001, 20_000, 65_535,
    ];
    const NOTIONALS: &[u64] = &[
        0, 1, 2, 3, 999, 1_000, 1_001, 100_000, 1_000_000_000, 50_000_000_000_000, (1 << 31) - 1, 1 << 31,
        (1 << 32) - 1, 1 << 32, (1 << 53) - 1, 1 << 53, (1 << 53) + 1, 1 << 62, (1 << 63) - 1, 1 << 63,
        u64::MAX - 1, u64::MAX,
    ];

    fn util_pair(r: &mut Rng, valid_only: bool) -> (u16, u16) {
        loop {
            let (a, b) = match r.below(4) {
                0 => (r.pick(UTILS), r.pick(UTILS)),
                1 => (r.below(10_001) as u16, r.below(10_001) as u16),
                2 => {
                    // within the caps, the state the program actually produces
                    let a = r.below(4_801) as u16;
                    let b = r.below(u64::from(4_800u16.min(8_000 - a)) + 1) as u16;
                    (a, b)
                }
                _ => (r.pick(UTILS), r.below(10_001) as u16),
            };
            if !valid_only || (a <= 10_000 && b <= 10_000) {
                return (a, b);
            }
        }
    }

    fn rate(r: &mut Rng) -> u16 {
        match r.below(3) {
            0 => r.pick(RATES),
            1 => r.below(3_000) as u16,
            _ => r.next() as u16,
        }
    }

    fn params(r: &mut Rng) -> Params {
        if !r.chance(8) {
            return vernier::DEFAULT_PARAMS;
        }
        // Random calibration anywhere in u16; exercises generality of the formulas, not only the devnet table.
        fn arr(r: &mut Rng, cap: u64) -> [u16; 4] {
            [r.below(cap) as u16, r.below(cap) as u16, r.below(cap) as u16, r.below(cap) as u16]
        }
        let small = r.chance(2);
        let cap = if small { 1_000 } else { 65_536 };
        let model_pay_bp = arr(r, cap);
        let model_rec_bp = arr(r, cap);
        let term_bp = arr(r, cap);
        let demand_k_bp = r.below(cap) as u16;
        let demand_cap_bp = r.below(cap) as u16;
        let collateral_bp = arr(r, cap);
        Params {
            model_pay_bp,
            model_rec_bp,
            term_bp,
            demand_k_bp,
            demand_cap_bp,
            collateral_bp,
        }
    }

    /// Notional chosen so that `floor(n · 10^4 / tvl)` and `k · |after|` sit on and around thresholds.
    fn notional(r: &mut Rng, tvl: u64, before: i64, k: u64, cap: u64) -> u64 {
        match r.below(8) {
            0 => r.pick(NOTIONALS),
            1 => r.next(),
            2 => r.below(1u64 << 53),
            3 => r.below(50_000_000),
            4 => {
                // d_bp exactly at or one either side of an integer multiple of tvl/10^4
                let m = r.below(20_001);
                let base = (u128::from(tvl) * u128::from(m) / 10_000) as u64;
                let off = r.below(3) as u64;
                base.wrapping_add(off).wrapping_sub(1)
            }
            5 => {
                // |after| at the rounding half: k·|after| ≡ 5000 (mod 10^4), approximately via search
                let target_after = r.below(30_000) as i64;
                let d = (target_after - before).unsigned_abs();
                (u128::from(d) * u128::from(tvl) / 10_000) as u64
            }
            6 => {
                // |after| at the cap boundary: cap·10^4/k
                if k == 0 {
                    r.below(1 << 40)
                } else {
                    let a = (cap * 10_000 / k) as i64 + r.below(5) as i64 - 2;
                    let d = (a - before).unsigned_abs();
                    (u128::from(d) * u128::from(tvl) / 10_000) as u64
                }
            }
            _ => {
                // d_bp around i32::MAX (the Overflow boundary)
                let d = u64::from(i32::MAX as u32) + r.below(5) - 2;
                (u128::from(d) * u128::from(tvl) / 10_000).min(u128::from(u64::MAX)) as u64
            }
        }
    }

    /// The i-th case of a seed. `valid_only` restricts to utilisations ≤ 10 000 (the quote's precondition).
    pub fn case(seed: u64, i: u64, valid_only: bool) -> Case {
        let mut r = Rng(seed ^ i.wrapping_mul(0xD1B5_4A32_D192_ED03));
        let _ = r.next();
        let p = params(&mut r);
        let tvl = match r.below(3) {
            0 => r.pick(TVLS),
            1 => r.below(50_000_000) + 1,
            _ => r.next() | 1,
        };
        let (up, ur) = util_pair(&mut r, valid_only);
        let before = i64::from(up) - i64::from(ur);
        let n = notional(&mut r, tvl, before, u64::from(p.demand_k_bp), u64::from(p.demand_cap_bp));
        Case {
            spot: rate(&mut r),
            ema: rate(&mut r),
            tenor: r.pick(&TENORS),
            leg: if r.chance(2) { Leg::Pay } else { Leg::Receive },
            notional: n,
            tvl,
            util_pay: up,
            util_rec: ur,
            params: p,
        }
    }
}

pub mod bridge {
    //! Runs one case through `vernier` and through the reference model and compares every field.
    use super::gen::Case;
    use super::reference as r;
    use vernier::{Leg, Pool, Tenor, VernierError};

    pub fn side(leg: Leg) -> r::Side {
        match leg {
            Leg::Pay => r::Side::PayFixed,
            Leg::Receive => r::Side::ReceiveFixed,
        }
    }
    pub fn tenor_ix(t: Tenor) -> usize {
        match t {
            Tenor::D28 => 0,
            Tenor::D60 => 1,
            Tenor::D90 => 2,
            Tenor::D180 => 3,
        }
    }
    pub fn calibration(p: &vernier::Params) -> r::Calibration {
        let w = |a: [u16; 4]| [i128::from(a[0]), i128::from(a[1]), i128::from(a[2]), i128::from(a[3])];
        r::Calibration {
            model_pay_bp: w(p.model_pay_bp),
            model_rec_bp: w(p.model_rec_bp),
            term_bp: w(p.term_bp),
            demand_k_bp: i128::from(p.demand_k_bp),
            demand_cap_bp: i128::from(p.demand_cap_bp),
            collateral_bp: w(p.collateral_bp),
        }
    }
    pub fn err(e: VernierError) -> r::RefError {
        match e {
            VernierError::EmptyPool => r::RefError::EmptyPool,
            VernierError::MalformedUtilisation => r::RefError::MalformedUtilisation,
            // The forward-horizon refusal is outside the spot model's domain; the bridge never generates it.
            VernierError::Overflow
            | VernierError::CorrelationOutOfRange
            | VernierError::ForwardHorizon => r::RefError::Overflow,
        }
    }

    /// Outcome of the implementation, flattened for printing and comparison.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Out {
        pub status: u8, // 0 ok, 1 EmptyPool, 2 MalformedUtilisation, 3 Overflow
        pub fixed: i128,
        pub reference: i128,
        pub model: i128,
        pub demand: i128,
        pub term: i128,
        pub before: i128,
        pub after: i128,
        pub reduces: bool,
        pub collateral: i128,
        pub capacity: i128,
    }

    fn status_of(e: r::RefError) -> u8 {
        match e {
            r::RefError::EmptyPool => 1,
            r::RefError::MalformedUtilisation => 2,
            r::RefError::Overflow => 3,
        }
    }

    pub fn run_impl(c: &Case) -> Out {
        let pool = Pool {
            tvl: c.tvl,
            util_pay_bp: c.util_pay,
            util_rec_bp: c.util_rec,
        };
        let coll = i128::from(vernier::collateral(c.notional, c.tenor, &c.params));
        let cap = i128::from(vernier::leg_capacity(&pool, c.leg));
        match vernier::quote(c.spot, c.ema, c.tenor, c.leg, c.notional, &pool, &c.params) {
            Ok(q) => Out {
                status: 0,
                fixed: i128::from(q.fixed_bp),
                reference: i128::from(q.reference_bp),
                model: i128::from(q.model_bp),
                demand: i128::from(q.demand_bp),
                term: i128::from(q.term_bp),
                before: i128::from(q.imbalance_before_bp),
                after: i128::from(q.imbalance_after_bp),
                reduces: q.reduces_imbalance,
                collateral: coll,
                capacity: cap,
            },
            Err(e) => Out {
                status: status_of(err(e)),
                fixed: 0,
                reference: 0,
                model: 0,
                demand: 0,
                term: 0,
                before: 0,
                after: 0,
                reduces: false,
                collateral: coll,
                capacity: cap,
            },
        }
    }

    pub fn run_ref(c: &Case) -> Out {
        let cal = calibration(&c.params);
        let t = tenor_ix(c.tenor);
        let s = side(c.leg);
        let (tvl, up, ur, n) = (
            i128::from(c.tvl),
            i128::from(c.util_pay),
            i128::from(c.util_rec),
            i128::from(c.notional),
        );
        let coll = r::collateral(n, t, &cal);
        let cap = r::leg_capacity(tvl, up, ur, s);
        match r::quote(i128::from(c.spot), i128::from(c.ema), t, s, n, tvl, up, ur, &cal) {
            Ok(q) => Out {
                status: 0,
                fixed: q.fixed_bp,
                reference: q.reference_bp,
                model: q.model_bp,
                demand: q.demand_bp,
                term: q.term_bp,
                before: q.imbalance_before_bp,
                after: q.imbalance_after_bp,
                reduces: q.reduces_imbalance,
                collateral: coll,
                capacity: cap,
            },
            Err(e) => Out {
                status: status_of(e),
                fixed: 0,
                reference: 0,
                model: 0,
                demand: 0,
                term: 0,
                before: 0,
                after: 0,
                reduces: false,
                collateral: coll,
                capacity: cap,
            },
        }
    }

    pub fn format_case(c: &Case) -> String {
        let p = &c.params;
        let leg = match c.leg {
            Leg::Pay => 0,
            Leg::Receive => 1,
        };
        let j = |a: [u16; 4]| format!("{} {} {} {}", a[0], a[1], a[2], a[3]);
        format!(
            "{} {} {} {} {} {} {} {} {} {} {} {} {} {}",
            c.spot,
            c.ema,
            tenor_ix(c.tenor),
            leg,
            c.notional,
            c.tvl,
            c.util_pay,
            c.util_rec,
            j(p.model_pay_bp),
            j(p.model_rec_bp),
            j(p.term_bp),
            p.demand_k_bp,
            p.demand_cap_bp,
            j(p.collateral_bp)
        )
    }
    pub fn format_out(o: &Out) -> String {
        format!(
            "{} {} {} {} {} {} {} {} {} {} {}",
            o.status,
            o.fixed,
            o.reference,
            o.model,
            o.demand,
            o.term,
            o.before,
            o.after,
            u8::from(o.reduces),
            o.collateral,
            o.capacity
        )
    }
}
