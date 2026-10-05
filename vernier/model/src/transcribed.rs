//! Line-for-line transcriptions of the integer maths in `programs/swap_amm/src/instructions/{math,fees,swap}.rs`,
//! `programs/swap_amm/src/instructions/admin.rs` and `programs/brink_index/src/lib.rs`, with Anchor's
//! `Result`/`require!` replaced by `Option`. The types, casts and rounding operators are kept exactly as in
//! the source so that the reference model can be differenced against the program's arithmetic without
//! compiling the Anchor crates. Each function names its source. These are the implementation under test,
//! not the reference.
#![allow(dead_code)]

pub const SECONDS_PER_DAY: i64 = 86_400;
pub const DAYS_PER_YEAR: u128 = 365;
pub const BP: u128 = 10_000;
pub const ACCRUAL_SCALE: u128 = 1_000_000_000_000_000_000;
pub const BUYBACK_BP: u64 = 5_000;
pub const LIQUIDATION_LOSS_BP: u64 = 9_900;
pub const CRANK_BOUNTY_BP: u32 = 2;
pub const CRANK_BOUNTY_CAP: u64 = 25_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LegKind {
    PayFixed,
    ReceiveFixed,
}

/// math.rs `mul_bp`
pub fn mul_bp(x: u64, bp: u32) -> Option<u64> {
    u64::try_from(u128::from(x).checked_mul(u128::from(bp))? / 10_000).ok()
}
/// math.rs `shares_for`
pub fn shares_for(amount: u64, tvl: u64, supply: u64) -> Option<u64> {
    if supply == 0 || tvl == 0 {
        return Some(amount);
    }
    u64::try_from(
        u128::from(amount)
            .checked_mul(u128::from(supply))?
            .checked_div(u128::from(tvl))?,
    )
    .ok()
}
/// math.rs `amount_for`
pub fn amount_for(shares: u64, tvl: u64, supply: u64) -> Option<u64> {
    if supply == 0 {
        return None;
    }
    u64::try_from(
        u128::from(shares)
            .checked_mul(u128::from(tvl))?
            .checked_div(u128::from(supply))?,
    )
    .ok()
}
/// math.rs `share_price_e6`
pub fn share_price_e6(tvl: u64, supply: u64) -> Option<u64> {
    if supply == 0 {
        return Some(1_000_000);
    }
    u64::try_from(
        u128::from(tvl)
            .checked_mul(1_000_000)?
            .checked_div(u128::from(supply))?,
    )
    .ok()
}
/// math.rs `rebase_utilisation` (the per-leg closure), returning the u16 or None for `PoolInvariant`.
pub fn util_of(open_notional: u64, tvl: u64) -> Option<u16> {
    if tvl == 0 {
        return if open_notional == 0 { Some(0) } else { None };
    }
    u16::try_from(
        u128::from(open_notional)
            .checked_mul(BP)?
            .checked_div(u128::from(tvl))?,
    )
    .ok()
}
/// math.rs `pnl_bounded`
pub fn pnl_bounded(diff_bp: i64, notional: u64, days: u16, collateral: u64) -> Option<i64> {
    let mag = u128::from(diff_bp.unsigned_abs())
        .checked_mul(u128::from(notional))?
        .checked_mul(u128::from(days))?
        .checked_div(BP.checked_mul(DAYS_PER_YEAR)?)?;
    let mag = u64::try_from(mag).unwrap_or(u64::MAX).min(collateral);
    let mag = i64::try_from(mag).ok()?;
    Some(if diff_bp < 0 { mag.checked_neg()? } else { mag })
}
/// math.rs `oriented_diff`
pub fn oriented_diff(leg: LegKind, floating_bp: i64, fixed_bp: i64) -> Option<i64> {
    let d = floating_bp.checked_sub(fixed_bp)?;
    Some(match leg {
        LegKind::PayFixed => d,
        LegKind::ReceiveFixed => d.checked_neg()?,
    })
}
/// math.rs `days_remaining`
pub fn days_remaining(now: i64, matures_ts: i64, total_days: u16) -> u16 {
    let secs = matures_ts.saturating_sub(now).max(0);
    let d = (secs.saturating_add(SECONDS_PER_DAY.saturating_sub(1))) / SECONDS_PER_DAY;
    u16::try_from(d).unwrap_or(total_days).min(total_days)
}
/// fees.rs `split`
pub fn split(fee: u64) -> (u64, u64) {
    let buyback =
        u64::try_from(u128::from(fee).saturating_mul(u128::from(BUYBACK_BP)) / 10_000).unwrap_or(0);
    (buyback, fee.saturating_sub(buyback))
}
/// swap.rs `accrual_at`
pub fn accrual_at(accrual_e18: u128, value_bp: u16, unix_ts: i64, now: i64) -> Option<u128> {
    let elapsed = u128::try_from(now.checked_sub(unix_ts)?.max(0)).ok()?;
    let add = u128::from(value_bp)
        .checked_mul(elapsed)
        .and_then(|x| x.checked_mul(ACCRUAL_SCALE))?;
    accrual_e18.checked_add(add)
}
/// swap.rs `average_bp`
pub fn average_bp(accrual_start: u128, accrual_end: u128, seconds: i64) -> Option<i64> {
    if seconds <= 0 {
        return None;
    }
    let secs = u128::try_from(seconds).ok()?;
    let num = accrual_end.checked_sub(accrual_start)?;
    let avg = num.checked_div(ACCRUAL_SCALE.checked_mul(secs)?)?;
    i64::try_from(avg).ok()
}
/// swap.rs `crank_bounty`
pub fn crank_bounty(notional: u64) -> Option<u64> {
    Some(mul_bp(notional, CRANK_BOUNTY_BP)?.min(CRANK_BOUNTY_CAP))
}
/// brink_index `publish`, the EMA update only.
pub fn ema_update(ema_bp: u16, value_bp: u16, half_life_slots: u64, dt_slots: u64) -> Option<u16> {
    let dt = u128::from(dt_slots);
    let hl = u128::from(half_life_slots);
    let ema = u128::from(ema_bp);
    let num = ema
        .checked_mul(hl)?
        .checked_add(u128::from(value_bp).checked_mul(dt)?)?;
    let new = num.checked_div(hl.checked_add(dt)?)?;
    u16::try_from(new).ok()
}

/// brink_index `publish` after patch 0008: the milli-bp EMA update; returns (ema_milli_bp, ema_bp).
pub fn ema_update_milli(ema_milli_bp: u32, value_bp: u16, half_life_slots: u64, dt_slots: u64) -> Option<(u32, u16)> {
    let dt = u128::from(dt_slots);
    let hl = u128::from(half_life_slots);
    let denom = hl.checked_add(dt)?;
    let num = u128::from(ema_milli_bp)
        .checked_mul(hl)?
        .checked_add(u128::from(value_bp).checked_mul(1_000)?.checked_mul(dt)?)?;
    let new_milli = num.checked_add(denom / 2)?.checked_div(denom)?;
    Some((u32::try_from(new_milli).ok()?, u16::try_from(new_milli.checked_add(500)? / 1_000).ok()?))
}
/// admin.rs `within_step`
pub fn within_step(old: u16, new: u16) -> bool {
    let lo = u32::from(old) / 2;
    let hi = (u32::from(old).saturating_mul(3) / 2).saturating_add(1);
    u32::from(new) >= lo && u32::from(new) <= hi
}

/// swap.rs `close`: the arithmetic part, with the pool fields passed in and returned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Closed {
    pub pnl: i64,
    pub gain: u64,
    pub loss: u64,
    pub fee: u64,
    pub bounty: u64,
    pub payout: u64,
    pub tvl: u64,
    pub collateral_held: u64,
    pub open_leg_notional: u64,
}
pub fn close(
    pnl: i64,
    swap_collateral: u64,
    swap_notional: u64,
    tvl: u64,
    collateral_held: u64,
    open_leg_notional: u64,
    bounty_eligible: bool,
) -> Option<Closed> {
    let pnl = if pnl > 0 {
        pnl.min(i64::try_from(tvl).unwrap_or(i64::MAX))
    } else {
        pnl
    };
    let gain = u64::try_from(pnl.max(0)).ok()?;
    let loss = u64::try_from(pnl.min(0).checked_neg()?).ok()?;
    let fee = u64::try_from(u128::from(gain).checked_mul(u128::from(10u32))? / 100).ok()?;
    let gross = swap_collateral
        .checked_add(gain)?
        .checked_sub(loss)?
        .checked_sub(fee)?;
    let bounty = if bounty_eligible {
        crank_bounty(swap_notional)?.min(gross)
    } else {
        0
    };
    let payout = gross.checked_sub(bounty)?;
    let open_leg_notional = open_leg_notional.checked_sub(swap_notional)?;
    let collateral_held = collateral_held.checked_sub(swap_collateral)?;
    let tvl = tvl.checked_add(loss)?.checked_sub(gain)?;
    Some(Closed {
        pnl,
        gain,
        loss,
        fee,
        bounty,
        payout,
        tvl,
        collateral_held,
        open_leg_notional,
    })
}

/// swap.rs `liquidate`: the threshold test. `unbounded` is the mark with bound u64::MAX.
pub fn exhausted(unbounded: i64, collateral: u64) -> Option<bool> {
    let threshold = i64::try_from(mul_bp(collateral, u32::try_from(LIQUIDATION_LOSS_BP).ok()?)?).ok()?;
    Some(unbounded <= threshold.checked_neg()?)
}
