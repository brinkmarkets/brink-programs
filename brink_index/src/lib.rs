//! Brink benchmark index.
//!
//! One `Benchmark` account per floating-rate source (Kamino USDC supply, marginfi USDC, JitoSOL staking yield, ...).
//! A set of registered publishers post observations; the program aggregates them into a guarded value and derives
//! everything the AMM needs so that no quote ever depends on a single unguarded off-chain number:
//!
//! * quorum: each publisher writes its own observation slot; the benchmark value only moves when at least
//!   `quorum` fresh observations agree with one another (the largest cluster of observations within `band_bp`
//!   of one of them), and the accepted value is always an actual observation, the lower median of that cluster.
//!   One compromised key can therefore neither move the value nor, by posting an outlier, break an honest
//!   quorum that agrees among itself (ADR-010, review F-32; external scan 1, M-6 and M-15);
//! * `ema_bp`: exponential moving average with a governance-set half-life, updated on every accepted median;
//! * `accrual_e18`: cumulative Σ rate × elapsed-seconds, so a swap's average index over its term is
//!   `(accrual_at_settle − accrual_at_open) / seconds`, unit 1e18 × bp·s;
//! * band: an accepted median further than `band_bp` from the EMA is clamped to the band edge and flagged
//!   (`clamped`), never rejected, so an honest regime shift slows repricing instead of stopping publication
//!   (the band widens with the time since the last accepted value so a long outage cannot trap the first honest
//!   value after it);
//! * drift: on top of the band, the value may move at most `max_drift_bp` per `drift_window_slots` from the
//!   value at the start of the window, however many publishes land (simulation finding S-6: without this, 24
//!   hourly band-edge publishes walked the EMA from 684 to 4,107 bp);
//! * guard floors: no guard can be set to a value that freezes or disables the benchmark (M-13).
//!
//! Readers (the swap AMM) use `Benchmark::tier` instead of a binary freshness check: `Fresh`, `Degraded`
//! (clamped, disputed or past `max_staleness_slots` but within twice that) and `Stale`.
//!
//! Status: deployed on devnet; audit in progress. Publishers are keeper keys in v1; v2 adds a verified on-chain reader
//! as one more publisher.
#![allow(clippy::module_name_repetitions)]
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::arithmetic_side_effects,
        clippy::indexing_slicing
    )
)]

use anchor_lang::prelude::*;
use anchor_lang::solana_program::bpf_loader_upgradeable;

declare_id!("J2udZ8xzSsETsrSnbW3DeLFBTVooKRup3P7LWKSRwuvS");

/// Fixed-point scale for `accrual_e18`.
pub const ACCRUAL_SCALE: u128 = 1_000_000_000_000_000_000;
/// Hard ceiling on any published rate: 300% annualised. Anything above is malformed.
pub const MAX_RATE_BP: u16 = 30_000;
/// Account layout version written into every account this program creates (ADR-015). Version 2 added the
/// publisher observations, the drift bound and the daily fixings ring; version 3 adds the EMA anchor slot and
/// the acceptance slot (external scan 1, M-11 and M-12).
pub const LAYOUT_VERSION: u8 = 3;
/// Publisher slots per registry. Launch: 3 publishers, quorum 2; mainnet target 5, quorum 3.
pub const MAX_PUBLISHERS: usize = 5;
/// Guard floors and ceilings (M-13): governance may tighten or relax within these, never disable.
pub const MIN_BAND_BP: u16 = 25;
pub const MAX_BAND_BP: u16 = 5_000;
pub const MIN_DRIFT_BP: u16 = 25;
pub const MAX_DRIFT_BP: u16 = 5_000;
/// Two minutes at ~400 ms per slot.
pub const MIN_STALENESS_SLOTS: u64 = 300;
/// Twelve days.
pub const MAX_STALENESS_SLOTS: u64 = 2_592_000;
pub const MIN_HALF_LIFE_SLOTS: u64 = 150;
pub const MAX_HALF_LIFE_SLOTS: u64 = 2_592_000;
/// About 48 minutes.
pub const MIN_DRIFT_WINDOW_SLOTS: u64 = 7_200;
pub const MAX_DRIFT_WINDOW_SLOTS: u64 = 2_592_000;
/// Seconds per UTC day; fixings are taken at 00:00 UTC.
pub const SECONDS_PER_DAY: i64 = 86_400;
/// Days of daily fixings kept in the benchmark (ADR-006). A swap that matures on a fixing day can be settled
/// exactly for this long after maturity whatever the publisher does in between.
pub const FIXING_DAYS: u32 = 128;
/// Bytes of the fixings ring: one little-endian `u128` accrual per day.
pub const FIXING_BYTES: usize = 16 * FIXING_DAYS as usize;

#[program]
pub mod brink_index {
    use super::*;

    /// Creates the registry: `authority` governs (the timelock PDA after bootstrap), `guardian` may only
    /// tighten (remove a publisher), `publishers` post observations. Only the program's upgrade authority may
    /// pay for and perform the creation (review finding F-14). A quorum below two is a devnet-only setting and
    /// is recorded permanently in `single_publisher`.
    pub fn initialise(ctx: Context<Initialise>, args: InitialiseArgs) -> Result<()> {
        let authority = ctx.accounts.authority.key();
        require!(
            args.guardian != authority && args.guardian != Pubkey::default(),
            IndexError::GuardianScope
        );
        let count = validate_publishers(&args.publishers)?;
        require!(
            args.quorum >= 1 && usize::from(args.quorum) <= count,
            IndexError::BadQuorum
        );
        let single = args.quorum < 2;
        require!(
            !single || args.allow_single_publisher,
            IndexError::BadQuorum
        );
        let r = &mut ctx.accounts.registry;
        r.version = LAYOUT_VERSION;
        r.authority = authority;
        r.guardian = args.guardian;
        r.publishers = args.publishers;
        r.quorum = args.quorum;
        r.single_publisher = single;
        r.count = 0;
        r.bump = ctx.bumps.registry;
        Ok(())
    }

    /// Creates a benchmark with its guard parameters. The first accepted median seeds the EMA.
    pub fn create_benchmark(
        ctx: Context<CreateBenchmark>,
        args: CreateBenchmarkArgs,
    ) -> Result<()> {
        validate_guards(&args.guards)?;
        require!(args.id.iter().any(|b| *b != 0), IndexError::BadId);
        let b = &mut ctx.accounts.benchmark;
        b.version = LAYOUT_VERSION;
        b.registry = ctx.accounts.registry.key();
        b.id = args.id;
        b.source = args.source;
        b.value_bp = 0;
        b.ema_bp = 0;
        b.slot = 0;
        b.unix_ts = 0;
        b.accrual_e18 = 0;
        b.set_guards(&args.guards);
        b.published = false;
        b.publish_count = 0;
        b.bump = ctx.bumps.benchmark;
        b.prev_value_bp = 0;
        b.prev_unix_ts = 0;
        b.drift_anchor_bp = 0;
        b.drift_anchor_slot = 0;
        b.clamped = false;
        b.disputed = false;
        b.support = 0;
        b.ema_milli_bp = 0;
        b.observations = [Observation::default(); MAX_PUBLISHERS];
        b.fixing_first_day = 0;
        b.fixing_head_day = 0;
        b.ema_slot = 0;
        b.accepted_slot = 0;
        ctx.accounts.registry.count = ctx
            .accounts
            .registry
            .count
            .checked_add(1)
            .ok_or(IndexError::Overflow)?;
        emit_cpi!(BenchmarkCreated {
            benchmark: b.key(),
            id: args.id,
            source: args.source
        });
        Ok(())
    }

    /// Posts one publisher's observation and, when at least `quorum` fresh observations agree, moves the
    /// benchmark to their median subject to the band and the drift bound. Never rejected for being out of band:
    /// the value is clamped and flagged instead. Rejected only above the hard ceiling or when this publisher
    /// posts more often than `min_interval_slots`.
    pub fn publish(ctx: Context<Publish>, value_bp: u16) -> Result<()> {
        let clock = Clock::get()?;
        let r = &ctx.accounts.registry;
        let b = &mut ctx.accounts.benchmark;
        require!(value_bp <= MAX_RATE_BP, IndexError::RateCeiling);
        let signer = ctx.accounts.publisher.key();
        let idx = r
            .publishers
            .iter()
            .position(|p| *p == signer && *p != Pubkey::default())
            .ok_or(IndexError::NotAPublisher)?;
        let own = b.observations.get(idx).ok_or(IndexError::Overflow)?;
        if own.publisher == signer && own.slot != 0 {
            require!(
                clock.slot.saturating_sub(own.slot) >= b.min_interval_slots,
                IndexError::TooFrequent
            );
        }
        *b.observations.get_mut(idx).ok_or(IndexError::Overflow)? = Observation {
            publisher: signer,
            value_bp,
            slot: clock.slot,
        };

        // Aggregate: fresh observations from currently registered publishers only.
        let mut values = [0u16; MAX_PUBLISHERS];
        let mut slots = [0u64; MAX_PUBLISHERS];
        let mut n = 0usize;
        for (i, obs) in b.observations.iter().enumerate() {
            let registered = r.publishers.get(i).copied().unwrap_or_default();
            if obs.publisher == Pubkey::default()
                || obs.publisher != registered
                || clock.slot.saturating_sub(obs.slot) > b.max_staleness_slots
            {
                continue;
            }
            if let (Some(v), Some(sl)) = (values.get_mut(n), slots.get_mut(n)) {
                *v = obs.value_bp;
                *sl = obs.slot;
                n = n.saturating_add(1);
            }
        }
        let fresh = values.get(..n).ok_or(IndexError::Overflow)?;
        let fresh_slots = slots.get(..n).ok_or(IndexError::Overflow)?;
        let fresh_count = u8::try_from(n).map_err(|_| IndexError::Overflow)?;
        if n < usize::from(r.quorum) {
            emit_cpi!(ObservationRecorded {
                benchmark: b.key(),
                publisher: signer,
                value_bp,
                slot: clock.slot,
                fresh_count,
                disputed: false
            });
            return Ok(());
        }
        // The largest cluster of observations that agree within the band; its lower median is the candidate.
        // A tie between clusters goes to the one nearest the current value (M-6, M-15).
        let (median, support) = agreeing_value(fresh, b.band_bp, b.published.then_some(b.value_bp));
        if support < r.quorum {
            b.disputed = true;
            emit_cpi!(ObservationRecorded {
                benchmark: b.key(),
                publisher: signer,
                value_bp,
                slot: clock.slot,
                fresh_count,
                disputed: true
            });
            return Ok(());
        }

        let mut clamped = false;
        let accepted = if b.published {
            // Accrual: previous value held over the elapsed seconds since the previous accepted value.
            let elapsed = u128::try_from(
                clock
                    .unix_timestamp
                    .checked_sub(b.unix_ts)
                    .ok_or(IndexError::ClockWentBackwards)?,
            )
            .map_err(|_| IndexError::ClockWentBackwards)?;
            // Daily fixings: the previous value is flat over the segment, so the accrual at every midnight the
            // segment crosses is exact and is written into the ring before the accumulator moves on (ADR-006).
            b.record_fixings(clock.unix_timestamp)?;
            let add = u128::from(b.value_bp)
                .checked_mul(elapsed)
                .and_then(|x| x.checked_mul(ACCRUAL_SCALE))
                .ok_or(IndexError::Overflow)?;
            b.accrual_e18 = b.accrual_e18.checked_add(add).ok_or(IndexError::Overflow)?;
            b.prev_value_bp = b.value_bp;
            b.prev_unix_ts = b.unix_ts;
            // Band around the EMA, widened by the time since the last accepted value so an outage cannot trap
            // the first honest value after it; clamp and flag rather than reject.
            let dt = clock.slot.saturating_sub(b.accepted_slot);
            let band = effective_band(b.band_bp, dt, b.half_life_slots);
            // Drift bound: at most `max_drift_bp` from the value at the start of the current window. The two
            // clamps are composed on their intersection; when the drift interval has fallen outside the band
            // (a stale anchor after a guard change, M-13) the band decides and the anchor is reset to the result.
            if clock.slot.saturating_sub(b.drift_anchor_slot) >= b.drift_window_slots {
                b.drift_anchor_bp = b.value_bp;
                b.drift_anchor_slot = clock.slot;
            }
            let (v2, c, reanchor) =
                clamp_composed(median, b.ema_bp, band, b.drift_anchor_bp, b.max_drift_bp);
            if reanchor {
                b.drift_anchor_bp = v2;
                b.drift_anchor_slot = clock.slot;
            }
            clamped = c;
            // EMA with slot-based decay: alpha = elapsed_slots / (elapsed_slots + half_life), where the elapsed
            // slots are measured from the last slot at which the EMA actually moved (`ema_slot`), not from the
            // last publication. A step too small to register at milli-bp precision therefore leaves the anchor
            // where it was and is picked up by the next publication instead of being discarded, so frequent
            // publications cannot stall the average (maths finding M-6; external scan 1, M-11). Kept at
            // milli-bp (bp x 1 000); `ema_bp` is the rounded public view.
            let dt_ema = clock.slot.saturating_sub(b.ema_slot);
            let (new_milli, moved) = ema_step(b.ema_milli_bp, v2, dt_ema, b.half_life_slots)?;
            if moved {
                b.ema_slot = clock.slot;
            }
            b.ema_milli_bp = new_milli;
            b.ema_bp = u16::try_from(
                u128::from(new_milli)
                    .checked_add(500)
                    .ok_or(IndexError::Overflow)?
                    .checked_div(1_000)
                    .ok_or(IndexError::Overflow)?,
            )
            .map_err(|_| IndexError::Overflow)?;
            v2
        } else {
            b.ema_bp = median;
            b.ema_milli_bp = u32::from(median).saturating_mul(1_000);
            b.published = true;
            b.prev_value_bp = median;
            b.prev_unix_ts = clock.unix_timestamp;
            b.drift_anchor_bp = median;
            b.drift_anchor_slot = clock.slot;
            b.ema_slot = clock.slot;
            median
        };
        b.value_bp = accepted;
        // Freshness is that of the oldest observation the accepted value rests on, not of this transaction:
        // a publication that merely re-aggregates old observations does not refresh the benchmark (M-12).
        b.slot = supporting_min_slot(fresh, fresh_slots, median, b.band_bp);
        b.accepted_slot = clock.slot;
        b.unix_ts = clock.unix_timestamp;
        b.clamped = clamped;
        b.disputed = false;
        b.support = support;
        b.publish_count = b.publish_count.checked_add(1).ok_or(IndexError::Overflow)?;
        emit_cpi!(Published {
            benchmark: b.key(),
            value_bp: accepted,
            median_bp: median,
            ema_bp: b.ema_bp,
            slot: clock.slot,
            accrual_e18: b.accrual_e18,
            support,
            clamped
        });
        Ok(())
    }

    /// Governance (delayed path): sets the guards within the floors and ceilings.
    pub fn set_guards(ctx: Context<Govern>, guards: GuardArgs) -> Result<()> {
        validate_guards(&guards)?;
        let b = &mut ctx.accounts.benchmark;
        b.set_guards(&guards);
        // A new drift bound starts from the current value: an anchor left from the old window must not force
        // the next honest value out of the band (M-13).
        if b.published {
            b.drift_anchor_bp = b.value_bp;
            b.drift_anchor_slot = Clock::get()?.slot;
        }
        Ok(())
    }

    /// Governance (delayed path): replaces the publisher set and quorum. A quorum below two is only accepted on
    /// a registry created with `allow_single_publisher`.
    pub fn set_publishers(
        ctx: Context<GovernRegistry>,
        publishers: [Pubkey; MAX_PUBLISHERS],
        quorum: u8,
    ) -> Result<()> {
        let count = validate_publishers(&publishers)?;
        let r = &mut ctx.accounts.registry;
        require!(
            quorum >= 1 && usize::from(quorum) <= count,
            IndexError::BadQuorum
        );
        require!(quorum >= 2 || r.single_publisher, IndexError::BadQuorum);
        r.publishers = publishers;
        r.quorum = quorum;
        Ok(())
    }

    /// Tightening (immediate): the authority or the guardian removes one publisher. The quorum is unchanged, so
    /// removal can only make the benchmark harder to move, never easier; if fewer publishers than the quorum
    /// remain the benchmark goes stale until `set_publishers` runs through the delayed path.
    pub fn remove_publisher(ctx: Context<GuardRegistry>, publisher: Pubkey) -> Result<()> {
        let r = &mut ctx.accounts.registry;
        let s = ctx.accounts.signer.key();
        require!(
            s == r.authority || s == r.guardian,
            IndexError::Unauthorised
        );
        let slot = r
            .publishers
            .iter_mut()
            .find(|p| **p == publisher && publisher != Pubkey::default())
            .ok_or(IndexError::NotAPublisher)?;
        *slot = Pubkey::default();
        Ok(())
    }

    /// Governance (delayed path): rotates the guardian key.
    pub fn set_guardian(ctx: Context<GovernRegistry>, guardian: Pubkey) -> Result<()> {
        let r = &mut ctx.accounts.registry;
        require!(
            guardian != r.authority && guardian != Pubkey::default(),
            IndexError::GuardianScope
        );
        r.guardian = guardian;
        Ok(())
    }

    /// Governance: hands the registry to a new authority, which co-signs so a mistyped key cannot take it.
    /// When the new authority is the timelock PDA the transfer is executed through a timelock `Invoke`, which
    /// makes the PDA a signer by construction.
    pub fn set_authority(ctx: Context<TransferRegistry>) -> Result<()> {
        let new_authority = ctx.accounts.new_authority.key();
        let r = &mut ctx.accounts.registry;
        require!(new_authority != r.guardian, IndexError::GuardianScope);
        r.authority = new_authority;
        Ok(())
    }
}

/// Number of non-empty publishers; rejects duplicates and an empty set.
fn validate_publishers(publishers: &[Pubkey; MAX_PUBLISHERS]) -> Result<usize> {
    let mut count = 0usize;
    for (i, p) in publishers.iter().enumerate() {
        if *p == Pubkey::default() {
            continue;
        }
        count = count.saturating_add(1);
        require!(
            publishers.iter().skip(i.saturating_add(1)).all(|q| q != p),
            IndexError::DuplicatePublisher
        );
    }
    require!(count >= 1, IndexError::BadQuorum);
    Ok(count)
}

fn validate_guards(g: &GuardArgs) -> Result<()> {
    require!(
        (MIN_BAND_BP..=MAX_BAND_BP).contains(&g.band_bp)
            && (MIN_DRIFT_BP..=MAX_DRIFT_BP).contains(&g.max_drift_bp)
            && (MIN_STALENESS_SLOTS..=MAX_STALENESS_SLOTS).contains(&g.max_staleness_slots)
            && (MIN_HALF_LIFE_SLOTS..=MAX_HALF_LIFE_SLOTS).contains(&g.half_life_slots)
            && (MIN_DRIFT_WINDOW_SLOTS..=MAX_DRIFT_WINDOW_SLOTS).contains(&g.drift_window_slots)
            // Two observations per publisher must fit inside one staleness window, or a quorum can never form.
            && g.min_interval_slots <= g.max_staleness_slots / 2,
        IndexError::BadGuard
    );
    Ok(())
}

/// One EMA step at milli-bp precision: `alpha = dt / (dt + half_life)` towards `value_bp`, rounded half up.
/// Returns the new EMA and whether the anchor slot should advance: it advances when the EMA moved or when the
/// input already equals the EMA; a step that rounds to zero keeps the anchor so the elapsed time accumulates
/// into the next step instead of being lost (M-11).
fn ema_step(old_milli: u32, value_bp: u16, dt: u64, half_life_slots: u64) -> Result<(u32, bool)> {
    let dt = u128::from(dt);
    let hl = u128::from(half_life_slots);
    let denom = hl.checked_add(dt).ok_or(IndexError::Overflow)?;
    let target = u128::from(value_bp)
        .checked_mul(1_000)
        .ok_or(IndexError::Overflow)?;
    let old = u128::from(old_milli);
    let num = old
        .checked_mul(hl)
        .ok_or(IndexError::Overflow)?
        .checked_add(target.checked_mul(dt).ok_or(IndexError::Overflow)?)
        .ok_or(IndexError::Overflow)?;
    let new = num
        .checked_add(denom / 2)
        .ok_or(IndexError::Overflow)?
        .checked_div(denom)
        .ok_or(IndexError::BadGuard)?;
    Ok((
        u32::try_from(new).map_err(|_| IndexError::Overflow)?,
        new != old || target == old,
    ))
}

/// Lower median of a sorted slice: the middle element for an odd count, the lower of the two middle elements
/// for an even count. Always an element of the slice.
fn lower_median(sorted: &[u16]) -> u16 {
    let n = sorted.len();
    if n == 0 {
        return 0;
    }
    sorted.get(n.saturating_sub(1) / 2).copied().unwrap_or(0)
}

/// The agreed value among `values` and the size of the agreement, for a tolerance of `tolerance_bp`.
///
/// For each observation the cluster of observations within `tolerance_bp` of it is formed; the largest cluster
/// wins and its lower median is the value. Agreement is therefore measured between the observations
/// themselves, never around a synthetic midpoint, so two observations `2 x band` apart do not support their
/// average (M-6), and an outlier can only enlarge nothing: it cannot shrink an honest cluster (M-15). Ties
/// between equally large clusters go to the one whose value is nearest `prefer` (the current value) or, with
/// nothing to prefer, to the lower value. Returns `(0, 0)` for an empty slice.
#[must_use]
pub fn agreeing_value(values: &[u16], tolerance_bp: u16, prefer: Option<u16>) -> (u16, u8) {
    let mut sorted = [0u16; MAX_PUBLISHERS];
    let n = values.len().min(MAX_PUBLISHERS);
    for (dst, src) in sorted.iter_mut().zip(values.iter()) {
        *dst = *src;
    }
    let Some(s) = sorted.get_mut(..n) else {
        return (0, 0);
    };
    if s.is_empty() {
        return (0, 0);
    }
    s.sort_unstable();
    let mut best: Option<(u16, usize)> = None;
    for centre in s.iter().copied() {
        // Members are contiguous in the sorted slice.
        let lo = s.partition_point(|v| *v < centre.saturating_sub(tolerance_bp));
        let hi = s.partition_point(|v| *v <= centre.saturating_add(tolerance_bp));
        let Some(members) = s.get(lo..hi) else {
            continue;
        };
        let value = lower_median(members);
        let size = members.len();
        let better = match best {
            None => true,
            Some((bv, bs)) => {
                size > bs
                    || (size == bs
                        && match prefer {
                            Some(p) => value.abs_diff(p) < bv.abs_diff(p),
                            None => value < bv,
                        })
            }
        };
        if better {
            best = Some((value, size));
        }
    }
    match best {
        Some((v, size)) => (v, u8::try_from(size).unwrap_or(u8::MAX)),
        None => (0, 0),
    }
}

/// Oldest slot among the observations within `tolerance_bp` of `value`: the freshness of the accepted value
/// is the freshness of the data it rests on (M-12). Returns 0 when nothing supports the value.
#[must_use]
pub fn supporting_min_slot(values: &[u16], slots: &[u64], value: u16, tolerance_bp: u16) -> u64 {
    values
        .iter()
        .zip(slots.iter())
        .filter(|(v, _)| v.abs_diff(value) <= tolerance_bp)
        .map(|(_, sl)| *sl)
        .min()
        .unwrap_or(0)
}

/// Clamps `value` into the intersection of the EMA band `[ema − band, ema + band]` and the drift interval
/// `[anchor − drift, anchor + drift]`. When the two do not intersect the band decides and the caller re-anchors
/// the drift bound to the result (third element), so a stale anchor can never force a value out of the band
/// (M-13). The flag says whether the value moved.
#[must_use]
pub fn clamp_composed(
    value: u16,
    ema: u16,
    band: u16,
    anchor: u16,
    drift: u16,
) -> (u16, bool, bool) {
    let b_lo = ema.saturating_sub(band);
    let b_hi = ema.saturating_add(band).min(MAX_RATE_BP);
    let d_lo = anchor.saturating_sub(drift);
    let d_hi = anchor.saturating_add(drift).min(MAX_RATE_BP);
    let lo = b_lo.max(d_lo);
    let hi = b_hi.min(d_hi);
    if lo <= hi {
        let v = value.clamp(lo, hi);
        (v, v != value, false)
    } else {
        let v = value.clamp(b_lo, b_hi);
        (v, v != value, true)
    }
}

/// `band × (1 + elapsed / half_life)`, saturating at `MAX_RATE_BP`.
#[must_use]
pub fn effective_band(band_bp: u16, elapsed_slots: u64, half_life_slots: u64) -> u16 {
    let hl = half_life_slots.max(1);
    let widened = u128::from(band_bp)
        .saturating_mul(u128::from(hl.saturating_add(elapsed_slots)))
        .checked_div(u128::from(hl))
        .unwrap_or(u128::from(band_bp));
    u16::try_from(widened.min(u128::from(MAX_RATE_BP))).unwrap_or(MAX_RATE_BP)
}

/// Clamps `value` into `[centre − width, centre + width]`; the flag says whether it moved.
#[must_use]
pub fn clamp_to(value: u16, centre: u16, width: u16) -> (u16, bool) {
    let lo = centre.saturating_sub(width);
    let hi = centre.saturating_add(width).min(MAX_RATE_BP);
    if value < lo {
        (lo, true)
    } else if value > hi {
        (hi, true)
    } else {
        (value, false)
    }
}

/// Reader classification of a benchmark. The swap AMM quotes only on `Fresh`, allows exits and imbalance-reducing
/// entries on `Degraded`, and refuses to quote, cancel or liquidate on `Stale` (settlement never needs freshness).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tier {
    Fresh,
    Degraded,
    Stale,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy)]
pub struct InitialiseArgs {
    pub guardian: Pubkey,
    pub publishers: [Pubkey; MAX_PUBLISHERS],
    pub quorum: u8,
    /// Devnet only: permits `quorum == 1`. Recorded permanently as `Registry.single_publisher`.
    pub allow_single_publisher: bool,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy)]
pub struct GuardArgs {
    pub band_bp: u16,
    pub max_staleness_slots: u64,
    pub half_life_slots: u64,
    pub min_interval_slots: u64,
    pub max_drift_bp: u16,
    pub drift_window_slots: u64,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy)]
pub struct CreateBenchmarkArgs {
    pub id: [u8; 16],
    pub source: Pubkey,
    pub guards: GuardArgs,
}

#[derive(
    AnchorSerialize, AnchorDeserialize, Clone, Copy, Default, PartialEq, Eq, Debug, InitSpace,
)]
pub struct Observation {
    pub publisher: Pubkey,
    pub value_bp: u16,
    pub slot: u64,
}

#[account]
#[derive(InitSpace)]
pub struct Registry {
    pub version: u8,
    pub authority: Pubkey,
    /// May only tighten: `remove_publisher`.
    pub guardian: Pubkey,
    /// Empty slots are `Pubkey::default()`. Index `i` owns `Benchmark.observations[i]`.
    pub publishers: [Pubkey; MAX_PUBLISHERS],
    pub quorum: u8,
    /// Devnet flag: the registry was created allowing a quorum of one. Never true on a mainnet registry.
    pub single_publisher: bool,
    pub count: u32,
    pub bump: u8,
    pub _reserved: [u8; 64],
}

/// Benchmark account. The swap AMM reads this layout directly (owner-checked), so field order is frozen per
/// `version`; both programs upgrade together when it changes.
#[account]
#[derive(InitSpace)]
pub struct Benchmark {
    pub version: u8,
    pub registry: Pubkey,
    pub id: [u8; 16],   // ascii tag, e.g. b"kamino-usdc"
    pub source: Pubkey, // the on-chain account the rate is derived from (informational in v1)
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
    pub bump: u8,
    /// Value held over the most recent accrual segment `[prev_unix_ts, unix_ts]` (maths finding M-2).
    pub prev_value_bp: u16,
    /// Start of the most recent accrual segment; equals `unix_ts` until the second accepted value.
    pub prev_unix_ts: i64,
    /// Drift bound (S-6): the value may move at most this far from `drift_anchor_bp` within one window.
    pub max_drift_bp: u16,
    pub drift_window_slots: u64,
    pub drift_anchor_bp: u16,
    pub drift_anchor_slot: u64,
    /// The last accepted value was clamped by the band or the drift bound.
    pub clamped: bool,
    /// The last aggregation had a quorum of fresh observations that did not agree; the value was not moved.
    pub disputed: bool,
    /// Observations that supported the last accepted value. `1` means a single-publisher (devnet) registry.
    pub support: u8,
    pub observations: [Observation; MAX_PUBLISHERS],
    /// EMA at bp x 1 000 (maths finding M-6). `ema_bp` is `rhu(ema_milli_bp / 1 000)`; at most
    /// `MAX_RATE_BP x 1 000`, so it fits `u32` and the `u16` narrowing of `ema_bp` cannot fail.
    pub ema_milli_bp: u32,
    /// UTC day number (unix seconds / 86 400) of the oldest fixing ever written; 0 until the first midnight the
    /// benchmark lives through.
    pub fixing_first_day: u32,
    /// UTC day number of the most recent fixing written.
    pub fixing_head_day: u32,
    /// Ring of `FIXING_DAYS` daily fixings: the cumulative accrual (`accrual_e18` units) at 00:00 UTC of day `d`
    /// lives at byte offset `16 * (d % FIXING_DAYS)` as a little-endian `u128`. Written by `publish` for every
    /// midnight a segment crosses; read by the AMM to settle a swap at its maturity whatever was published later
    /// (ADR-006, review finding F-12, maths finding M-2, simulation finding S-3). Raw bytes rather than
    /// `[u128; N]` so that loading the account is a copy, not a per-element decode.
    pub fixings: [u8; FIXING_BYTES],
    /// Slot at which the EMA last moved (or was confirmed equal to its input); the decay of the next step is
    /// measured from here, not from the last publication (M-11).
    pub ema_slot: u64,
    /// Slot of the transaction that last accepted a value. `slot` is the freshness of the data behind it (M-12);
    /// this is when the benchmark itself last moved, used to widen the band after an outage.
    pub accepted_slot: u64,
    pub _reserved: [u8; 12],
}

impl Benchmark {
    fn set_guards(&mut self, g: &GuardArgs) {
        self.band_bp = g.band_bp;
        self.max_staleness_slots = g.max_staleness_slots;
        self.half_life_slots = g.half_life_slots;
        self.min_interval_slots = g.min_interval_slots;
        self.max_drift_bp = g.max_drift_bp;
        self.drift_window_slots = g.drift_window_slots;
    }

    /// Pure reader classification; see `Tier`.
    #[must_use]
    pub fn tier(&self, now_slot: u64) -> Tier {
        if !self.published {
            return Tier::Stale;
        }
        let age = now_slot.saturating_sub(self.slot);
        if age > self.max_staleness_slots.saturating_mul(2) {
            return Tier::Stale;
        }
        if age > self.max_staleness_slots || self.clamped || self.disputed {
            return Tier::Degraded;
        }
        Tier::Fresh
    }

    /// UTC day number of a unix timestamp (floor division; timestamps before 1970 are not valid here).
    pub fn day_of(unix_ts: i64) -> Result<u32> {
        u32::try_from(unix_ts.div_euclid(SECONDS_PER_DAY)).map_err(|_| IndexError::Overflow.into())
    }
    /// Midnight that starts UTC day `day`.
    pub fn day_start(day: u32) -> Result<i64> {
        i64::from(day)
            .checked_mul(SECONDS_PER_DAY)
            .ok_or(IndexError::Overflow.into())
    }
    /// Cumulative accrual at 00:00 UTC of `day`, if the ring still holds it. `None` for days before the first
    /// fixing, after the latest one, or older than `FIXING_DAYS` days before the latest one.
    #[must_use]
    pub fn fixing(&self, day: u32) -> Option<u128> {
        if self.fixing_first_day == 0
            || day < self.fixing_first_day
            || day > self.fixing_head_day
            || self.fixing_head_day.saturating_sub(day) >= FIXING_DAYS
        {
            return None;
        }
        let off = 16usize.checked_mul(usize::try_from(day % FIXING_DAYS).ok()?)?;
        let bytes: [u8; 16] = self
            .fixings
            .get(off..off.checked_add(16)?)?
            .try_into()
            .ok()?;
        Some(u128::from_le_bytes(bytes))
    }
    fn set_fixing(&mut self, day: u32, accrual: u128) -> Result<()> {
        let off = 16usize
            .checked_mul(usize::try_from(day % FIXING_DAYS).map_err(|_| IndexError::Overflow)?)
            .ok_or(IndexError::Overflow)?;
        let end = off.checked_add(16).ok_or(IndexError::Overflow)?;
        self.fixings
            .get_mut(off..end)
            .ok_or(IndexError::Overflow)?
            .copy_from_slice(&accrual.to_le_bytes());
        Ok(())
    }
    /// Writes the fixing for every midnight in `(self.unix_ts, now]`, using the value held flat over that
    /// segment. Only the last `FIXING_DAYS` days matter, so a long outage costs at most `FIXING_DAYS` writes.
    /// Must run before `accrual_e18`, `value_bp` and `unix_ts` are advanced to `now`.
    pub fn record_fixings(&mut self, now: i64) -> Result<()> {
        let from_day = Self::day_of(self.unix_ts)?;
        let to_day = Self::day_of(now)?;
        if to_day <= from_day {
            return Ok(());
        }
        let first = from_day
            .checked_add(1)
            .ok_or(IndexError::Overflow)?
            .max(to_day.saturating_sub(FIXING_DAYS.saturating_sub(1)));
        let mut day = first;
        while day <= to_day {
            let midnight = Self::day_start(day)?;
            let secs = u128::try_from(
                midnight
                    .checked_sub(self.unix_ts)
                    .ok_or(IndexError::ClockWentBackwards)?,
            )
            .map_err(|_| IndexError::ClockWentBackwards)?;
            let add = u128::from(self.value_bp)
                .checked_mul(secs)
                .and_then(|x| x.checked_mul(ACCRUAL_SCALE))
                .ok_or(IndexError::Overflow)?;
            let accrual = self
                .accrual_e18
                .checked_add(add)
                .ok_or(IndexError::Overflow)?;
            self.set_fixing(day, accrual)?;
            day = day.checked_add(1).ok_or(IndexError::Overflow)?;
        }
        if self.fixing_first_day == 0 {
            self.fixing_first_day = first;
        }
        self.fixing_head_day = to_day;
        Ok(())
    }
}

#[derive(Accounts)]
pub struct Initialise<'info> {
    #[account(init, payer = payer, space = 8 + Registry::INIT_SPACE, seeds = [b"registry"], bump)]
    pub registry: Box<Account<'info, Registry>>,
    pub authority: Signer<'info>,
    /// The deployer: must be this program's upgrade authority (F-14).
    #[account(mut)]
    pub payer: Signer<'info>,
    /// This program's own `ProgramData`; binds `payer` to the upgrade authority so the singleton cannot be
    /// front-run by the first caller after deployment.
    #[account(
        address = bpf_loader_upgradeable::get_program_data_address(&crate::ID) @ IndexError::NotUpgradeAuthority,
        constraint = program_data.upgrade_authority_address == Some(payer.key()) @ IndexError::NotUpgradeAuthority,
    )]
    pub program_data: Account<'info, ProgramData>,
    pub system_program: Program<'info, System>,
}

#[event_cpi]
#[derive(Accounts)]
#[instruction(args: CreateBenchmarkArgs)]
pub struct CreateBenchmark<'info> {
    #[account(mut, seeds = [b"registry"], bump = registry.bump, has_one = authority)]
    pub registry: Box<Account<'info, Registry>>,
    pub authority: Signer<'info>,
    #[account(mut)]
    pub payer: Signer<'info>,
    #[account(init, payer = payer, space = 8 + Benchmark::INIT_SPACE, seeds = [b"benchmark", args.id.as_ref()], bump)]
    pub benchmark: Box<Account<'info, Benchmark>>,
    pub system_program: Program<'info, System>,
}

#[event_cpi]
#[derive(Accounts)]
pub struct Publish<'info> {
    #[account(seeds = [b"registry"], bump = registry.bump)]
    pub registry: Box<Account<'info, Registry>>,
    /// Any registered publisher; membership is checked in the handler.
    pub publisher: Signer<'info>,
    #[account(mut, has_one = registry, seeds = [b"benchmark", benchmark.id.as_ref()], bump = benchmark.bump)]
    pub benchmark: Box<Account<'info, Benchmark>>,
}

#[derive(Accounts)]
pub struct Govern<'info> {
    #[account(seeds = [b"registry"], bump = registry.bump, has_one = authority)]
    pub registry: Box<Account<'info, Registry>>,
    pub authority: Signer<'info>,
    #[account(mut, has_one = registry)]
    pub benchmark: Box<Account<'info, Benchmark>>,
}

#[derive(Accounts)]
pub struct GovernRegistry<'info> {
    #[account(mut, seeds = [b"registry"], bump = registry.bump, has_one = authority)]
    pub registry: Box<Account<'info, Registry>>,
    pub authority: Signer<'info>,
}

/// Authority or guardian; checked in the handler.
#[derive(Accounts)]
pub struct GuardRegistry<'info> {
    #[account(mut, seeds = [b"registry"], bump = registry.bump)]
    pub registry: Box<Account<'info, Registry>>,
    pub signer: Signer<'info>,
}

#[derive(Accounts)]
pub struct TransferRegistry<'info> {
    #[account(mut, seeds = [b"registry"], bump = registry.bump, has_one = authority)]
    pub registry: Box<Account<'info, Registry>>,
    pub authority: Signer<'info>,
    pub new_authority: Signer<'info>,
}

#[event]
pub struct BenchmarkCreated {
    pub benchmark: Pubkey,
    pub id: [u8; 16],
    pub source: Pubkey,
}
/// An observation that did not (yet) move the benchmark: no quorum, or a quorum that disagreed.
#[event]
pub struct ObservationRecorded {
    pub benchmark: Pubkey,
    pub publisher: Pubkey,
    pub value_bp: u16,
    pub slot: u64,
    pub fresh_count: u8,
    pub disputed: bool,
}
#[event]
pub struct Published {
    pub benchmark: Pubkey,
    pub value_bp: u16,
    pub median_bp: u16,
    pub ema_bp: u16,
    pub slot: u64,
    pub accrual_e18: u128,
    pub support: u8,
    pub clamped: bool,
}

#[error_code]
pub enum IndexError {
    #[msg("guard parameter outside its floor and ceiling")]
    BadGuard,
    #[msg("benchmark id must be non-zero")]
    BadId,
    #[msg("rate above the hard ceiling")]
    RateCeiling,
    #[msg("publish arrived before min_interval_slots elapsed")]
    TooFrequent,
    #[msg("observation outside the EMA band")]
    OutOfBand,
    #[msg("clock went backwards")]
    ClockWentBackwards,
    #[msg("arithmetic overflow")]
    Overflow,
    #[msg("only the program's upgrade authority may initialise")]
    NotUpgradeAuthority,
    #[msg("signer is not a registered publisher")]
    NotAPublisher,
    #[msg("publishers must be distinct")]
    DuplicatePublisher,
    #[msg("quorum must be between 1 and the publisher count, and at least 2 unless the registry allows one")]
    BadQuorum,
    #[msg("guardian must differ from the authority and be non-zero")]
    GuardianScope,
    #[msg("signer is not permitted to do this")]
    Unauthorised,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blank() -> Benchmark {
        Benchmark {
            registry: Pubkey::default(),
            id: [1; 16],
            source: Pubkey::default(),
            value_bp: 0,
            ema_bp: 0,
            slot: 0,
            unix_ts: 0,
            accrual_e18: 0,
            max_staleness_slots: 1,
            band_bp: 1,
            half_life_slots: 1,
            min_interval_slots: 0,
            published: false,
            publish_count: 0,
            bump: 0,
            prev_value_bp: 0,
            prev_unix_ts: 0,
            version: LAYOUT_VERSION,
            max_drift_bp: 400,
            drift_window_slots: 216_000,
            drift_anchor_bp: 0,
            drift_anchor_slot: 0,
            clamped: false,
            disputed: false,
            support: 0,
            observations: [Observation::default(); MAX_PUBLISHERS],
            ema_milli_bp: 0,
            fixing_first_day: 0,
            fixing_head_day: 0,
            fixings: [0; FIXING_BYTES],
            ema_slot: 0,
            accepted_slot: 0,
            _reserved: [0; 12],
        }
    }
    /// Mirrors the accrual part of `publish` for host tests.
    fn publish_at(b: &mut Benchmark, now: i64, value_bp: u16) {
        if b.published {
            let elapsed = u128::try_from(now - b.unix_ts).unwrap();
            b.record_fixings(now).unwrap();
            b.accrual_e18 += u128::from(b.value_bp) * elapsed * ACCRUAL_SCALE;
            b.prev_value_bp = b.value_bp;
            b.prev_unix_ts = b.unix_ts;
        } else {
            b.published = true;
            b.prev_value_bp = value_bp;
            b.prev_unix_ts = now;
        }
        b.value_bp = value_bp;
        b.unix_ts = now;
    }
    const DAY: i64 = SECONDS_PER_DAY;

    #[test]
    fn fixings_are_exact_at_every_crossed_midnight() {
        let mut b = blank();
        let t0 = 20_000 * DAY + 3_600; // 01:00 on day 20 000
        publish_at(&mut b, t0, 684);
        assert_eq!(b.fixing(20_001), None);
        // One publish nine days later crosses nine midnights at 684 bp flat.
        publish_at(&mut b, t0 + 9 * DAY + 60, 700);
        for d in 20_001..=20_009 {
            let secs = u128::try_from(i64::from(d) * DAY - t0).unwrap();
            assert_eq!(b.fixing(d), Some(684 * secs * ACCRUAL_SCALE), "day {d}");
        }
        assert_eq!(b.fixing(20_000), None);
        assert_eq!(b.fixing(20_010), None);
        assert_eq!(b.fixing_first_day, 20_001);
        assert_eq!(b.fixing_head_day, 20_009);
        // A second publish the same day writes nothing new; the next day's fixing uses the new value.
        publish_at(&mut b, t0 + 9 * DAY + 120, 700);
        assert_eq!(b.fixing_head_day, 20_009);
        publish_at(&mut b, t0 + 10 * DAY, 700);
        // Day 20 009 ran at 684 bp until 01:01, then at 700 bp.
        let expected = b.fixing(20_009).unwrap()
            + 684 * u128::try_from(3_600 + 60).unwrap() * ACCRUAL_SCALE
            + 700 * u128::try_from(DAY - 3_600 - 60).unwrap() * ACCRUAL_SCALE;
        assert_eq!(b.fixing(20_010), Some(expected));
    }

    #[test]
    fn ring_keeps_the_last_fixing_days_and_forgets_older_ones() {
        let mut b = blank();
        let t0 = 10_000 * DAY;
        publish_at(&mut b, t0, 100);
        for i in 1..=(FIXING_DAYS as i64 + 40) {
            publish_at(&mut b, t0 + i * DAY + 1, 100);
        }
        let head = 10_000 + FIXING_DAYS + 40;
        assert_eq!(b.fixing_head_day, head);
        assert!(b.fixing(head).is_some());
        assert!(b.fixing(head - FIXING_DAYS + 1).is_some());
        assert!(b.fixing(head - FIXING_DAYS).is_none());
        // A 400-day outage writes only the last FIXING_DAYS fixings, each still exact.
        let far = b.unix_ts + 400 * DAY;
        publish_at(&mut b, far, 100);
        let head = Benchmark::day_of(far).unwrap();
        assert_eq!(b.fixing_head_day, head);
        assert!(b.fixing(head - FIXING_DAYS + 1).is_some());
        assert!(b.fixing(head - FIXING_DAYS).is_none());
        let d = head - 5;
        let secs = u128::try_from(Benchmark::day_start(d).unwrap() - b.prev_unix_ts).unwrap();
        assert_eq!(
            b.fixing(d),
            Some(
                b.accrual_e18 - 100 * u128::try_from(far - b.prev_unix_ts).unwrap() * ACCRUAL_SCALE
                    + 100 * secs * ACCRUAL_SCALE
            )
        );
    }

    #[test]
    fn layout_size_is_as_documented() {
        // 8-byte discriminator plus the InitSpace body: about 2.3 KiB, within one realloc step (ADR-014).
        let size = 8 + Benchmark::INIT_SPACE;
        assert!(size > 2_100 && size < 2_700, "{size}");
    }

    const GUARDS: GuardArgs = GuardArgs {
        band_bp: 300,
        max_staleness_slots: 2_000,
        half_life_slots: 10_000,
        min_interval_slots: 150,
        max_drift_bp: 400,
        drift_window_slots: 216_000,
    };

    #[test]
    fn frequent_small_publications_do_not_stall_the_ema() {
        // M-11: with the anchor at the last publication, 1 000 publications 150 slots apart at a half life of
        // 10 000 000 slots each round to zero and the EMA never moves. Anchored to the last move it catches up.
        let hl = 10_000_000u64;
        let (one, moved) = ema_step(684_000, 700, 150, hl).unwrap();
        assert_eq!(
            (one, moved),
            (684_000, false),
            "a single short step rounds away"
        );
        let mut ema = 684_000u32;
        let mut anchor = 0u64;
        for i in 1..=1_000u64 {
            let now = i * 150;
            let (n, m) = ema_step(ema, 700, now - anchor, hl).unwrap();
            if m {
                anchor = now;
            }
            ema = n;
        }
        let (direct, _) = ema_step(684_000, 700, 150_000, hl).unwrap();
        // Each registered step rounds half up, so the stepped path leads the single-step path by at most half
        // a milli-bp per step and never passes the input (the quotient is bounded by the target).
        assert!(
            ema >= direct && ema <= direct + 100,
            "{ema} vs one step of the whole span {direct}"
        );
        assert!(ema > 684_000 && ema <= 700_000);
        // An input equal to the EMA advances the anchor, so a later jump is not weighted by the idle time.
        assert_eq!(ema_step(684_000, 684, 5_000, hl).unwrap(), (684_000, true));
    }

    #[test]
    fn agreement_ignores_a_single_outlier_with_three_publishers() {
        let (m, s) = agreeing_value(&[700, 705, 30_000], 300, None);
        assert_eq!(
            (m, s),
            (700, 2),
            "the outlier does not join the honest pair"
        );
        let (m, s) = agreeing_value(&[30_000, 700, 0], 300, None);
        assert_eq!(
            (m, s),
            (0, 1),
            "three mutually distant observations: every cluster is a singleton, the lowest is reported"
        );
        // M-15: an outlier cannot break an honest pair that agrees within the band, whatever the band.
        let (m, s) = agreeing_value(&[700, 740, 30_000], 25, None);
        assert_eq!(
            (m, s),
            (700, 1),
            "700 and 740 are more than 25 bp apart: no agreement"
        );
        let (m, s) = agreeing_value(&[700, 740, 30_000], 40, None);
        assert_eq!(
            (m, s),
            (700, 2),
            "within 40 bp they agree; the outlier changes nothing"
        );
    }

    #[test]
    fn two_observations_must_agree_between_themselves() {
        // M-6: agreement is between observations, never around their midpoint.
        let (m, s) = agreeing_value(&[700, 710], 300, None);
        assert_eq!((m, s), (700, 2));
        let (m, s) = agreeing_value(&[700, 1_300], 300, None);
        assert_eq!(
            s, 1,
            "600 bp apart with a 300 bp band: a disputed pair, not an accepted 1 000"
        );
        assert_eq!(m, 700);
        // Four observations split two and two cannot manufacture a midpoint either.
        let (m, s) = agreeing_value(&[700, 700, 1_300, 1_300], 300, None);
        assert_eq!(s, 2);
        assert_eq!(
            m, 700,
            "with nothing to prefer the lower cluster is reported"
        );
        let (m, _) = agreeing_value(&[700, 700, 1_300, 1_300], 300, Some(1_250));
        assert_eq!(
            m, 1_300,
            "a tie goes to the cluster nearest the current value"
        );
        assert_eq!(agreeing_value(&[], 300, None), (0, 0));
        assert_eq!(agreeing_value(&[684], 300, None), (684, 1));
        // The value is always an observation: the lower median of the cluster.
        assert_eq!(agreeing_value(&[700, 720, 740, 760], 100, None), (720, 4));
        assert_eq!(agreeing_value(&[700, 720, 740], 100, None), (720, 3));
    }

    #[test]
    fn freshness_is_that_of_the_oldest_supporting_observation() {
        // M-12: re-aggregating old observations does not refresh the benchmark.
        assert_eq!(
            supporting_min_slot(&[700, 705, 30_000], &[1_000, 900, 2_000], 700, 300),
            900
        );
        assert_eq!(supporting_min_slot(&[700], &[], 700, 300), 0);
    }

    #[test]
    fn band_and_drift_compose_and_a_stale_anchor_yields_to_the_band() {
        // Inside both: unchanged.
        assert_eq!(clamp_composed(900, 684, 300, 700, 400), (900, false, false));
        // Drift binds first: 684 + 300 = 984 allowed by the band, 700 + 150 = 850 by the drift bound.
        assert_eq!(clamp_composed(950, 684, 300, 700, 150), (850, true, false));
        // M-13: the anchor sits at 2 000 after a guard change while the EMA is 684; the intervals are disjoint,
        // so the band decides and the caller re-anchors.
        assert_eq!(
            clamp_composed(690, 684, 300, 2_000, 100),
            (690, false, true)
        );
        assert_eq!(
            clamp_composed(1_200, 684, 300, 2_000, 100),
            (984, true, true)
        );
    }

    #[test]
    fn band_widens_with_outage_and_clamps_instead_of_rejecting() {
        assert_eq!(effective_band(300, 0, 10_000), 300);
        assert_eq!(effective_band(300, 10_000, 10_000), 600);
        assert_eq!(
            effective_band(300, 1_000_000, 10_000),
            30_000,
            "saturates at the ceiling"
        );
        assert_eq!(clamp_to(1_200, 684, 300), (984, true));
        assert_eq!(clamp_to(100, 684, 300), (384, true));
        assert_eq!(clamp_to(900, 684, 300), (900, false));
        assert_eq!(
            clamp_to(30_000, 29_900, 300),
            (30_000, false),
            "ceiling is never exceeded"
        );
        assert_eq!(clamp_to(30_000, 29_500, 300), (29_800, true));
    }

    /// S-6 reproduction in miniature: 24 hourly publishes at the band edge move the value by at most
    /// `max_drift_bp` within one window, however many land.
    #[test]
    fn drift_bound_caps_a_band_edge_walk_inside_one_window() {
        let anchor = 684u16;
        let mut value = anchor;
        let mut ema = anchor;
        for _ in 0..24 {
            let target = ema.saturating_add(300); // sits at the band edge every hour
            let (v1, _) = clamp_to(target, ema, 300);
            let (v2, _) = clamp_to(v1, anchor, GUARDS.max_drift_bp);
            value = v2;
            ema = ema + (v2 - ema) / 2; // generous EMA; the bound does not depend on it
        }
        assert_eq!(value, anchor + GUARDS.max_drift_bp);
        assert!(ema <= anchor + GUARDS.max_drift_bp);
    }

    #[test]
    fn guard_floors_cannot_be_walked_to_zero() {
        assert!(validate_guards(&GUARDS).is_ok());
        let zero_band = GuardArgs {
            band_bp: 0,
            ..GUARDS
        };
        assert!(validate_guards(&zero_band).is_err());
        let tiny_band = GuardArgs {
            band_bp: MIN_BAND_BP - 1,
            ..GUARDS
        };
        assert!(validate_guards(&tiny_band).is_err());
        let zero_drift = GuardArgs {
            max_drift_bp: 0,
            ..GUARDS
        };
        assert!(validate_guards(&zero_drift).is_err());
        let short_staleness = GuardArgs {
            max_staleness_slots: 10,
            ..GUARDS
        };
        assert!(validate_guards(&short_staleness).is_err());
        let interval_too_long = GuardArgs {
            min_interval_slots: 1_001,
            ..GUARDS
        };
        assert!(
            validate_guards(&interval_too_long).is_err(),
            "two observations must fit in a window"
        );
        let huge_window = GuardArgs {
            drift_window_slots: MAX_DRIFT_WINDOW_SLOTS + 1,
            ..GUARDS
        };
        assert!(validate_guards(&huge_window).is_err());
    }

    #[test]
    fn publisher_set_validation() {
        let a = Pubkey::new_from_array([1; 32]);
        let b = Pubkey::new_from_array([2; 32]);
        let z = Pubkey::default();
        assert_eq!(validate_publishers(&[a, b, z, z, z]).unwrap(), 2);
        assert_eq!(validate_publishers(&[z, a, z, z, z]).unwrap(), 1);
        assert!(validate_publishers(&[a, a, z, z, z]).is_err(), "duplicate");
        assert!(validate_publishers(&[z; MAX_PUBLISHERS]).is_err(), "empty");
    }

    #[test]
    fn tiers_are_fresh_degraded_stale() {
        let mut b = Benchmark {
            version: LAYOUT_VERSION,
            registry: Pubkey::default(),
            id: [0; 16],
            source: Pubkey::default(),
            value_bp: 684,
            ema_bp: 684,
            slot: 1_000,
            unix_ts: 0,
            accrual_e18: 0,
            max_staleness_slots: 2_000,
            band_bp: 300,
            half_life_slots: 10_000,
            min_interval_slots: 150,
            published: true,
            publish_count: 1,
            bump: 0,
            prev_value_bp: 684,
            prev_unix_ts: 0,
            max_drift_bp: 400,
            drift_window_slots: 216_000,
            drift_anchor_bp: 684,
            drift_anchor_slot: 1_000,
            clamped: false,
            disputed: false,
            support: 2,
            observations: [Observation::default(); MAX_PUBLISHERS],
            ema_milli_bp: 0,
            fixing_first_day: 0,
            fixing_head_day: 0,
            fixings: [0; FIXING_BYTES],
            ema_slot: 1_000,
            accepted_slot: 1_000,
            _reserved: [0; 12],
        };
        assert_eq!(b.tier(1_000), Tier::Fresh);
        assert_eq!(b.tier(3_000), Tier::Fresh, "age == max_staleness is fresh");
        assert_eq!(b.tier(3_001), Tier::Degraded);
        assert_eq!(b.tier(5_000), Tier::Degraded);
        assert_eq!(b.tier(5_001), Tier::Stale);
        b.clamped = true;
        assert_eq!(b.tier(1_000), Tier::Degraded);
        b.clamped = false;
        b.disputed = true;
        assert_eq!(b.tier(1_000), Tier::Degraded);
        b.published = false;
        assert_eq!(b.tier(1_000), Tier::Stale);
    }

    #[test]
    fn layout_sizes_are_recorded() {
        // Facts for the migration note and the test harness mirror (plus the 8-byte discriminator).
        assert_eq!(Observation::INIT_SPACE, 42);
        assert_eq!(Registry::INIT_SPACE, 1 + 32 + 32 + 160 + 1 + 1 + 4 + 1 + 64);
        assert_eq!(
            Benchmark::INIT_SPACE,
            1 + 32
                + 16
                + 32
                + 2
                + 2
                + 8
                + 8
                + 16
                + 8
                + 2
                + 8
                + 8
                + 1
                + 8
                + 1
                + 2
                + 8
                + 2
                + 8
                + 2
                + 8
                + 1
                + 1
                + 1
                + 42 * 5
                + 4
                + 4
                + 4
                + FIXING_BYTES
                + 8
                + 8
                + 12
        );
    }
}
