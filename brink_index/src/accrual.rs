//! Cumulative-accrual reads against a benchmark's history: live segment, previous segment, daily fixings ring
//! and the deterministic fallback beyond it. Pure functions of the account, shared by the swap AMM (settlement,
//! forward starts, LP pricing), the venue (index reconstruction at midnights) and the split program (maturity
//! settlement), so every program reads the same number for the same instant.

use crate::{Benchmark, IndexError, ACCRUAL_SCALE, FIXING_DAYS, SECONDS_PER_DAY};
use anchor_lang::prelude::*;

/// How the cumulative accrual at an instant was obtained (ADR-017 item 1.2). Emitted by the AMM in
/// `SwapClosed.fixing_kind` so indexers can count settlements that used anything other than an exact reading.
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

/// Accrual contributed by `bp` held flat over `secs` seconds, in `accrual_e18` units. The basis-point seconds
/// are formed in 64 bits first (native on SBF, where every 128-bit product is a software routine) and widened
/// once; a span beyond 2^48 seconds takes the 128-bit path.
pub fn rate_over(bp: u16, secs: i64) -> Result<u128> {
    let secs = u64::try_from(secs).map_err(|_| IndexError::Overflow)?;
    let bp_secs = match u64::from(bp).checked_mul(secs) {
        Some(x) => u128::from(x),
        None => u128::from(bp)
            .checked_mul(u128::from(secs))
            .ok_or(IndexError::Overflow)?,
    };
    bp_secs
        .checked_mul(ACCRUAL_SCALE)
        .ok_or_else(|| IndexError::Overflow.into())
}

/// Oldest point of benchmark history still held on chain: the oldest daily fixing in the ring, or the start of
/// the latest segment when no fixing has been recorded.
pub fn oldest_known(b: &Benchmark) -> Result<(i64, u128)> {
    if b.fixing_first_day != 0 {
        let oldest_day = b.fixing_first_day.max(
            b.fixing_head_day
                .saturating_sub(FIXING_DAYS.saturating_sub(1)),
        );
        if let Some(f) = b.fixing(oldest_day) {
            return Ok((Benchmark::day_start(oldest_day)?, f));
        }
    }
    let seg_len = b
        .unix_ts
        .checked_sub(b.prev_unix_ts)
        .ok_or(IndexError::Overflow)?
        .max(0);
    let seg_start = b
        .accrual_e18
        .checked_sub(rate_over(b.prev_value_bp, seg_len)?)
        .ok_or(IndexError::Overflow)?;
    Ok((b.prev_unix_ts, seg_start))
}

/// Cumulative accrual at `t`, total over every `t` not before 1970: a settlement is never refused for lack of
/// history (audit F-34, review F-12, maths M-2, simulation S-3). Forward of the last publish the last value is
/// held flat; inside the latest segment the segment's value is removed pro rata; before that the daily fixings
/// ring written by `publish` answers (ADR-006 as amended by ADR-017).
pub fn accrual_lookup(b: &Benchmark, t: i64) -> Result<(u128, FixingKind)> {
    // A midnight the ring holds, before the latest segment, is answered from the ring alone: the venue's index
    // walk reads one such point per day between its anchor and now, so this path carries no 128-bit products.
    if t < b.prev_unix_ts {
        let day = Benchmark::day_of(t)?;
        if t == Benchmark::day_start(day)? {
            if let Some(f) = b.fixing(day) {
                return Ok((f, FixingKind::Fixing));
            }
        }
    }
    if t >= b.unix_ts {
        let a = b
            .accrual_e18
            .checked_add(rate_over(
                b.value_bp,
                t.checked_sub(b.unix_ts).ok_or(IndexError::Overflow)?,
            )?)
            .ok_or(IndexError::Overflow)?;
        return Ok((a, FixingKind::Live));
    }
    // Accrual at the start of the latest segment.
    let seg_len = b
        .unix_ts
        .checked_sub(b.prev_unix_ts)
        .ok_or(IndexError::Overflow)?
        .max(0);
    let seg_start = b
        .accrual_e18
        .checked_sub(rate_over(b.prev_value_bp, seg_len)?)
        .ok_or(IndexError::Overflow)?;
    if t >= b.prev_unix_ts {
        let a = seg_start
            .checked_add(rate_over(
                b.prev_value_bp,
                t.checked_sub(b.prev_unix_ts).ok_or(IndexError::Overflow)?,
            )?)
            .ok_or(IndexError::Overflow)?;
        return Ok((a, FixingKind::Segment));
    }
    // Before the latest segment: daily fixings.
    let day = Benchmark::day_of(t)?;
    let day_start = Benchmark::day_start(day)?;
    let next_start = day_start
        .checked_add(SECONDS_PER_DAY)
        .ok_or(IndexError::Overflow)?;
    // Nearest known point after `t`: the next day's fixing when the ring holds it, else the segment start.
    let right = match b.fixing(day.checked_add(1).ok_or(IndexError::Overflow)?) {
        Some(f) if next_start <= b.prev_unix_ts => (next_start, f),
        _ => (b.prev_unix_ts, seg_start),
    };
    match b.fixing(day) {
        Some(f0) if t == day_start => Ok((f0, FixingKind::Fixing)),
        Some(f0) if right.1 >= f0 => {
            let span = u128::try_from(right.0.checked_sub(day_start).ok_or(IndexError::Overflow)?)
                .map_err(|_| IndexError::Overflow)?;
            let frac = u128::try_from(t.checked_sub(day_start).ok_or(IndexError::Overflow)?)
                .map_err(|_| IndexError::Overflow)?;
            let delta = right
                .1
                .checked_sub(f0)
                .ok_or(IndexError::Overflow)?
                .checked_mul(frac)
                .ok_or(IndexError::Overflow)?
                .checked_div(span)
                .ok_or(IndexError::Overflow)?;
            Ok((
                f0.checked_add(delta).ok_or(IndexError::Overflow)?,
                FixingKind::Interpolated,
            ))
        }
        _ => {
            let back = rate_over(
                b.prev_value_bp,
                right.0.checked_sub(t).ok_or(IndexError::Overflow)?,
            )?;
            Ok((right.1.saturating_sub(back), FixingKind::Fallback))
        }
    }
}

/// Cumulative accrual at `now`; see [`accrual_lookup`].
pub fn accrual_at(b: &Benchmark, now: i64) -> Result<u128> {
    accrual_lookup(b, now).map(|(a, _)| a)
}
