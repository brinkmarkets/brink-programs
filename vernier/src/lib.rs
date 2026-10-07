//! Vernier: the Brink pricing library.
//!
//! Pure, integer, deterministic, `no_std`, dependency-free. The web mirror (`web/app/src/lib/vernier.ts`)
//! and this crate must agree bit for bit; `tests/vernier.test.ts` and the proptest suite below enforce the
//! same properties, and the CI parity job runs the shared vector file against both.
//!
//! All rates are annualised basis points (6.84% = 684). Utilisation is in bp of pool TVL (0..=10_000).
//! Notional and TVL are integer USDC (6 decimals are the caller's concern; the library is unit-agnostic as long
//! as both use the same unit).
//!
//! ```text
//! pay fixed:      fixed = max(spot, ema) + model_pay[t] + demand + term[t]
//! receive fixed:  fixed = min(spot, ema) − model_rec[t] − demand − term[t]
//! imbalance_before = util_pay − util_rec
//! imbalance_after  = before ± ceil(notional · 10_000 / tvl)       (+ pay, − receive)
//! s_before = before (pay) or −before (receive)       imbalance signed towards the leg being opened
//! demand = 0                                                 if |after| ≤ |before|
//!        = min(cap, round(k · max(0, 2 · s_before + d) / 20_000))   otherwise
//! ```
//!
//! The demand spread is the slope times the average signed imbalance over the fill, measured in the direction
//! the trade pushes the book: `(after² − before²) / d` in signed terms, which is the trapezoid integral of
//! `k · imbalance` from `before` to `after`. A trade therefore pays for the increase in the square of the
//! imbalance it causes and nothing else, and any split of a fill into pieces pays the same total as the whole up
//! to one rounding per piece, whether or not the pieces cross zero or the mirror point `−before` (maths finding
//! M-3; external scan 1, L-23; external scan 2, finding 19, which closed the crossing-fill discount of the
//! earlier `|after|² / (|before| + |after|)` form). The size in bp of TVL rounds up, so no trade is too small to be
//! charged. A fill that ends at or inside `|before|` pays nothing: for a fill that crosses zero to a larger
//! `|after|` only the part beyond the mirror point extends the book, and its area `(|after|² − |before|²) / 2`
//! is averaged over the whole fill `|before| + |after|`, giving `|after| − |before|`.
#![no_std]
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::arithmetic_side_effects,
        clippy::indexing_slicing
    )
)]
#![deny(missing_docs)]

/// Which fixed leg the trader takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Leg {
    /// Trader pays fixed, receives floating (borrower hedge).
    Pay,
    /// Trader receives fixed, pays floating (lender hedge).
    Receive,
}

/// Tenor bucket. The order is the index into every per-tenor table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Tenor {
    /// 28 days
    D28 = 0,
    /// 60 days
    D60 = 1,
    /// 90 days
    D90 = 2,
    /// 180 days
    D180 = 3,
}

impl Tenor {
    /// Days in the tenor.
    #[must_use]
    pub const fn days(self) -> u16 {
        match self {
            Tenor::D28 => 28,
            Tenor::D60 => 60,
            Tenor::D90 => 90,
            Tenor::D180 => 180,
        }
    }
    const fn ix(self) -> usize {
        self as usize
    }
}

/// Governance-set calibration. Changes pass a 48-hour delay on chain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Params {
    /// Pay-side model spread per tenor, bp.
    pub model_pay_bp: [u16; 4],
    /// Receive-side model spread per tenor, bp.
    pub model_rec_bp: [u16; 4],
    /// Tenor term per tenor, bp.
    pub term_bp: [u16; 4],
    /// Demand slope: bp per 1.0 of imbalance.
    pub demand_k_bp: u16,
    /// Demand cap, bp.
    pub demand_cap_bp: u16,
    /// Collateral per tenor, bp of notional.
    pub collateral_bp: [u16; 4],
}

/// Largest demand spread reachable inside the utilisation caps at the default calibration (maths finding
/// M-11). Within the caps `|after| <= 4 800`, so with `k = 45` the trapezoid spread is at most
/// `rhu(45 · (4 799 + 4 800) / 20 000) = 22` bp; the 60 bp `demand_cap_bp` is a parameter bound that cannot
/// bind at this slope. Documented as such; `demand_cap_bp` would need `k >= 125` to bind.
pub const DEFAULT_DEMAND_MAX_BP: u16 = 22;

/// The calibration published for the devnet preview. Mirrors `DEFAULT_PARAMS` in the web library.
pub const DEFAULT_PARAMS: Params = Params {
    model_pay_bp: [11, 22, 31, 54],
    model_rec_bp: [9, 12, 14, 19],
    term_bp: [3, 5, 7, 12],
    demand_k_bp: 45,
    demand_cap_bp: 60,
    collateral_bp: [120, 230, 330, 600],
};

/// Per-leg utilisation cap, bp of TVL.
pub const CAP_LEG_BP: u32 = 4_800;
/// Total utilisation cap, bp of TVL.
pub const CAP_TOTAL_BP: u32 = 8_000;
/// Opening fee, bp of notional.
pub const OPENING_FEE_BP: u32 = 5;
/// Income fee, percent of positive settlement.
pub const INCOME_FEE_PCT: u32 = 10;
/// LP exit fee while swaps are open, bp.
pub const LP_EXIT_FEE_BP: u32 = 50;
/// Trade size in bp of TVL is clamped here before the imbalance arithmetic. Three times the pool: any larger
/// trade is refused by the caps regardless, and the clamp makes `demand_bp` total over `u64` notionals.
pub const D_CLAMP_BP: u32 = 30_000;

/// Pool state the library needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pool {
    /// Total value locked, integer USDC. Must be > 0.
    pub tvl: u64,
    /// Pay-fixed utilisation, bp of TVL.
    pub util_pay_bp: u16,
    /// Receive-fixed utilisation, bp of TVL.
    pub util_rec_bp: u16,
}

/// Why a quote could not be produced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VernierError {
    /// TVL must be positive.
    EmptyPool,
    /// Utilisation above 10 000 bp is malformed state.
    MalformedUtilisation,
    /// Intermediate arithmetic overflowed; inputs are out of the supported range.
    Overflow,
    /// A correlation offset above `MAX_OFFSET_BP` (10 000 bp, a full offset).
    CorrelationOutOfRange,
    /// A forward whose start plus tenor runs past the quoted term structure (`FORWARD_HORIZON_DAYS`).
    ForwardHorizon,
}

/// A full quote with every term visible.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Quote {
    /// Reference rate, bp (max(spot, ema) for pay; min for receive).
    pub reference_bp: i32,
    /// Signed model spread, bp.
    pub model_bp: i32,
    /// Signed demand spread, bp (0 when the trade reduces the imbalance).
    pub demand_bp: i32,
    /// Signed tenor term, bp.
    pub term_bp: i32,
    /// Fixed rate, bp.
    pub fixed_bp: i32,
    /// Imbalance before the trade, bp of TVL.
    pub imbalance_before_bp: i32,
    /// Imbalance after the trade, bp of TVL.
    pub imbalance_after_bp: i32,
    /// True when the trade reduces or holds |imbalance|.
    pub reduces_imbalance: bool,
}

/// Demand spread (unsigned, bp) and the imbalance it was computed from.
///
/// The spread is `k` times the signed average imbalance over the fill, round half up, with the imbalance
/// signed towards the trade: `max(0, 2 s_before + d) / 2`, where `s_before` is the imbalance before the trade
/// (negative when the trade reduces it) and `d` the size. A fill that crosses zero therefore pays the same as
/// the sum of the two fills that meet at zero, so splitting a trade cannot lower its spread (external scan 2,
/// finding 19). Reducing trades, whose signed average is at or below zero, pay nothing.
///
/// # Errors
/// `EmptyPool` when `tvl == 0`; `MalformedUtilisation` when either utilisation exceeds 10 000 bp. `Overflow`
/// is kept in the signature for the arithmetic guards but is not reachable for `u64` inputs: the size is
/// clamped at `D_CLAMP_BP` so `after` stays within `[-40 000, 40 000]`.
pub fn demand_bp(
    pool: &Pool,
    leg: Leg,
    notional: u64,
    p: &Params,
) -> Result<(u16, i32, i32, bool), VernierError> {
    if pool.tvl == 0 {
        return Err(VernierError::EmptyPool);
    }
    if pool.util_pay_bp > 10_000 || pool.util_rec_bp > 10_000 {
        return Err(VernierError::MalformedUtilisation);
    }
    let before = i32::from(pool.util_pay_bp)
        .checked_sub(i32::from(pool.util_rec_bp))
        .ok_or(VernierError::Overflow)?;
    // Size in bp of TVL, rounded up: a trade can never be too small to move the price it is charged. Clamped at
    // `D_CLAMP_BP`: beyond three times the pool the imbalance is already outside anything the caps admit, the
    // quote is only ever refused by `leg_capacity`, and clamping keeps the function total (maths finding M-14).
    let d: u128 = u128::from(notional)
        .checked_mul(10_000)
        .ok_or(VernierError::Overflow)?
        .div_ceil(u128::from(pool.tvl));
    let d_bp = i32::try_from(d.min(u128::from(D_CLAMP_BP))).map_err(|_| VernierError::Overflow)?;
    let after = match leg {
        Leg::Pay => before.checked_add(d_bp),
        Leg::Receive => before.checked_sub(d_bp),
    }
    .ok_or(VernierError::Overflow)?;
    let reduces = after.unsigned_abs() <= before.unsigned_abs();
    // Twice the average signed imbalance over the fill, in the direction of the leg: `s_before + s_after` where
    // `s_after = s_before + d`. This is `(after² − before²) / d`, the exact trapezoid of `imbalance` from `before`
    // to `after`, so the charge depends only on the end points and telescopes over any split of the fill
    // (external scan 2, finding 19). One sign: `|before| + |after|`. Crossing zero to a larger `|after|`:
    // `|after| − |before|`, the part beyond the mirror point averaged over the whole fill. At or inside the mirror
    // point the sum is zero or negative and the trade is a reducing one.
    let s_before: i64 = match leg {
        Leg::Pay => i64::from(before),
        Leg::Receive => i64::from(before)
            .checked_neg()
            .ok_or(VernierError::Overflow)?,
    };
    let twice_avg_signed = s_before
        .checked_mul(2)
        .and_then(|x| x.checked_add(i64::from(d_bp)))
        .ok_or(VernierError::Overflow)?;
    let twice_avg = u64::try_from(twice_avg_signed.max(0)).map_err(|_| VernierError::Overflow)?;
    // k is bp per unit of imbalance → divide by 2 · 10 000, round half up.
    let raw = u64::from(p.demand_k_bp)
        .checked_mul(twice_avg)
        .and_then(|x| x.checked_add(10_000))
        .ok_or(VernierError::Overflow)?
        / 20_000;
    let demand = if reduces {
        0
    } else {
        raw.min(u64::from(p.demand_cap_bp))
    };
    let demand = u16::try_from(demand).map_err(|_| VernierError::Overflow)?;
    Ok((demand, before, after, reduces))
}

/// Full quote.
///
/// # Errors
/// See [`demand_bp`].
pub fn quote(
    spot_bp: u16,
    ema_bp: u16,
    tenor: Tenor,
    leg: Leg,
    notional: u64,
    pool: &Pool,
    p: &Params,
) -> Result<Quote, VernierError> {
    let (demand, before, after, reduces) = demand_bp(pool, leg, notional, p)?;
    let t = tenor.ix();
    // `ix()` is 0..=3 and every table has four entries; `get` keeps the no-panic guarantee explicit.
    let term = i32::from(p.term_bp.get(t).copied().ok_or(VernierError::Overflow)?);
    let demand = i32::from(demand);
    let (reference, model) = match leg {
        Leg::Pay => (
            i32::from(spot_bp.max(ema_bp)),
            i32::from(
                p.model_pay_bp
                    .get(t)
                    .copied()
                    .ok_or(VernierError::Overflow)?,
            ),
        ),
        Leg::Receive => (
            i32::from(spot_bp.min(ema_bp)),
            i32::from(
                p.model_rec_bp
                    .get(t)
                    .copied()
                    .ok_or(VernierError::Overflow)?,
            ),
        ),
    };
    // Every operand is a widened u16, so negation cannot overflow; wrapping_neg makes that explicit to the linter.
    let sign = |v: i32| {
        if matches!(leg, Leg::Pay) {
            v
        } else {
            v.wrapping_neg()
        }
    };
    let fixed = reference
        .checked_add(sign(model))
        .and_then(|v| v.checked_add(sign(demand)))
        .and_then(|v| v.checked_add(sign(term)))
        .ok_or(VernierError::Overflow)?;
    Ok(Quote {
        reference_bp: reference,
        model_bp: sign(model),
        demand_bp: sign(demand),
        term_bp: sign(term),
        fixed_bp: fixed,
        imbalance_before_bp: before,
        imbalance_after_bp: after,
        reduces_imbalance: reduces,
    })
}

/// Mid quote: the fixed rate with no demand term (`reference ± model ± term`). Used for valuations that must
/// not move with the pool's transient imbalance, such as the liquidation trigger (external scan 1, L-22): a
/// position's distance from exhaustion is a property of the rate path, not of who else is trading this slot.
///
/// # Errors
/// `Overflow` only if a table index is out of range, which `Tenor::ix` rules out.
pub fn quote_mid(
    spot_bp: u16,
    ema_bp: u16,
    tenor: Tenor,
    leg: Leg,
    p: &Params,
) -> Result<i32, VernierError> {
    let t = tenor.ix();
    let term = i32::from(p.term_bp.get(t).copied().ok_or(VernierError::Overflow)?);
    let (reference, model) = match leg {
        Leg::Pay => (
            i32::from(spot_bp.max(ema_bp)),
            i32::from(
                p.model_pay_bp
                    .get(t)
                    .copied()
                    .ok_or(VernierError::Overflow)?,
            ),
        ),
        Leg::Receive => (
            i32::from(spot_bp.min(ema_bp)),
            i32::from(
                p.model_rec_bp
                    .get(t)
                    .copied()
                    .ok_or(VernierError::Overflow)?,
            ),
        ),
    };
    let sign = |v: i32| {
        if matches!(leg, Leg::Pay) {
            v
        } else {
            v.wrapping_neg()
        }
    };
    reference
        .checked_add(sign(model))
        .and_then(|v| v.checked_add(sign(term)))
        .ok_or(VernierError::Overflow)
}

/// Largest correlation offset the library accepts: 10 000 bp removes the whole demand charge of both legs.
/// Governance caps the offset lower on chain (`swap_amm::state::MAX_CORRELATION_BP`).
pub const MAX_OFFSET_BP: u16 = 10_000;

/// One side of a basis swap as the library sees it: the benchmark reading and the pool it is quoted against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BasisSide<'a> {
    /// Benchmark spot, bp.
    pub spot_bp: u16,
    /// Benchmark EMA, bp.
    pub ema_bp: u16,
    /// The pool the leg is opened in.
    pub pool: &'a Pool,
    /// That pool's effective calibration.
    pub params: &'a Params,
}

/// A basis quote: the pay-fixed leg on side A, the receive-fixed leg on side B, with the correlation offset
/// already applied to both legs' demand components.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BasisQuote {
    /// Pay-fixed leg on pool A, demand reduced by the offset.
    pub pay: Quote,
    /// Receive-fixed leg on pool B, demand reduced by the offset.
    pub receive: Quote,
    /// Unsigned demand charge across both legs before the offset, bp.
    pub demand_before_bp: u16,
    /// Unsigned demand charge across both legs after the offset, bp: what the position pays for imbalance.
    pub combined_demand_bp: u16,
    /// Demand removed across both legs, bp (`demand_before_bp - combined_demand_bp`).
    pub offset_bp: u16,
}

/// The part of one leg's unsigned demand charge that a correlation offset removes: `floor(d x c / 10 000)`.
/// Floored so the pool keeps the rounding and a full offset (`c = 10 000`) removes exactly `d`.
///
/// # Errors
/// `CorrelationOutOfRange` above `MAX_OFFSET_BP`.
pub fn demand_offset(demand_bp: u16, correlation_bp: u16) -> Result<u16, VernierError> {
    if correlation_bp > MAX_OFFSET_BP {
        return Err(VernierError::CorrelationOutOfRange);
    }
    let r = u32::from(demand_bp)
        .checked_mul(u32::from(correlation_bp))
        .and_then(|x| x.checked_div(u32::from(MAX_OFFSET_BP)))
        .ok_or(VernierError::Overflow)?;
    u16::try_from(r).map_err(|_| VernierError::Overflow)
}

/// Quotes a basis swap: pay fixed on side A and receive fixed on side B, same tenor and notional, each leg
/// priced by [`quote`] against its own pool exactly as a single swap would be, then each leg's demand component
/// reduced by the same fraction `correlation_bp` of itself (`demand_offset`). The model and tenor components
/// are untouched, so the offset can only ever return part of the imbalance charge: the pay leg's fixed rate
/// falls by its offset and the receive leg's fixed rate rises by its offset. With `correlation_bp = 0` both
/// legs are the ordinary quotes; with 10 000 neither leg pays any demand.
///
/// # Errors
/// Whatever [`quote`] reports for either side, and `CorrelationOutOfRange` above `MAX_OFFSET_BP`.
pub fn quote_basis(
    a: &BasisSide,
    b: &BasisSide,
    tenor: Tenor,
    notional: u64,
    correlation_bp: u16,
) -> Result<BasisQuote, VernierError> {
    if correlation_bp > MAX_OFFSET_BP {
        return Err(VernierError::CorrelationOutOfRange);
    }
    let mut pay = quote(
        a.spot_bp,
        a.ema_bp,
        tenor,
        Leg::Pay,
        notional,
        a.pool,
        a.params,
    )?;
    let mut receive = quote(
        b.spot_bp,
        b.ema_bp,
        tenor,
        Leg::Receive,
        notional,
        b.pool,
        b.params,
    )?;
    // Demand components are at most `demand_cap_bp` (a `u16`), so the unsigned values fit.
    let d_pay = u16::try_from(pay.demand_bp.unsigned_abs()).map_err(|_| VernierError::Overflow)?;
    let d_rec =
        u16::try_from(receive.demand_bp.unsigned_abs()).map_err(|_| VernierError::Overflow)?;
    let off_pay = demand_offset(d_pay, correlation_bp)?;
    let off_rec = demand_offset(d_rec, correlation_bp)?;
    // Pay leg: demand enters positively, so the offset lowers the fixed rate. Receive leg: demand enters
    // negatively, so the offset raises it. Both operands are widened `u16`s; the sums cannot overflow `i32`.
    pay.demand_bp = pay
        .demand_bp
        .checked_sub(i32::from(off_pay))
        .ok_or(VernierError::Overflow)?;
    pay.fixed_bp = pay
        .fixed_bp
        .checked_sub(i32::from(off_pay))
        .ok_or(VernierError::Overflow)?;
    receive.demand_bp = receive
        .demand_bp
        .checked_add(i32::from(off_rec))
        .ok_or(VernierError::Overflow)?;
    receive.fixed_bp = receive
        .fixed_bp
        .checked_add(i32::from(off_rec))
        .ok_or(VernierError::Overflow)?;
    let before = d_pay.checked_add(d_rec).ok_or(VernierError::Overflow)?;
    let offset = off_pay.checked_add(off_rec).ok_or(VernierError::Overflow)?;
    Ok(BasisQuote {
        pay,
        receive,
        demand_before_bp: before,
        combined_demand_bp: before.checked_sub(offset).ok_or(VernierError::Overflow)?,
        offset_bp: offset,
    })
}

/// Collateral in the notional's unit, rounded up.
#[must_use]
pub fn collateral(notional: u64, tenor: Tenor, p: &Params) -> u64 {
    let bp = u128::from(p.collateral_bp.get(tenor.ix()).copied().unwrap_or(u16::MAX));
    let n = u128::from(notional).saturating_mul(bp);
    u64::try_from(n.div_ceil(10_000)).unwrap_or(u64::MAX)
}

/// Forward-starting swaps. Start offsets a trader may choose, in days.
pub const START_DAYS: [u16; 3] = [28, 60, 90];
/// The curve ends at the longest tenor: a forward's start plus tenor may not run past it.
pub const FORWARD_HORIZON_DAYS: u16 = 180;
/// Tenor knots of the model curve, in days, in table order.
pub const TENOR_DAYS: [u16; 4] = [28, 60, 90, 180];

/// Scale for the interpolated model curve: four decimal places of a basis point.
const CURVE_SCALE: i64 = 10_000;

/// The leg's model spread at `d` days, linearly interpolated between the tenor knots and flat below the first
/// knot, scaled by `CURVE_SCALE`. `d` above the last knot is refused by the callers (horizon).
fn model_at_scaled(table: &[u16; 4], d: u16) -> Result<i64, VernierError> {
    let knot = |k: usize| -> Result<(i64, i64), VernierError> {
        let day = TENOR_DAYS
            .get(k)
            .copied()
            .ok_or(VernierError::ForwardHorizon)?;
        let model = table.get(k).copied().ok_or(VernierError::ForwardHorizon)?;
        Ok((i64::from(day), i64::from(model)))
    };
    let d = i64::from(d);
    let (first_day, first_model) = knot(0)?;
    if d <= first_day {
        return first_model
            .checked_mul(CURVE_SCALE)
            .ok_or(VernierError::Overflow);
    }
    for k in 0..3 {
        let (d0, m0) = knot(k)?;
        let (d1, m1) = knot(k.checked_add(1).ok_or(VernierError::Overflow)?)?;
        if d <= d1 {
            // m0 + (m1 - m0) * (d - d0) / (d1 - d0), scaled; every operand is small so i64 is ample.
            let span = d1.checked_sub(d0).ok_or(VernierError::Overflow)?;
            if span <= 0 {
                return Err(VernierError::Overflow);
            }
            let num = m1
                .checked_sub(m0)
                .and_then(|rise| rise.checked_mul(d.checked_sub(d0)?))
                .and_then(|x| x.checked_mul(CURVE_SCALE))
                .ok_or(VernierError::Overflow)?;
            return m0
                .checked_mul(CURVE_SCALE)
                .and_then(|x| x.checked_add(num.checked_div(span)?))
                .ok_or(VernierError::Overflow);
        }
    }
    Err(VernierError::ForwardHorizon)
}

/// The implied forward of the leg's model curve for a swap of `tenor` starting `start_days` from now, in bp,
/// rounded half up: `(m(s + n) · (s + n) − m(s) · s) / n`. A flat curve returns the spot model for the tenor
/// exactly; a rising curve returns more than the spot model, a falling curve less (floored at zero, since a
/// model spread is a charge).
///
/// # Errors
/// `ForwardHorizon` when `start_days + tenor.days()` exceeds `FORWARD_HORIZON_DAYS`; `Overflow` never in
/// practice (inputs are bounded by the horizon and the `u16` tables).
pub fn forward_model_bp(
    p: &Params,
    leg: Leg,
    start_days: u16,
    tenor: Tenor,
) -> Result<u16, VernierError> {
    let n = tenor.days();
    let end = start_days.checked_add(n).ok_or(VernierError::Overflow)?;
    if end > FORWARD_HORIZON_DAYS {
        return Err(VernierError::ForwardHorizon);
    }
    let table = match leg {
        Leg::Pay => &p.model_pay_bp,
        Leg::Receive => &p.model_rec_bp,
    };
    let m_end = model_at_scaled(table, end)?;
    let m_start = if start_days == 0 {
        0
    } else {
        model_at_scaled(table, start_days)?
    };
    let num = m_end
        .checked_mul(i64::from(end))
        .and_then(|x| x.checked_sub(m_start.checked_mul(i64::from(start_days))?))
        .ok_or(VernierError::Overflow)?;
    let den = i64::from(n)
        .checked_mul(CURVE_SCALE)
        .ok_or(VernierError::Overflow)?;
    // Round half up on a non-negative numerator; a negative forward is floored at zero.
    let bp = if num <= 0 {
        0
    } else {
        num.checked_add(den.checked_div(2).ok_or(VernierError::Overflow)?)
            .and_then(|x| x.checked_div(den))
            .ok_or(VernierError::Overflow)?
    };
    u16::try_from(bp).map_err(|_| VernierError::Overflow)
}

/// Quotes a forward-starting swap: [`quote`] with the model component replaced by [`forward_model_bp`]. The
/// reference, demand and tenor terms are the spot swap's for the same leg, notional and tenor.
///
/// # Errors
/// Whatever [`quote`] reports, and `ForwardHorizon` from [`forward_model_bp`].
#[allow(
    clippy::too_many_arguments,
    reason = "the spot quote's seven inputs plus the start; a struct would only rename them"
)]
pub fn quote_forward(
    spot_bp: u16,
    ema_bp: u16,
    start_days: u16,
    tenor: Tenor,
    leg: Leg,
    notional: u64,
    pool: &Pool,
    p: &Params,
) -> Result<Quote, VernierError> {
    let q = quote(spot_bp, ema_bp, tenor, leg, notional, pool, p)?;
    let fwd = i32::from(forward_model_bp(p, leg, start_days, tenor)?);
    let model = if matches!(leg, Leg::Pay) {
        fwd
    } else {
        fwd.wrapping_neg()
    };
    let fixed = q
        .fixed_bp
        .checked_sub(q.model_bp)
        .and_then(|v| v.checked_add(model))
        .ok_or(VernierError::Overflow)?;
    Ok(Quote {
        model_bp: model,
        fixed_bp: fixed,
        ..q
    })
}

/// Mid forward quote (no demand term), the forward counterpart of [`quote_mid`].
///
/// # Errors
/// `ForwardHorizon` past the curve; `Overflow` on a malformed table index.
pub fn quote_mid_forward(
    spot_bp: u16,
    ema_bp: u16,
    start_days: u16,
    tenor: Tenor,
    leg: Leg,
    p: &Params,
) -> Result<i32, VernierError> {
    let t = tenor.ix();
    let term = i32::from(p.term_bp.get(t).copied().ok_or(VernierError::Overflow)?);
    let fwd = i32::from(forward_model_bp(p, leg, start_days, tenor)?);
    let reference = match leg {
        Leg::Pay => i32::from(spot_bp.max(ema_bp)),
        Leg::Receive => i32::from(spot_bp.min(ema_bp)),
    };
    let sign = |v: i32| {
        if matches!(leg, Leg::Pay) {
            v
        } else {
            v.wrapping_neg()
        }
    };
    reference
        .checked_add(sign(fwd))
        .and_then(|v| v.checked_add(sign(term)))
        .ok_or(VernierError::Overflow)
}

/// Capacity left on a leg (notional unit). Zero when the leg or the pool is at cap.
#[must_use]
pub fn leg_capacity(pool: &Pool, leg: Leg) -> u64 {
    let used = u32::from(match leg {
        Leg::Pay => pool.util_pay_bp,
        Leg::Receive => pool.util_rec_bp,
    });
    let leg_room = CAP_LEG_BP.saturating_sub(used);
    let total_room = CAP_TOTAL_BP
        .saturating_sub(u32::from(pool.util_pay_bp).saturating_add(u32::from(pool.util_rec_bp)));
    let room = u128::from(leg_room.min(total_room));
    u64::try_from(room.saturating_mul(u128::from(pool.tvl)) / 10_000).unwrap_or(u64::MAX)
}

/// Pool solvency invariant: both caps hold. Programs call this at the end of every instruction.
#[must_use]
pub fn pool_invariants_hold(pool: &Pool) -> bool {
    u32::from(pool.util_pay_bp) <= CAP_LEG_BP
        && u32::from(pool.util_rec_bp) <= CAP_LEG_BP
        && u32::from(pool.util_pay_bp).saturating_add(u32::from(pool.util_rec_bp)) <= CAP_TOTAL_BP
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const TENORS: [Tenor; 4] = [Tenor::D28, Tenor::D60, Tenor::D90, Tenor::D180];
    const P: Params = DEFAULT_PARAMS;

    #[test]
    fn forward_of_flat_curve_is_the_spot_model() {
        let mut p = P;
        p.model_pay_bp = [20, 20, 20, 20];
        p.model_rec_bp = [12, 12, 12, 12];
        for s in START_DAYS {
            for t in [Tenor::D28, Tenor::D60, Tenor::D90] {
                assert_eq!(
                    forward_model_bp(&p, Leg::Pay, s, t).unwrap(),
                    20,
                    "pay {s} {t:?}"
                );
                assert_eq!(
                    forward_model_bp(&p, Leg::Receive, s, t).unwrap(),
                    12,
                    "rec {s} {t:?}"
                );
            }
        }
    }

    #[test]
    fn forward_of_rising_curve_exceeds_spot_model_and_vectors_pin_the_ts_mirror() {
        // DEFAULT_PARAMS pay model rises with tenor, so every forward model is at least the spot model of its tenor.
        for s in START_DAYS {
            for t in [Tenor::D28, Tenor::D60, Tenor::D90] {
                let f = forward_model_bp(&P, Leg::Pay, s, t).unwrap();
                assert!(f >= P.model_pay_bp[t.ix()], "pay {s} {t:?}: {f}");
            }
        }
        // Worked values, pinned so that packages/vernier (TypeScript) asserts the same table. Pay model knots
        // (28,11) (60,22) (90,31) (180,54); start 28 tenor 60 → end 88: m(88) = 22 + 9·28/30 = 30.4;
        // (30.4·88 − 11·28) / 60 = (2675.2 − 308) / 60 = 39.45 → 39.
        assert_eq!(forward_model_bp(&P, Leg::Pay, 28, Tenor::D60).unwrap(), 39);
        // start 90 tenor 90 → end 180: (54·180 − 31·90) / 90 = (9720 − 2790) / 90 = 77.
        assert_eq!(forward_model_bp(&P, Leg::Pay, 90, Tenor::D90).unwrap(), 77);
        // start 60 tenor 28 → end 88: (30.4·88 − 22·60) / 28 = (2675.2 − 1320) / 28 = 48.4 → 48.
        assert_eq!(forward_model_bp(&P, Leg::Pay, 60, Tenor::D28).unwrap(), 48);
        assert_eq!(forward_model_bp(&P, Leg::Pay, 28, Tenor::D28).unwrap(), 30);
        assert_eq!(forward_model_bp(&P, Leg::Pay, 60, Tenor::D90).unwrap(), 63);
        assert_eq!(
            forward_model_bp(&P, Leg::Receive, 28, Tenor::D60).unwrap(),
            16
        );
        assert_eq!(
            forward_model_bp(&P, Leg::Receive, 90, Tenor::D90).unwrap(),
            24
        );
        assert_eq!(
            forward_model_bp(&P, Leg::Receive, 60, Tenor::D28).unwrap(),
            18
        );
    }

    #[test]
    fn forward_refuses_the_horizon_and_zero_start_is_spot() {
        assert_eq!(
            forward_model_bp(&P, Leg::Pay, 28, Tenor::D180),
            Err(VernierError::ForwardHorizon)
        );
        assert_eq!(
            forward_model_bp(&P, Leg::Pay, 91, Tenor::D90),
            Err(VernierError::ForwardHorizon)
        );
        for t in TENORS {
            assert_eq!(
                forward_model_bp(&P, Leg::Pay, 0, t).unwrap(),
                P.model_pay_bp[t.ix()]
            );
            assert_eq!(
                forward_model_bp(&P, Leg::Receive, 0, t).unwrap(),
                P.model_rec_bp[t.ix()]
            );
        }
    }

    #[test]
    fn quote_forward_replaces_only_the_model_term() {
        let pool = Pool {
            tvl: 12_640_000,
            util_pay_bp: 4_100,
            util_rec_bp: 2_100,
        };
        let spot = quote(684, 671, Tenor::D60, Leg::Pay, 100_000, &pool, &P).unwrap();
        let fwd = quote_forward(684, 671, 28, Tenor::D60, Leg::Pay, 100_000, &pool, &P).unwrap();
        assert_eq!(
            (fwd.reference_bp, fwd.demand_bp, fwd.term_bp),
            (spot.reference_bp, spot.demand_bp, spot.term_bp)
        );
        assert_eq!(fwd.model_bp, 39);
        assert_eq!(fwd.fixed_bp, spot.fixed_bp - spot.model_bp + 39);
        let rec =
            quote_forward(684, 671, 28, Tenor::D60, Leg::Receive, 100_000, &pool, &P).unwrap();
        assert_eq!(
            rec.model_bp,
            -i32::from(forward_model_bp(&P, Leg::Receive, 28, Tenor::D60).unwrap())
        );
        assert_eq!(
            quote_mid_forward(684, 671, 28, Tenor::D60, Leg::Pay, &P).unwrap(),
            fwd.fixed_bp - fwd.demand_bp
        );
    }

    #[test]
    fn docs_worked_example() {
        let pool = Pool {
            tvl: 12_640_000,
            util_pay_bp: 4_100,
            util_rec_bp: 2_100,
        };
        let pay = quote(684, 671, Tenor::D90, Leg::Pay, 100_000, &pool, &P).unwrap();
        assert_eq!(
            (
                pay.reference_bp,
                pay.model_bp,
                pay.demand_bp,
                pay.term_bp,
                pay.fixed_bp
            ),
            (684, 31, 9, 7, 731)
        );
        let rec = quote(684, 671, Tenor::D90, Leg::Receive, 100_000, &pool, &P).unwrap();
        assert_eq!(
            (
                rec.reference_bp,
                rec.model_bp,
                rec.demand_bp,
                rec.term_bp,
                rec.fixed_bp
            ),
            (671, -14, 0, -7, 650)
        );
        assert!(rec.reduces_imbalance);
    }

    #[test]
    fn split_trade_pays_what_the_whole_pays() {
        // Maths finding M-3: eighty opens of 1 250 on the documented pool used to pay nothing; now each pays
        // the 9 bp the single 100 000 trade pays, and a trade smaller than tvl / 10^4 is still charged.
        let mut pool = Pool {
            tvl: 12_640_000,
            util_pay_bp: 4_100,
            util_rec_bp: 2_100,
        };
        let whole = quote(684, 671, Tenor::D90, Leg::Pay, 100_000, &pool, &P).unwrap();
        assert_eq!(whole.demand_bp, 9);
        let mut open_pay: u64 = 4_100 * 12_640_000 / 10_000;
        for _ in 0..80 {
            let q = quote(684, 671, Tenor::D90, Leg::Pay, 1_250, &pool, &P).unwrap();
            assert_eq!(q.demand_bp, 9, "{q:?}");
            open_pay += 1_250;
            pool.util_pay_bp = u16::try_from(open_pay * 10_000 / pool.tvl).unwrap();
        }
        assert_eq!(pool.util_pay_bp, 4_179);
        // Crossing zero charges only the part beyond the mirror point, averaged over the whole fill (L-23 as
        // amended by external scan 2, finding 19): from −1 000 to +1 500 the extending area is
        // (1 500² − 1 000²) / 2 over a fill of 2 500, so twice the average is 1 500 − 1 000 = 500 and the spread
        // is rhu(45 · 500 / 20 000) = 1. Crossing to an equal or smaller |imbalance| is a reducing trade and pays
        // nothing, as before.
        let cross = Pool {
            tvl: 10_000,
            util_pay_bp: 0,
            util_rec_bp: 1_000,
        };
        let q = quote(684, 671, Tenor::D90, Leg::Pay, 2_500, &cross, &P).unwrap();
        assert_eq!((q.imbalance_after_bp, q.demand_bp), (1_500, 1));
        // The same 2 500 opened from a balanced book pays rhu(45 · 2 500 / 20 000) = 6; a crossing fill never
        // pays more than that, and a fill that barely crosses pays almost nothing.
        let flat = Pool {
            tvl: 10_000,
            util_pay_bp: 0,
            util_rec_bp: 0,
        };
        assert_eq!(
            quote(684, 671, Tenor::D90, Leg::Pay, 2_500, &flat, &P)
                .unwrap()
                .demand_bp,
            6
        );
        let barely = Pool {
            tvl: 10_000,
            util_pay_bp: 0,
            util_rec_bp: 2_400,
        };
        assert_eq!(
            quote(684, 671, Tenor::D90, Leg::Pay, 2_500, &barely, &P)
                .unwrap()
                .demand_bp,
            0
        );
        // The mid quote carries no demand term at all (L-22).
        assert_eq!(
            quote_mid(684, 671, Tenor::D90, Leg::Pay, &P).unwrap(),
            684 + 31 + 7
        );
        assert_eq!(
            quote_mid(684, 671, Tenor::D90, Leg::Receive, &P).unwrap(),
            671 - 14 - 7
        );
        let q = quote(684, 671, Tenor::D90, Leg::Pay, 2_000, &cross, &P).unwrap();
        assert!(q.reduces_imbalance && q.demand_bp == 0);
        // A notional far above the pool is clamped rather than failing (maths finding M-14); the caps refuse it.
        let tiny = Pool {
            tvl: 1,
            util_pay_bp: 0,
            util_rec_bp: 0,
        };
        let q = quote(684, 671, Tenor::D90, Leg::Pay, u64::MAX, &tiny, &P).unwrap();
        assert_eq!(q.imbalance_after_bp, i32::try_from(D_CLAMP_BP).unwrap());
        assert_eq!(leg_capacity(&tiny, Leg::Pay), 0);
        // Within the caps the largest reachable spread is a small extending trade at the leg cap:
        // rhu(45 · (4 799 + 4 800) / 20 000) = 22 bp; the 60 bp cap is not reachable (maths finding M-11).
        let edge = Pool {
            tvl: 10_000,
            util_pay_bp: 4_799,
            util_rec_bp: 0,
        };
        let q = quote(684, 671, Tenor::D90, Leg::Pay, 1, &edge, &P).unwrap();
        assert_eq!((q.imbalance_after_bp, q.demand_bp), (4_800, 22));
    }

    /// Basis swaps: the correlation offset reduces both legs' demand components by the same fraction and
    /// nothing else; 0 is the identity, 10 000 removes all demand, above that is refused.
    /// Twice the average signed imbalance a fill of `d` bp pays from `before` (pay leg), unrounded and uncapped.
    fn twice_avg_pay(before: i64, d: i64) -> i64 {
        (2 * before + d).max(0)
    }

    #[test]
    fn crossing_fill_pays_the_same_whether_traded_whole_or_at_the_mirror_point() {
        // External scan 2, finding 19: with the earlier `|after|² / (|before| + |after|)` form a trader who opened
        // 4 000 000 (free, reducing) and then 500 000 paid about 1.1 bp notional-weighted against 3 bp for the
        // whole 4 500 000. Now both routes price the same area: the whole fill pays rhu(45 · 500 / 20 000) = 1 bp
        // on 4 500 000 (exact 1.125 bp, 5 062.5 bp·USDC) and the second piece pays rhu(45 · 4 500 / 20 000) = 10 bp
        // on 500 000 (exact 10.125 bp, the same 5 062.5 bp·USDC).
        let pool = Pool {
            tvl: 10_000_000,
            util_pay_bp: 0,
            util_rec_bp: 2_000,
        };
        let whole = quote(684, 671, Tenor::D180, Leg::Pay, 4_500_000, &pool, &P).unwrap();
        assert_eq!((whole.imbalance_after_bp, whole.demand_bp), (2_500, 1));
        let first = quote(684, 671, Tenor::D180, Leg::Pay, 4_000_000, &pool, &P).unwrap();
        assert!(first.reduces_imbalance && first.demand_bp == 0);
        let moved = Pool {
            tvl: 10_000_000,
            util_pay_bp: 4_000,
            util_rec_bp: 2_000,
        };
        let second = quote(684, 671, Tenor::D180, Leg::Pay, 500_000, &moved, &P).unwrap();
        assert_eq!((second.imbalance_after_bp, second.demand_bp), (2_500, 10));
        assert_eq!(
            twice_avg_pay(-2_000, 4_500) * 4_500,
            twice_avg_pay(2_000, 500) * 500
        );
        // The same end state reached from a balanced book costs the area from zero: 2 500 bp over 2 500, which is
        // rhu(45 · 2 500 / 20 000) = 6 bp, more than the crossing fill pays for the part it actually extends.
        let flat = Pool {
            tvl: 10_000_000,
            util_pay_bp: 0,
            util_rec_bp: 0,
        };
        assert_eq!(
            quote(684, 671, Tenor::D180, Leg::Pay, 2_500_000, &flat, &P)
                .unwrap()
                .demand_bp,
            6
        );
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(4_000))]
        /// The signed trapezoid telescopes: pieces that each extend the book pay exactly what the whole pays
        /// before rounding, and no split of any fill pays less than the whole, because a reducing piece is
        /// charged nothing rather than credited (external scan 2, finding 19).
        #[test]
        fn splitting_a_fill_never_pays_less_than_the_whole(
            util_pay in 0u16..=4_800,
            util_rec in 0u16..=4_800,
            pieces in proptest::collection::vec(1u64..=1_000, 1..=4),
            pay in any::<bool>(),
        ) {
            let leg = if pay { Leg::Pay } else { Leg::Receive };
            let tvl = 10_000u64;
            let mut pool = Pool { tvl, util_pay_bp: util_pay, util_rec_bp: util_rec };
            let total: u64 = pieces.iter().sum();
            let whole = demand_bp(&pool, leg, total, &P).unwrap();
            let s = |before: i32| -> i64 { if pay { i64::from(before) } else { -i64::from(before) } };
            let whole_area = twice_avg_pay(s(whole.1), i64::try_from(total).unwrap()) * i64::try_from(total).unwrap();
            let mut split_area = 0i64;
            let mut all_extend = true;
            for piece in &pieces {
                let (dem, before, after, reduces) = demand_bp(&pool, leg, *piece, &P).unwrap();
                let d = i64::try_from(*piece).unwrap();
                let twice = twice_avg_pay(s(before), d);
                prop_assert_eq!(
                    u64::from(dem),
                    if reduces { 0 } else { ((45 * u64::try_from(twice).unwrap() + 10_000) / 20_000).min(60) }
                );
                all_extend &= !reduces;
                split_area += twice * d;
                // Apply the fill to the book exactly (tvl = 10 000 so one unit of notional is one bp).
                let after_pay = if pay { i64::from(pool.util_pay_bp) + d } else { i64::from(pool.util_pay_bp) };
                let after_rec = if pay { i64::from(pool.util_rec_bp) } else { i64::from(pool.util_rec_bp) + d };
                prop_assert_eq!(after_pay - after_rec, i64::from(after));
                pool.util_pay_bp = u16::try_from(after_pay).unwrap();
                pool.util_rec_bp = u16::try_from(after_rec).unwrap();
            }
            prop_assert!(split_area >= whole_area, "split {split_area} < whole {whole_area}");
            if all_extend {
                prop_assert_eq!(split_area, whole_area);
            }
        }
    }

    #[test]
    fn basis_offset_is_symmetric_bounded_and_touches_demand_only() {
        // Pool A has a pay-heavy book, so the pay leg pays 9 bp of demand; pool B is balanced, so the receive
        // leg pays 6 bp from a flat book (2 500 of 10 000 TVL).
        let pool_a = Pool {
            tvl: 12_640_000,
            util_pay_bp: 4_100,
            util_rec_bp: 2_100,
        };
        let pool_b = Pool {
            tvl: 10_000,
            util_pay_bp: 0,
            util_rec_bp: 0,
        };
        let a = BasisSide {
            spot_bp: 684,
            ema_bp: 671,
            pool: &pool_a,
            params: &P,
        };
        let b = BasisSide {
            spot_bp: 420,
            ema_bp: 430,
            pool: &pool_b,
            params: &P,
        };
        // Pool B is quoted on 2 500 of notional; pool A on 100 000. Use the same notional for both sides by
        // scaling pool B up: 100 000 of 400 000 TVL is the same 2 500 bp fill.
        let pool_b = Pool {
            tvl: 400_000,
            ..pool_b
        };
        let b = BasisSide { pool: &pool_b, ..b };
        let plain = quote_basis(&a, &b, Tenor::D90, 100_000, 0).unwrap();
        let pay0 = quote(684, 671, Tenor::D90, Leg::Pay, 100_000, &pool_a, &P).unwrap();
        let rec0 = quote(420, 430, Tenor::D90, Leg::Receive, 100_000, &pool_b, &P).unwrap();
        assert_eq!(plain.pay, pay0);
        assert_eq!(plain.receive, rec0);
        assert_eq!((pay0.demand_bp, rec0.demand_bp), (9, -6));
        assert_eq!(
            (
                plain.demand_before_bp,
                plain.combined_demand_bp,
                plain.offset_bp
            ),
            (15, 15, 0)
        );

        // Half offset: floor(9 / 2) = 4 off the pay leg, 3 off the receive leg; the fixed rates move by
        // exactly those amounts and in opposite directions; model, term and reference are unchanged.
        let half = quote_basis(&a, &b, Tenor::D90, 100_000, 5_000).unwrap();
        assert_eq!(half.pay.demand_bp, 5);
        assert_eq!(half.pay.fixed_bp, pay0.fixed_bp - 4);
        assert_eq!(half.receive.demand_bp, -3);
        assert_eq!(half.receive.fixed_bp, rec0.fixed_bp + 3);
        assert_eq!(
            (half.pay.reference_bp, half.pay.model_bp, half.pay.term_bp),
            (pay0.reference_bp, pay0.model_bp, pay0.term_bp)
        );
        assert_eq!(
            (
                half.receive.reference_bp,
                half.receive.model_bp,
                half.receive.term_bp
            ),
            (rec0.reference_bp, rec0.model_bp, rec0.term_bp)
        );
        assert_eq!(
            (
                half.demand_before_bp,
                half.combined_demand_bp,
                half.offset_bp
            ),
            (15, 8, 7)
        );

        // Full offset: no demand on either leg; the quotes are the mid quotes.
        let full = quote_basis(&a, &b, Tenor::D90, 100_000, 10_000).unwrap();
        assert_eq!((full.pay.demand_bp, full.receive.demand_bp), (0, 0));
        assert_eq!(
            full.pay.fixed_bp,
            quote_mid(684, 671, Tenor::D90, Leg::Pay, &P).unwrap()
        );
        assert_eq!(
            full.receive.fixed_bp,
            quote_mid(420, 430, Tenor::D90, Leg::Receive, &P).unwrap()
        );
        assert_eq!(full.combined_demand_bp, 0);

        // Symmetry: swapping which pool carries the imbalance moves the offset to the other leg, and the
        // combined figures are the same.
        let a2 = BasisSide {
            spot_bp: 684,
            ema_bp: 671,
            pool: &pool_b,
            params: &P,
        };
        let pool_a_rec = Pool {
            tvl: 12_640_000,
            util_pay_bp: 2_100,
            util_rec_bp: 4_100,
        };
        let b2 = BasisSide {
            spot_bp: 420,
            ema_bp: 430,
            pool: &pool_a_rec,
            params: &P,
        };
        let mirrored = quote_basis(&a2, &b2, Tenor::D90, 100_000, 5_000).unwrap();
        assert_eq!(
            (mirrored.pay.demand_bp, mirrored.receive.demand_bp),
            (3, -5)
        );
        assert_eq!(
            (
                mirrored.demand_before_bp,
                mirrored.combined_demand_bp,
                mirrored.offset_bp
            ),
            (15, 8, 7)
        );

        // Bounds.
        assert_eq!(
            quote_basis(&a, &b, Tenor::D90, 100_000, 10_001),
            Err(VernierError::CorrelationOutOfRange)
        );
        assert_eq!(
            demand_offset(9, 10_001),
            Err(VernierError::CorrelationOutOfRange)
        );
        assert_eq!(demand_offset(u16::MAX, 10_000), Ok(u16::MAX));
        assert_eq!(demand_offset(9, 0), Ok(0));
        // Monotone in the offset and never larger than the demand.
        let mut prev = 0;
        for c in (0..=10_000).step_by(250) {
            let r = demand_offset(22, c).unwrap();
            assert!(r >= prev && r <= 22);
            prev = r;
        }
    }

    #[test]
    fn rejects_malformed_pool() {
        assert_eq!(
            demand_bp(
                &Pool {
                    tvl: 0,
                    util_pay_bp: 0,
                    util_rec_bp: 0
                },
                Leg::Pay,
                1,
                &P
            ),
            Err(VernierError::EmptyPool)
        );
        assert_eq!(
            demand_bp(
                &Pool {
                    tvl: 1,
                    util_pay_bp: 10_001,
                    util_rec_bp: 0
                },
                Leg::Pay,
                1,
                &P
            ),
            Err(VernierError::MalformedUtilisation)
        );
    }

    fn arb_pool() -> impl Strategy<Value = Pool> {
        (1u64..50_000_000, 0u16..=4_800)
            .prop_flat_map(|(tvl, pay)| (Just(tvl), Just(pay), 0u16..=(4_800u16.min(8_000 - pay))))
            .prop_map(|(tvl, util_pay_bp, util_rec_bp)| Pool {
                tvl,
                util_pay_bp,
                util_rec_bp,
            })
    }
    fn arb_leg() -> impl Strategy<Value = Leg> {
        prop_oneof![Just(Leg::Pay), Just(Leg::Receive)]
    }
    fn arb_tenor() -> impl Strategy<Value = Tenor> {
        prop_oneof![
            Just(Tenor::D28),
            Just(Tenor::D60),
            Just(Tenor::D90),
            Just(Tenor::D180)
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(20_000))]

        #[test]
        fn demand_is_bounded(pool in arb_pool(), leg in arb_leg(), n in 0u64..50_000_000) {
            let (d, ..) = demand_bp(&pool, leg, n, &P).unwrap();
            prop_assert!(d <= P.demand_cap_bp);
        }

        /// Maths finding M-11: inside the caps the demand spread never exceeds 22 bp at the default
        /// calibration, whatever the trade; the 60 bp cap is documentation of a bound, not a binding limit.
        #[test]
        fn demand_within_caps_is_at_most_22bp(pool in arb_pool(), leg in arb_leg(), n in 0u64..u64::MAX) {
            let n = n.min(leg_capacity(&pool, leg));
            let (d, ..) = demand_bp(&pool, leg, n, &P).unwrap();
            prop_assert!(d <= DEFAULT_DEMAND_MAX_BP, "{d}");
        }

        #[test]
        fn reducing_trades_pay_no_demand(pool in arb_pool(), leg in arb_leg(), n in 0u64..50_000_000) {
            let (d, before, after, reduces) = demand_bp(&pool, leg, n, &P).unwrap();
            if after.unsigned_abs() <= before.unsigned_abs() { prop_assert!(reduces); prop_assert_eq!(d, 0); }
        }

        #[test]
        fn pay_monotone_up_receive_monotone_down(pool in arb_pool(), s in 0u16..3000, e in 0u16..3000, t in arb_tenor(), a in 0u64..2_000_000, extra in 0u64..2_000_000) {
            let b = a + extra;
            prop_assert!(quote(s, e, t, Leg::Pay, b, &pool, &P).unwrap().fixed_bp >= quote(s, e, t, Leg::Pay, a, &pool, &P).unwrap().fixed_bp);
            prop_assert!(quote(s, e, t, Leg::Receive, b, &pool, &P).unwrap().fixed_bp <= quote(s, e, t, Leg::Receive, a, &pool, &P).unwrap().fixed_bp);
        }

        #[test]
        fn no_free_lunch(pool in arb_pool(), s in 0u16..3000, e in 0u16..3000, t in arb_tenor(), n in 0u64..5_000_000) {
            prop_assert!(quote(s, e, t, Leg::Pay, n, &pool, &P).unwrap().fixed_bp >= quote(s, e, t, Leg::Receive, n, &pool, &P).unwrap().fixed_bp);
        }

        #[test]
        fn reference_protects_pool(pool in arb_pool(), s in 0u16..3000, e in 0u16..3000, t in arb_tenor()) {
            prop_assert_eq!(quote(s, e, t, Leg::Pay, 1000, &pool, &P).unwrap().reference_bp, i32::from(s.max(e)));
            prop_assert_eq!(quote(s, e, t, Leg::Receive, 1000, &pool, &P).unwrap().reference_bp, i32::from(s.min(e)));
        }

        #[test]
        fn spread_bounded(pool in arb_pool(), s in 0u16..3000, e in 0u16..3000, t in arb_tenor(), leg in arb_leg(), n in 0u64..50_000_000) {
            let q = quote(s, e, t, leg, n, &pool, &P).unwrap();
            let model = match leg { Leg::Pay => P.model_pay_bp[t.ix()], Leg::Receive => P.model_rec_bp[t.ix()] };
            let bound = i32::from(model) + i32::from(P.demand_cap_bp) + i32::from(P.term_bp[t.ix()]);
            prop_assert!((q.fixed_bp - q.reference_bp).abs() <= bound);
        }

        #[test]
        fn term_structure_monotone_in_tenor(pool in arb_pool(), s in 0u16..3000, e in 0u16..3000, leg in arb_leg(), n in 0u64..1_000_000) {
            let mut prev = -1i32;
            for t in TENORS { let q = quote(s, e, t, leg, n, &pool, &P).unwrap(); let sp = (q.fixed_bp - q.reference_bp).abs(); prop_assert!(sp >= prev); prev = sp; }
        }

        #[test]
        fn capacity_respects_caps(pool in arb_pool(), leg in arb_leg()) {
            let cap = leg_capacity(&pool, leg);
            let add_bp = cap * 10_000 / pool.tvl;
            let used = match leg { Leg::Pay => pool.util_pay_bp, Leg::Receive => pool.util_rec_bp };
            prop_assert!(u64::from(used) + add_bp <= u64::from(CAP_LEG_BP) + 1);
            prop_assert!(u64::from(pool.util_pay_bp) + u64::from(pool.util_rec_bp) + add_bp <= u64::from(CAP_TOTAL_BP) + 1);
        }

        /// The offset only ever removes demand: each leg's fixed rate moves towards its mid by at most its
        /// demand, model and term are unchanged, and the combined demand never increases with the offset.
        #[test]
        fn basis_offset_never_exceeds_demand(pa in arb_pool(), pb in arb_pool(), s in 0u16..3000, e in 0u16..3000, t in arb_tenor(), n in 0u64..5_000_000, c in 0u16..=10_000) {
            let a = BasisSide { spot_bp: s, ema_bp: e, pool: &pa, params: &P };
            let b = BasisSide { spot_bp: e, ema_bp: s, pool: &pb, params: &P };
            let q0 = quote_basis(&a, &b, t, n, 0).unwrap();
            let q = quote_basis(&a, &b, t, n, c).unwrap();
            prop_assert!(q.pay.fixed_bp <= q0.pay.fixed_bp && q.pay.fixed_bp >= q0.pay.fixed_bp - q0.pay.demand_bp);
            prop_assert!(q.receive.fixed_bp >= q0.receive.fixed_bp && q.receive.fixed_bp <= q0.receive.fixed_bp - q0.receive.demand_bp);
            prop_assert_eq!((q.pay.model_bp, q.pay.term_bp, q.receive.model_bp, q.receive.term_bp), (q0.pay.model_bp, q0.pay.term_bp, q0.receive.model_bp, q0.receive.term_bp));
            prop_assert!(q.combined_demand_bp <= q.demand_before_bp);
            prop_assert_eq!(q.combined_demand_bp + q.offset_bp, q.demand_before_bp);
            prop_assert_eq!(i32::from(q.combined_demand_bp), q.pay.demand_bp - q.receive.demand_bp);
            prop_assert!(q.pay.fixed_bp - q.pay.demand_bp == q0.pay.fixed_bp - q0.pay.demand_bp);
        }

        #[test]
        fn collateral_monotone(a in 0u64..2_000_000, extra in 0u64..2_000_000) {
            let mut prev = 0u64;
            for t in TENORS { prop_assert!(collateral(a + extra, t, &P) >= collateral(a, t, &P)); prop_assert!(collateral(a, t, &P) >= prev); prev = collateral(a, t, &P); }
        }
    }
}

/// Kani bounded proofs. Run with `cargo kani -p vernier` (CI job `proofs`); they are not part of `cargo test`.
#[cfg(kani)]
mod proofs {
    use super::*;

    #[kani::proof]
    fn demand_never_exceeds_cap() {
        let pool = Pool {
            tvl: kani::any(),
            util_pay_bp: kani::any(),
            util_rec_bp: kani::any(),
        };
        kani::assume(pool.tvl > 0 && pool.util_pay_bp <= 10_000 && pool.util_rec_bp <= 10_000);
        let n: u64 = kani::any();
        let leg = if kani::any() { Leg::Pay } else { Leg::Receive };
        if let Ok((d, ..)) = demand_bp(&pool, leg, n, &DEFAULT_PARAMS) {
            assert!(d <= DEFAULT_PARAMS.demand_cap_bp);
        }
    }

    #[kani::proof]
    fn quote_never_panics_on_valid_pool() {
        let pool = Pool {
            tvl: kani::any(),
            util_pay_bp: kani::any(),
            util_rec_bp: kani::any(),
        };
        kani::assume(pool.tvl > 0 && pool.util_pay_bp <= 10_000 && pool.util_rec_bp <= 10_000);
        let _ = quote(
            kani::any(),
            kani::any(),
            Tenor::D90,
            Leg::Pay,
            kani::any(),
            &pool,
            &DEFAULT_PARAMS,
        );
    }
}
