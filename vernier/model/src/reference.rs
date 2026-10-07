//! Exact-arithmetic reference model of the Brink maths, written from `docs/audit/maths/SPEC.md`.
//!
//! Every quantity is first formed as an exact rational (`i128` numerator over a positive `i128`
//! denominator) and then rounded with an explicit, named rounding step. Nothing here mirrors the
//! structure of `programs/vernier/src/lib.rs` or `programs/swap_amm/src/instructions/*.rs`; the
//! file is deliberately self-contained so that an integration test can include it with `#[path]`.
//!
//! Conventions: rates are annualised basis points; utilisation is bp of pool capital; amounts are
//! integer base units (USDC at six decimals on chain, but the model is unit-agnostic).
#![allow(dead_code)]

/// Exact rational `n / d` with `d > 0`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Q {
    pub n: i128,
    pub d: i128,
}

impl Q {
    pub const fn new(n: i128, d: i128) -> Q {
        assert!(d > 0, "denominator must be positive");
        Q { n, d }
    }
    pub const fn int(n: i128) -> Q {
        Q { n, d: 1 }
    }
    /// floor(n / d) for any sign of `n`.
    pub fn floor(self) -> i128 {
        let q = self.n / self.d;
        if self.n % self.d != 0 && self.n < 0 {
            q - 1
        } else {
            q
        }
    }
    /// ceil(n / d) for any sign of `n`.
    pub fn ceil(self) -> i128 {
        let q = self.n / self.d;
        if self.n % self.d != 0 && self.n > 0 {
            q + 1
        } else {
            q
        }
    }
    /// Round half up: floor(n / d + 1/2), for non-negative `n` only (the code never rounds a signed value).
    pub fn round_half_up(self) -> i128 {
        assert!(
            self.n >= 0,
            "round_half_up is defined for non-negative values"
        );
        Q::new(2 * self.n + self.d, 2 * self.d).floor()
    }
    pub fn mul_int(self, k: i128) -> Q {
        Q::new(self.n * k, self.d)
    }
    pub fn add_int(self, k: i128) -> Q {
        Q::new(self.n + k * self.d, self.d)
    }
    pub fn abs(self) -> Q {
        Q::new(self.n.abs(), self.d)
    }
    pub fn lt(self, other: Q) -> bool {
        self.n * other.d < other.n * self.d
    }
    pub fn le(self, other: Q) -> bool {
        self.n * other.d <= other.n * self.d
    }
}

pub const BP: i128 = 10_000;
pub const DAYS_PER_YEAR: i128 = 365;
pub const SECONDS_PER_DAY: i128 = 86_400;
pub const ACCRUAL_SCALE: i128 = 1_000_000_000_000_000_000;
pub const CAP_LEG_BP: i128 = 4_800;
pub const CAP_TOTAL_BP: i128 = 8_000;
pub const D_CLAMP_BP: i128 = 30_000;
pub const OPENING_FEE_BP: i128 = 5;
pub const INCOME_FEE_PCT: i128 = 10;
pub const LP_EXIT_FEE_BP: i128 = 50;
pub const BUYBACK_BP: i128 = 5_000;
pub const LIQUIDATION_LOSS_BP: i128 = 9_900;
pub const CRANK_BOUNTY_BP: i128 = 2;
pub const CRANK_BOUNTY_CAP: i128 = 25_000_000;
pub const MAX_RATE_BP: i128 = 30_000;
pub const I32_MAX: i128 = i32::MAX as i128;
pub const U16_MAX: i128 = u16::MAX as i128;
pub const U64_MAX: i128 = u64::MAX as i128;
pub const I64_MAX: i128 = i64::MAX as i128;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    PayFixed,
    ReceiveFixed,
}

impl Side {
    /// +1 for pay fixed (adds to imbalance), −1 for receive fixed.
    pub const fn sign(self) -> i128 {
        match self {
            Side::PayFixed => 1,
            Side::ReceiveFixed => -1,
        }
    }
    pub const fn other(self) -> Side {
        match self {
            Side::PayFixed => Side::ReceiveFixed,
            Side::ReceiveFixed => Side::PayFixed,
        }
    }
}

pub const TENOR_DAYS: [i128; 4] = [28, 60, 90, 180];

/// Calibration, as integers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Calibration {
    pub model_pay_bp: [i128; 4],
    pub model_rec_bp: [i128; 4],
    pub term_bp: [i128; 4],
    pub demand_k_bp: i128,
    pub demand_cap_bp: i128,
    pub collateral_bp: [i128; 4],
}

pub const DEFAULT_CALIBRATION: Calibration = Calibration {
    model_pay_bp: [11, 22, 31, 54],
    model_rec_bp: [9, 12, 14, 19],
    term_bp: [3, 5, 7, 12],
    demand_k_bp: 45,
    demand_cap_bp: 60,
    collateral_bp: [120, 230, 330, 600],
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefError {
    EmptyPool,
    MalformedUtilisation,
    Overflow,
}

/// Everything a quote exposes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RefQuote {
    pub reference_bp: i128,
    pub model_bp: i128,
    pub demand_bp: i128,
    pub term_bp: i128,
    pub fixed_bp: i128,
    pub imbalance_before_bp: i128,
    pub imbalance_after_bp: i128,
    pub reduces_imbalance: bool,
}

/// SPEC section 3: demand spread (trapezoid form, maths finding M-3).
///
/// before = util_pay − util_rec
/// d      = ceil(notional · 10^4 / tvl)
/// after  = before + sign(side) · d
/// reduces = |after| ≤ |before|
/// s_before = before (pay) or −before (receive)
/// twice_avg = max(0, 2 · s_before + d)       the signed trapezoid (after² − before²) / d
///             (external scan 1, L-23, as amended by external scan 2, finding 19)
/// demand  = 0 if reduces, else min(cap, round_half_up(k · twice_avg / (2 · 10^4)))
///
/// Domain bounds stated by the SPEC: tvl > 0; util ≤ 10^4 each. `d` is clamped at 30 000 so `after` lies in
/// [−40 000, 40 000] and no Overflow path remains.
pub fn demand(
    tvl: i128,
    util_pay_bp: i128,
    util_rec_bp: i128,
    side: Side,
    notional: i128,
    c: &Calibration,
) -> Result<(i128, i128, i128, bool), RefError> {
    if tvl <= 0 {
        return Err(RefError::EmptyPool);
    }
    if util_pay_bp > BP || util_rec_bp > BP || util_pay_bp < 0 || util_rec_bp < 0 {
        return Err(RefError::MalformedUtilisation);
    }
    let before = util_pay_bp - util_rec_bp;
    // Patch 0003 (M-3, M-14): size rounds up and is clamped at 30 000 bp of TVL, so the function is total.
    let d = Q::new(notional * BP, tvl).ceil().min(D_CLAMP_BP);
    let after = before + side.sign() * d;
    let reduces = after.abs() <= before.abs();
    let demand = if reduces {
        0
    } else {
        // Twice the average signed imbalance over the fill in the direction of the leg: the exact trapezoid of
        // the imbalance from `before` to `after`, so the charge telescopes over any split of the fill (external
        // scan 2, finding 19). Equal to |before| + |after| on one side of zero and |after| − |before| for a
        // crossing fill that extends the book.
        let s_before = side.sign() * before;
        let twice_avg = (2 * s_before + d).max(0);
        let raw = Q::new(c.demand_k_bp * twice_avg, 2 * BP).round_half_up();
        raw.min(c.demand_cap_bp)
    };
    Ok((demand, before, after, reduces))
}

/// SPEC section 4: the quote.
///
/// pay:     fixed = max(spot, ema) + model_pay[t] + demand + term[t]
/// receive: fixed = min(spot, ema) − model_rec[t] − demand − term[t]
#[allow(clippy::too_many_arguments)]
pub fn quote(
    spot_bp: i128,
    ema_bp: i128,
    tenor_ix: usize,
    side: Side,
    notional: i128,
    tvl: i128,
    util_pay_bp: i128,
    util_rec_bp: i128,
    c: &Calibration,
) -> Result<RefQuote, RefError> {
    let (demand, before, after, reduces) =
        demand(tvl, util_pay_bp, util_rec_bp, side, notional, c)?;
    let s = side.sign();
    let (reference, model) = match side {
        Side::PayFixed => (spot_bp.max(ema_bp), c.model_pay_bp[tenor_ix]),
        Side::ReceiveFixed => (spot_bp.min(ema_bp), c.model_rec_bp[tenor_ix]),
    };
    let term = c.term_bp[tenor_ix];
    Ok(RefQuote {
        reference_bp: reference,
        model_bp: s * model,
        demand_bp: s * demand,
        term_bp: s * term,
        fixed_bp: reference + s * (model + demand + term),
        imbalance_before_bp: before,
        imbalance_after_bp: after,
        reduces_imbalance: reduces,
    })
}

/// SPEC section 5: collateral = ceil(notional · collateral_bp[t] / 10^4), saturating at u64::MAX.
pub fn collateral(notional: i128, tenor_ix: usize, c: &Calibration) -> i128 {
    Q::new(notional * c.collateral_bp[tenor_ix], BP)
        .ceil()
        .min(U64_MAX)
}

/// SPEC section 6: capacity = floor(min(cap_leg − util_leg, cap_total − util_pay − util_rec)⁺ · tvl / 10^4).
pub fn leg_capacity(tvl: i128, util_pay_bp: i128, util_rec_bp: i128, side: Side) -> i128 {
    let used = match side {
        Side::PayFixed => util_pay_bp,
        Side::ReceiveFixed => util_rec_bp,
    };
    let leg_room = (CAP_LEG_BP - used).max(0);
    let total_room = (CAP_TOTAL_BP - util_pay_bp - util_rec_bp).max(0);
    let room = leg_room.min(total_room);
    Q::new(room * tvl, BP).floor().min(U64_MAX)
}

/// SPEC section 6: caps hold.
pub fn caps_hold(util_pay_bp: i128, util_rec_bp: i128) -> bool {
    util_pay_bp <= CAP_LEG_BP
        && util_rec_bp <= CAP_LEG_BP
        && util_pay_bp + util_rec_bp <= CAP_TOTAL_BP
}

/// SPEC section 7: utilisation = floor(open_notional · 10^4 / tvl); must fit u16.
pub fn utilisation(open_notional: i128, tvl: i128) -> Option<i128> {
    if tvl <= 0 {
        return if open_notional == 0 { Some(0) } else { None };
    }
    let u = Q::new(open_notional * BP, tvl).floor();
    (u <= U16_MAX).then_some(u)
}

/// SPEC section 8: x · bp / 10^4 rounded down.
pub fn mul_bp(x: i128, bp: i128) -> i128 {
    Q::new(x * bp, BP).floor()
}

/// SPEC section 9: trader pnl for a rate difference held over `days`, magnitude floored, clamped to `bound`.
///
/// pnl = sign(diff) · min(bound, floor(|diff| · notional · days / (10^4 · 365)))
/// Returns None where the implementation reports Overflow (magnitude after the clamp above i64::MAX).
pub fn pnl(diff_bp: i128, notional: i128, days: i128, bound: i128) -> Option<i128> {
    let mag = Q::new(diff_bp.abs() * notional * days, BP * DAYS_PER_YEAR).floor();
    let mag = mag.min(U64_MAX).min(bound);
    if mag > I64_MAX {
        return None;
    }
    Some(if diff_bp < 0 { -mag } else { mag })
}

/// SPEC section 9: oriented difference, floating − fixed for pay fixed and fixed − floating for receive fixed.
pub fn oriented_diff(side: Side, floating_bp: i128, fixed_bp: i128) -> i128 {
    side.sign() * (floating_bp - fixed_bp)
}

/// SPEC section 9: days remaining = clamp(ceil(max(0, matures − now) / 86 400), 0, total).
pub fn days_remaining(now: i128, matures_ts: i128, total_days: i128) -> i128 {
    let secs = (matures_ts - now).max(0);
    Q::new(secs, SECONDS_PER_DAY).ceil().min(total_days)
}

/// SPEC section 10: cumulative accrual at `now`, last value held flat; no extrapolation backwards.
pub fn accrual_at(accrual_e18: i128, value_bp: i128, unix_ts: i128, now: i128) -> i128 {
    accrual_e18 + value_bp * (now - unix_ts).max(0) * ACCRUAL_SCALE
}

/// SPEC section 10: average rate = floor((end − start) / (scale · seconds)), seconds > 0.
pub fn average_bp(start: i128, end: i128, seconds: i128) -> Option<i128> {
    if seconds <= 0 || end < start {
        return None;
    }
    Some(Q::new(end - start, ACCRUAL_SCALE * seconds).floor())
}

/// SPEC section 11 (patch 0006, finding M-6): EMA step at milli-bp,
/// new_milli = rhu((ema_milli · hl + value · 1000 · dt) / (hl + dt)); the public view is rhu(new_milli / 1000).
pub fn ema_step_milli(ema_milli: i128, value_bp: i128, hl_slots: i128, dt_slots: i128) -> i128 {
    Q::new(
        ema_milli * hl_slots + value_bp * 1_000 * dt_slots,
        hl_slots + dt_slots,
    )
    .round_half_up()
}
/// Public view of the milli-bp EMA.
pub fn ema_view_bp(ema_milli: i128) -> i128 {
    Q::new(ema_milli, 1_000).round_half_up()
}
/// Kept for the pre-patch comparison: new = floor((ema · hl + value · dt) / (hl + dt)).
pub fn ema_step(ema_bp: i128, value_bp: i128, hl_slots: i128, dt_slots: i128) -> i128 {
    Q::new(ema_bp * hl_slots + value_bp * dt_slots, hl_slots + dt_slots).floor()
}

/// SPEC section 12: fee split, buyback = floor(fee · 5000 / 10^4), treasury = fee − buyback.
pub fn split(fee: i128) -> (i128, i128) {
    let b = Q::new(fee * BUYBACK_BP, BP).floor();
    (b, fee - b)
}

/// SPEC section 13: shares for a deposit, floor(amount · supply / tvl); 1:1 when supply or tvl is zero.
pub fn shares_for(amount: i128, tvl: i128, supply: i128) -> i128 {
    if supply == 0 || tvl == 0 {
        return amount;
    }
    Q::new(amount * supply, tvl).floor()
}

/// SPEC section 13: amount for shares, floor(shares · tvl / supply); supply must be positive.
pub fn amount_for(shares: i128, tvl: i128, supply: i128) -> Option<i128> {
    if supply <= 0 {
        return None;
    }
    Some(Q::new(shares * tvl, supply).floor())
}

/// SPEC section 13: share price scaled by 10^6, floor; 10^6 for an empty supply.
pub fn share_price_e6(tvl: i128, supply: i128) -> i128 {
    if supply == 0 {
        return 1_000_000;
    }
    Q::new(tvl * 1_000_000, supply).floor()
}

/// SPEC section 14: the close of a swap, as pool cash accounting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CloseResult {
    pub pnl_applied: i128,
    pub gain: i128,
    pub loss: i128,
    pub income_fee: i128,
    pub bounty: i128,
    pub payout: i128,
    pub tvl_after: i128,
    pub collateral_held_after: i128,
}

/// pnl is already in [−collateral, +collateral]. gain is additionally capped at tvl.
/// fee = floor(gain · 10 / 100); gross = collateral + gain − loss − fee;
/// bounty = min(min(floor(notional · 2 / 10^4), 25 USDC), gross) when eligible; payout = gross − bounty;
/// tvl' = tvl + loss − gain; collateral_held' = collateral_held − collateral.
pub fn close(
    pnl: i128,
    collateral: i128,
    notional: i128,
    tvl: i128,
    collateral_held: i128,
    bounty_eligible: bool,
) -> CloseResult {
    let pnl = if pnl > 0 { pnl.min(tvl) } else { pnl };
    let gain = pnl.max(0);
    let loss = (-pnl).max(0);
    let income_fee = Q::new(gain * INCOME_FEE_PCT, 100).floor();
    let gross = collateral + gain - loss - income_fee;
    let bounty = if bounty_eligible {
        mul_bp(notional, CRANK_BOUNTY_BP)
            .min(CRANK_BOUNTY_CAP)
            .min(gross)
    } else {
        0
    };
    CloseResult {
        pnl_applied: pnl,
        gain,
        loss,
        income_fee,
        bounty,
        payout: gross - bounty,
        tvl_after: tvl + loss - gain,
        collateral_held_after: collateral_held - collateral,
    }
}

/// SPEC section 15: calibration step bound, floor(old / 2) ≤ new ≤ floor(3 · old / 2) + 1.
pub fn within_step(old: i128, new: i128) -> bool {
    new >= Q::new(old, 2).floor() && new <= Q::new(3 * old, 2).floor() + 1
}
