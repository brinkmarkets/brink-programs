//! Account layouts. Fixed-size fields first, reserved padding on every account, explicit state machines.
use anchor_lang::prelude::*;

/// Kill switch and graceful degradation, designed in from day one.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq, Eq, Debug, InitSpace)]
pub enum OperatingMode {
    Normal,
    Limited,
    WithdrawOnly,
    Halted,
}

/// Vernier parameters as stored on chain (mirror of `vernier::Params`).
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq, Eq, Debug, InitSpace)]
pub struct VernierParamsOnChain {
    pub model_pay_bp: [u16; 4],
    pub model_rec_bp: [u16; 4],
    pub term_bp: [u16; 4],
    pub demand_k_bp: u16,
    pub demand_cap_bp: u16,
    pub collateral_bp: [u16; 4],
}
impl From<VernierParamsOnChain> for vernier::Params {
    fn from(p: VernierParamsOnChain) -> Self {
        vernier::Params {
            model_pay_bp: p.model_pay_bp,
            model_rec_bp: p.model_rec_bp,
            term_bp: p.term_bp,
            demand_k_bp: p.demand_k_bp,
            demand_cap_bp: p.demand_cap_bp,
            collateral_bp: p.collateral_bp,
        }
    }
}

#[account]
#[derive(InitSpace)]
pub struct Global {
    pub authority: Pubkey, // timelock PDA / governance account after bootstrap
    pub guardian: Pubkey,  // may only move mode towards Halted; independent of authority
    pub usdc_mint: Pubkey,
    pub token_program: Pubkey, // owner of the USDC mint (Token or Token-2022); every pool uses the same one
    pub treasury: Pubkey,      // USDC account receiving the treasury half of platform fees
    pub buyback_escrow: Pubkey, // USDC account receiving the BRINK buyback half; accrues until the token exists
    pub fee_vault: Pubkey,      // USDC account owned by this PDA holding unswept fees
    pub buyback_accrued: u64,   // unswept, 6 dp
    pub treasury_accrued: u64, // unswept, 6 dp (legacy: fees accrue per pool since review F-29; stays 0)
    pub buyback_lifetime: u64, // swept to date, updated by `sweep_fees`
    pub treasury_lifetime: u64, // swept to date, updated by `sweep_fees`
    pub mode: OperatingMode,
    pub param_delay_slots: u64, // 48h ≈ 432_000 slots
    pub limited_mode_cap: u64,  // max notional in Limited
    pub pool_count: u32,
    pub bump: u8,
    pub _reserved: [u8; 128],
}

/// Hook points. Hooks observe and veto entries only (open, deposit); no exit (withdraw, cancel, settle,
/// liquidate) makes a hook CPI in any mode, so no hook can ever veto or starve an exit (review F-30, ADR-003).
/// The four retired exit points keep their slots so the layout and the point discriminators are stable; they must
/// be false and never fire. Hook CPIs are made in `Normal` mode only.
#[derive(
    AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq, Eq, Debug, Default, InitSpace,
)]
pub struct HookFlags {
    /// Veto point: a non-zero return refuses the open.
    pub before_open: bool,
    /// Notification after the open is booked; the pool account bytes carry the post-open state.
    pub after_open: bool,
    /// Retired (was `before_cancel`). Must be false.
    pub reserved_2: bool,
    /// Retired (was `after_cancel`). Must be false.
    pub reserved_3: bool,
    /// Veto point: a non-zero return refuses the deposit.
    pub before_deposit: bool,
    /// Notification after the deposit is booked; the pool account bytes carry the post-deposit state.
    pub after_deposit: bool,
    /// Retired (was `before_withdraw`). Must be false.
    pub reserved_6: bool,
    /// Retired (was `after_settle`). Must be false.
    pub reserved_7: bool,
}
impl HookFlags {
    /// True when no retired exit point is set.
    #[must_use]
    pub fn exit_points_clear(&self) -> bool {
        !(self.reserved_2 || self.reserved_3 || self.reserved_6 || self.reserved_7)
    }
}

/// Which pricing curve quotes this pool. `External` is reserved for v1.1 (pricer CPI); creating such a pool
/// is permitted so governance can stage it, but quotes fail with `PricerMismatch` until the CPI path ships.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq, Eq, Debug, InitSpace)]
pub enum Pricer {
    Vernier,
    External { program: Pubkey },
}

#[account]
#[derive(InitSpace)]
pub struct Pool {
    pub benchmark: Pubkey,    // benchmark account published by the index program
    pub share_mint: Pubkey,   // LP shares, 6 dp, pool PDA is mint authority
    pub vault: Pubkey,        // USDC token account owned by the pool PDA: tvl + collateral_held
    pub hook_program: Pubkey, // Pubkey::default() when no hooks
    pub hooks: HookFlags,
    pub pricer: Pricer,
    pub params: VernierParamsOnChain, // effective calibration
    pub pending_params: VernierParamsOnChain, // queued calibration
    pub pending_effective_slot: u64,  // 0 when nothing queued
    pub tvl: u64,                     // LP capital incl. realised pnl, USDC 6 dp
    pub collateral_held: u64,         // trader collateral in the vault, not LP capital
    pub util_pay_bp: u16,
    pub util_rec_bp: u16,
    pub open_pay_notional: u64,
    pub open_rec_notional: u64,
    pub open_swaps: u32,
    pub fees_lifetime: u64,
    pub event_seq: u64, // per-pool event sequence for indexers
    pub min_notional: u64,
    pub max_notional: u64,
    pub bump: u8,
    /// Open book aggregates (ADR-008, ADR-017 section 2), kept per leg since external scan 1 (M-4): the pay-fixed
    /// side and the receive-fixed side are clamped to their own collateral separately, so an unbounded gain on
    /// one side can never be netted against an uncollectible loss on the other. Each side is signed with +1 for
    /// pay-fixed and -1 for receive-fixed swaps and maintained exactly by `open` (add) and `close` (subtract), so
    /// the unrealised value of the book is an O(1) function of the benchmark accrual, the published value and
    /// the clock; see `Pool::book_value`. Units: notional in USDC base units, times basis-point-seconds or seconds.
    pub book_pay: BookSide,
    pub book_rec: BookSide,
    /// Trader collateral held per side; `collateral_held` is their sum.
    pub collateral_pay: u64,
    pub collateral_rec: u64,
    /// Limited-mode budget window (simulation S-2): new notional opened in this pool since `limited_window_start`
    /// may not exceed `Global::limited_mode_cap` within `LIMITED_WINDOW_SECS`, in addition to the per-swap cap.
    pub limited_window_start: i64,
    pub limited_window_notional: u64,
    /// Unswept platform fees held in this pool's vault, buyback half (review F-29, ADR-001, ADR-013). Fees
    /// accrue per pool so the hot path never writes the singleton `Global` or a shared fee account.
    pub fees_buyback_accrued: u64,
    /// Unswept platform fees held in this pool's vault, treasury half.
    pub fees_treasury_accrued: u64,
    /// USDC set aside in the vault for processed, unclaimed queued withdrawals (ADR-009). Not LP capital.
    pub withdraw_reserved: u64,
    /// Maturity ladder (external scan 1, M-5): open swaps counted by maturity day. Maturities fall on UTC
    /// midnights, so a day bucket is exact: once a bucket's midnight has passed and its count is not zero, a
    /// matured swap is awaiting settlement and LP pricing is suspended until it is settled (anyone may settle it
    /// and is paid a bounty for doing so). Bucket `day % MATURITY_BUCKETS` holds six little-endian bytes: the
    /// `u32` day it counts and a `u16` count, so a stale bucket is detected rather than aliased. Raw bytes rather
    /// than typed arrays so that loading the account is a copy, not a per-element decode (stack frame bound).
    pub ladder: [u8; LADDER_BYTES],
    /// Mirror of the share mint supply, so that paths without the mint account (`trader_open_swap`) can value
    /// queued shares (M-18). Asserted equal to the mint wherever the mint is present.
    pub share_supply: u64,
    /// Mirror of the withdrawal queue's open epoch (M-18): shares queued and the slot of the epoch's first
    /// request, zero when none. A new swap may not consume the capacity an eligible epoch is waiting for.
    pub queued_shares: u64,
    pub queue_first_slot: u64,
    pub _reserved: [u8; 24],
}

/// Buckets in the maturity ladder: more than the longest tenor in days.
pub const MATURITY_BUCKETS: usize = 256;
/// Bytes per ladder bucket: `u32` day plus `u16` count.
pub const LADDER_BUCKET_BYTES: usize = 6;
pub const LADDER_BYTES: usize = MATURITY_BUCKETS * LADDER_BUCKET_BYTES;

/// One side of the open book: the four signed aggregates for one leg.
#[derive(
    AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq, Eq, Debug, Default, InitSpace,
)]
pub struct BookSide {
    pub notional: i128,        // sum of sign x notional
    pub accrual_start: i128,   // sum of sign x notional x (benchmark accrual at open / SCALE)
    pub maturity_weight: i128, // sum of sign x notional x matures_ts
    pub fixed_leg: i128,       // sum of sign x notional x fixed_bp x (matures_ts - opened_ts)
}

impl BookSide {
    fn add(&mut self, t: &BookTerms) -> Result<()> {
        use crate::errors::BrinkError::Overflow;
        self.notional = self.notional.checked_add(t.notional).ok_or(Overflow)?;
        self.accrual_start = self
            .accrual_start
            .checked_add(t.accrual_start)
            .ok_or(Overflow)?;
        self.maturity_weight = self
            .maturity_weight
            .checked_add(t.maturity_weight)
            .ok_or(Overflow)?;
        self.fixed_leg = self.fixed_leg.checked_add(t.fixed_leg).ok_or(Overflow)?;
        Ok(())
    }
    fn sub(&mut self, t: &BookTerms) -> Result<()> {
        use crate::errors::BrinkError::Overflow;
        self.notional = self.notional.checked_sub(t.notional).ok_or(Overflow)?;
        self.accrual_start = self
            .accrual_start
            .checked_sub(t.accrual_start)
            .ok_or(Overflow)?;
        self.maturity_weight = self
            .maturity_weight
            .checked_sub(t.maturity_weight)
            .ok_or(Overflow)?;
        self.fixed_leg = self.fixed_leg.checked_sub(t.fixed_leg).ok_or(Overflow)?;
        Ok(())
    }
    /// Unrealised value of this side to its traders, unclamped; see `Pool::book_value`.
    pub fn value(&self, a_now: i128, value_bp: u16, now: i64) -> Result<i128> {
        use crate::errors::BrinkError::Overflow;
        let accrued = self
            .notional
            .checked_mul(a_now)
            .ok_or(Overflow)?
            .checked_sub(self.accrual_start)
            .ok_or(Overflow)?;
        let remaining_secs = self
            .maturity_weight
            .checked_sub(self.notional.checked_mul(i128::from(now)).ok_or(Overflow)?)
            .ok_or(Overflow)?;
        let forward = i128::from(value_bp)
            .checked_mul(remaining_secs)
            .ok_or(Overflow)?;
        let num = accrued
            .checked_add(forward)
            .ok_or(Overflow)?
            .checked_sub(self.fixed_leg)
            .ok_or(Overflow)?;
        num.checked_div(
            i128::try_from(crate::instructions::math::BP_SECONDS_PER_YEAR).map_err(|_| Overflow)?,
        )
        .ok_or(Overflow.into())
    }
}

/// Length of the Limited-mode budget window.
pub const LIMITED_WINDOW_SECS: i64 = 3_600;

/// One swap's contribution to the pool book, signed for its leg.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BookTerms {
    pub leg: LegKind,
    pub notional: i128,
    pub accrual_start: i128,
    pub maturity_weight: i128,
    pub fixed_leg: i128,
}

impl BookTerms {
    /// Terms for a swap from the fields `Swap` stores; exact integers, no rounding (the accrual at open is on the
    /// live path and therefore a multiple of `SCALE`).
    pub fn for_swap(
        leg: LegKind,
        notional: u64,
        index_accrual_start: u128,
        fixed_bp: u16,
        opened_ts: i64,
        matures_ts: i64,
    ) -> Result<Self> {
        use crate::errors::BrinkError::Overflow;
        let n = i128::from(notional);
        let a = i128::try_from(
            index_accrual_start
                .checked_div(brink_index::ACCRUAL_SCALE)
                .ok_or(Overflow)?,
        )
        .map_err(|_| Overflow)?;
        let term = i128::from(matures_ts.checked_sub(opened_ts).ok_or(Overflow)?);
        let sign: i128 = match leg {
            LegKind::PayFixed => 1,
            LegKind::ReceiveFixed => -1,
        };
        let sn = n.checked_mul(sign).ok_or(Overflow)?;
        Ok(Self {
            leg,
            notional: sn,
            accrual_start: sn.checked_mul(a).ok_or(Overflow)?,
            maturity_weight: sn.checked_mul(i128::from(matures_ts)).ok_or(Overflow)?,
            fixed_leg: sn
                .checked_mul(i128::from(fixed_bp))
                .ok_or(Overflow)?
                .checked_mul(term)
                .ok_or(Overflow)?,
        })
    }
}

impl BookTerms {
    /// Terms for a forward-starting swap before its start. With `notional = 0` and
    /// `accrual_start = 0` the accrued leg of `BookSide::value` vanishes and the remaining-term leg reduces to
    /// `value_bp · notional · term`, so the book marks the forward at the published value held flat over its
    /// own term against its fixed leg, with no new pool state.
    pub fn for_forward(
        leg: LegKind,
        notional: u64,
        fixed_bp: u16,
        start_ts: i64,
        matures_ts: i64,
    ) -> Result<Self> {
        use crate::errors::BrinkError::Overflow;
        let n = i128::from(notional);
        let term = i128::from(matures_ts.checked_sub(start_ts).ok_or(Overflow)?);
        let sign: i128 = match leg {
            LegKind::PayFixed => 1,
            LegKind::ReceiveFixed => -1,
        };
        let sn = n.checked_mul(sign).ok_or(Overflow)?;
        Ok(Self {
            leg,
            notional: 0,
            accrual_start: 0,
            maturity_weight: sn.checked_mul(term).ok_or(Overflow)?,
            fixed_leg: sn
                .checked_mul(i128::from(fixed_bp))
                .ok_or(Overflow)?
                .checked_mul(term)
                .ok_or(Overflow)?,
        })
    }
}

impl Pool {
    /// Platform fees sitting in the vault that belong to neither LPs nor traders.
    pub fn fees_held(&self) -> Result<u64> {
        self.fees_buyback_accrued
            .checked_add(self.fees_treasury_accrued)
            .ok_or(crate::errors::BrinkError::Overflow.into())
    }
    /// Charges `notional` against the Limited-mode budget: at most `cap` of new notional per pool per
    /// `LIMITED_WINDOW_SECS`, whichever trader opens it and however it is split (simulation S-2). The window starts
    /// at the first open after a quiet hour; the per-swap cap is checked by the caller.
    pub fn charge_limited_budget(&mut self, now: i64, notional: u64, cap: u64) -> Result<i64> {
        use crate::errors::BrinkError::Overflow;
        let elapsed = now.checked_sub(self.limited_window_start).ok_or(Overflow)?;
        if !(0..LIMITED_WINDOW_SECS).contains(&elapsed) {
            self.limited_window_start = now;
            self.limited_window_notional = 0;
        }
        let used = self
            .limited_window_notional
            .checked_add(notional)
            .ok_or(Overflow)?;
        require!(used <= cap, crate::errors::BrinkError::LimitedModeCap);
        self.limited_window_notional = used;
        Ok(self.limited_window_start)
    }
    /// Releases the budget a closing swap was charged, when its window is still the open one, so that a swap
    /// cannot hold the shared hourly budget after it has gone (M-17).
    pub fn release_limited_budget(&mut self, window_start: i64, notional: u64) {
        if window_start != 0 && window_start == self.limited_window_start {
            self.limited_window_notional = self.limited_window_notional.saturating_sub(notional);
        }
    }
    fn side_mut(&mut self, leg: LegKind) -> &mut BookSide {
        match leg {
            LegKind::PayFixed => &mut self.book_pay,
            LegKind::ReceiveFixed => &mut self.book_rec,
        }
    }
    /// Adds a swap to the book (on open).
    pub fn book_add(&mut self, t: &BookTerms) -> Result<()> {
        self.side_mut(t.leg).add(t)
    }
    /// Removes a swap from the book (on any close). Exactly reverses `book_add`.
    pub fn book_sub(&mut self, t: &BookTerms) -> Result<()> {
        self.side_mut(t.leg).sub(t)
    }
    fn a_now(accrual_now_e18: u128) -> Result<i128> {
        use crate::errors::BrinkError::Overflow;
        i128::try_from(
            accrual_now_e18
                .checked_div(brink_index::ACCRUAL_SCALE)
                .ok_or(Overflow)?,
        )
        .map_err(|_| Overflow.into())
    }
    /// Unrealised value of the whole open book to traders (positive: the pool owes; negative: traders owe), in
    /// USDC base units, unclamped. For each swap this is the accrued leg to `now` plus the remaining term valued
    /// at the current published rate held flat; summed over a side the fixed legs collapse to one constant per
    /// swap, so each side is
    /// `[N x a(now) - A + v x (M - now x N) - F] / (10_000 x 365 x 86_400)` with `N, A, M, F` the four
    /// aggregates, `a(now)` the benchmark accrual in basis-point-seconds and `v` the published value (ADR-008).
    /// Exact for the accrued part; the remaining part uses a flat forward at `v` (no spread), which is the
    /// mid of the two unwind quotes. Swaps past maturity are not in this mark: LP pricing is suspended while any
    /// matured swap awaits settlement (`has_matured_open`, M-5).
    pub fn book_value(&self, accrual_now_e18: u128, value_bp: u16, now: i64) -> Result<i128> {
        let a = Self::a_now(accrual_now_e18)?;
        self.book_pay
            .value(a, value_bp, now)?
            .checked_add(self.book_rec.value(a, value_bp, now)?)
            .ok_or(crate::errors::BrinkError::Overflow.into())
    }
    /// `book_value` with each side clamped to what can actually change hands on that side: its traders can win
    /// at most the collateral they posted and lose at most the same (every close clamps to collateral), and the
    /// pool can pay at most `tvl` in total. Clamping per side means a gain beyond collateral on one leg is never
    /// netted against an uncollectible loss on the other (M-4). Offsets within one leg remain aggregate: see the
    /// cap-out close in `liquidate`, which closes any swap whose unbounded mark has reached its collateral, and
    /// the collateral floors in the calibration bounds.
    pub fn book_value_clamped(
        &self,
        accrual_now_e18: u128,
        value_bp: u16,
        now: i64,
    ) -> Result<i128> {
        use crate::errors::BrinkError::Overflow;
        let a = Self::a_now(accrual_now_e18)?;
        let clamp_side = |v: i128, c: u64| -> Result<i128> {
            let c = i128::from(c);
            Ok(v.clamp(c.checked_neg().ok_or(Overflow)?, c))
        };
        let pay = clamp_side(self.book_pay.value(a, value_bp, now)?, self.collateral_pay)?;
        let rec = clamp_side(self.book_rec.value(a, value_bp, now)?, self.collateral_rec)?;
        let v = pay.checked_add(rec).ok_or(Overflow)?;
        Ok(v.min(i128::from(self.tvl)))
    }
    fn ladder_bucket(&self, i: usize) -> Result<(u32, u16)> {
        use crate::errors::BrinkError::Overflow;
        let off = i.checked_mul(LADDER_BUCKET_BYTES).ok_or(Overflow)?;
        let b = self
            .ladder
            .get(off..off.checked_add(LADDER_BUCKET_BYTES).ok_or(Overflow)?)
            .ok_or(Overflow)?;
        let day = u32::from_le_bytes(
            b.get(0..4)
                .ok_or(Overflow)?
                .try_into()
                .map_err(|_| Overflow)?,
        );
        let count = u16::from_le_bytes(
            b.get(4..6)
                .ok_or(Overflow)?
                .try_into()
                .map_err(|_| Overflow)?,
        );
        Ok((day, count))
    }
    fn set_ladder_bucket(&mut self, i: usize, day: u32, count: u16) -> Result<()> {
        use crate::errors::BrinkError::Overflow;
        let off = i.checked_mul(LADDER_BUCKET_BYTES).ok_or(Overflow)?;
        let b = self
            .ladder
            .get_mut(off..off.checked_add(LADDER_BUCKET_BYTES).ok_or(Overflow)?)
            .ok_or(Overflow)?;
        b.get_mut(0..4)
            .ok_or(Overflow)?
            .copy_from_slice(&day.to_le_bytes());
        b.get_mut(4..6)
            .ok_or(Overflow)?
            .copy_from_slice(&count.to_le_bytes());
        Ok(())
    }
    fn ladder_index(matures_ts: i64) -> Result<(u32, usize)> {
        use crate::errors::BrinkError::Overflow;
        let day = brink_index::Benchmark::day_of(matures_ts).map_err(|_| Overflow)?;
        Ok((
            day,
            usize::try_from(day).map_err(|_| Overflow)? % MATURITY_BUCKETS,
        ))
    }
    /// Adds an open swap to the maturity ladder (M-5). `matures_ts` is a UTC midnight.
    pub fn ladder_add(&mut self, matures_ts: i64) -> Result<()> {
        use crate::errors::BrinkError;
        let (day, i) = Self::ladder_index(matures_ts)?;
        let (d, c) = self.ladder_bucket(i)?;
        if c > 0 {
            // A bucket still counting a swap from an earlier cycle is a matured swap awaiting settlement.
            require!(d == day, BrinkError::MaturedUnsettled);
        }
        self.set_ladder_bucket(i, day, c.checked_add(1).ok_or(BrinkError::Overflow)?)
    }
    /// Removes a closing swap from the maturity ladder. Exactly reverses `ladder_add`.
    pub fn ladder_sub(&mut self, matures_ts: i64) -> Result<()> {
        use crate::errors::BrinkError;
        let (day, i) = Self::ladder_index(matures_ts)?;
        let (d, c) = self.ladder_bucket(i)?;
        require!(d == day && c > 0, BrinkError::PoolInvariant);
        self.set_ladder_bucket(i, day, c.checked_sub(1).ok_or(BrinkError::Overflow)?)
    }
    /// True when an open swap has passed its maturity and not yet been settled (M-5). While this holds the
    /// aggregate mark cannot value the book (a matured swap's payoff is fixed by its maturity fixing, not by the
    /// current rate), so deposits, withdrawals and queue processing wait for the settlement crank.
    pub fn has_matured_open(&self, now: i64) -> bool {
        let today = brink_index::Benchmark::day_of(now).unwrap_or(u32::MAX);
        self.ladder
            .as_chunks::<LADDER_BUCKET_BYTES>()
            .0
            .iter()
            .any(|b| {
                let day = b
                    .get(0..4)
                    .and_then(|x| x.try_into().ok())
                    .map(u32::from_le_bytes)
                    .unwrap_or(u32::MAX);
                let count = b
                    .get(4..6)
                    .and_then(|x| x.try_into().ok())
                    .map(u16::from_le_bytes)
                    .unwrap_or(0);
                count > 0 && day <= today
            })
    }
    /// Refuses LP pricing while a matured swap awaits settlement (M-5).
    pub fn require_no_matured_open(&self, now: i64) -> Result<()> {
        require!(
            !self.has_matured_open(now),
            crate::errors::BrinkError::MaturedUnsettled
        );
        Ok(())
    }
    /// The share-supply mirror must agree with the mint wherever the mint is present (M-18).
    pub fn assert_share_supply(&self, mint_supply: u64) -> Result<()> {
        require!(
            self.share_supply == mint_supply,
            crate::errors::BrinkError::ShareSupplyMismatch
        );
        Ok(())
    }
    /// LP capital for pricing a deposit: fair value, `tvl` less what the pool owes the book or plus what the book
    /// owes the pool. A depositor neither captures a settlement that is already visible nor dilutes the LPs who
    /// bore the risk (maths finding M-12, simulation S-4).
    pub fn effective_tvl_for_deposit(
        &self,
        accrual_now_e18: u128,
        value_bp: u16,
        now: i64,
    ) -> Result<u64> {
        let v = self.book_value_clamped(accrual_now_e18, value_bp, now)?;
        let e = i128::from(self.tvl)
            .checked_sub(v)
            .ok_or(crate::errors::BrinkError::Overflow)?;
        Ok(u64::try_from(e.max(0)).unwrap_or(u64::MAX))
    }
    /// LP capital for pricing a withdrawal: conservative, `tvl` less what the pool owes the book, with no credit
    /// for unrealised trader losses (they are credited to the LPs who remain when the swaps close).
    pub fn effective_tvl_for_withdraw(
        &self,
        accrual_now_e18: u128,
        value_bp: u16,
        now: i64,
    ) -> Result<u64> {
        let v = self
            .book_value_clamped(accrual_now_e18, value_bp, now)?
            .max(0);
        let e = i128::from(self.tvl)
            .checked_sub(v)
            .ok_or(crate::errors::BrinkError::Overflow)?;
        Ok(u64::try_from(e.max(0)).unwrap_or(u64::MAX))
    }
    /// Everything the vault must hold: `tvl + collateral_held + fees_held + withdraw_reserved`.
    pub fn accounted(&self) -> Result<u64> {
        self.tvl
            .checked_add(self.collateral_held)
            .and_then(|x| x.checked_add(self.fees_held().ok()?))
            .and_then(|x| x.checked_add(self.withdraw_reserved))
            .ok_or(crate::errors::BrinkError::Overflow.into())
    }
    #[must_use]
    pub fn vernier_pool(&self) -> vernier::Pool {
        vernier::Pool {
            tvl: self.tvl,
            util_pay_bp: self.util_pay_bp,
            util_rec_bp: self.util_rec_bp,
        }
    }
    /// Caps plus conservation, for the entry paths (`trader_open_swap`, `lp_withdraw`) and for the paths that
    /// only add capital (`lp_deposit`, `sync_vault`), where the caps cannot be breached. Close paths call
    /// `assert_conservation` only (maths finding M-4).
    pub fn assert_invariants(&self, vault_amount: u64) -> Result<()> {
        self.assert_caps()?;
        self.assert_conservation(vault_amount)
    }
    /// Utilisation caps are entry constraints: they bound what may be opened and what LPs may withdraw. With no
    /// LP capital left nothing may remain open against it (the last LP may leave an empty book).
    pub fn assert_caps(&self) -> Result<()> {
        if self.tvl == 0 {
            require!(
                self.open_pay_notional == 0 && self.open_rec_notional == 0,
                crate::errors::BrinkError::PoolInvariant
            );
            return Ok(());
        }
        require!(
            vernier::pool_invariants_hold(&self.vernier_pool()),
            crate::errors::BrinkError::PoolInvariant
        );
        Ok(())
    }
    /// Conservation and well-formedness. Called at the end of every instruction that touches the pool, including
    /// every close path: a settle, cancel or liquidation is never refused because the pool is over a cap or
    /// because trader gains have consumed the LP capital (maths finding M-4).
    pub fn assert_conservation(&self, vault_amount: u64) -> Result<()> {
        require!(
            self.util_pay_bp <= 10_000 && self.util_rec_bp <= 10_000,
            crate::errors::BrinkError::PoolInvariant
        );
        // Conservation: the vault holds at least LP capital plus trader collateral plus unswept platform fees
        // plus processed withdrawals awaiting claim. Strict equality would let anyone brick the pool by
        // transferring dust straight to the vault; a surplus is instead folded into LP capital by the
        // permissionless `sync_vault` instruction.
        let expected = self.accounted()?;
        require!(
            vault_amount >= expected,
            crate::errors::BrinkError::Conservation
        );
        Ok(())
    }
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq, Eq, Debug, InitSpace)]
pub enum SwapState {
    Open,
    Settled {
        pnl: i64,
    },
    Liquidated {
        pnl: i64,
    },
    Cancelled {
        pnl: i64,
    },
    /// Closed early by the permissionless crank because its unbounded mark had reached its collateral: the trader
    /// is paid the full collateral gain (M-4 cap-out).
    Capped {
        pnl: i64,
    },
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq, Eq, Debug, InitSpace)]
pub enum LegKind {
    PayFixed,
    ReceiveFixed,
}

#[account]
#[derive(InitSpace)]
pub struct Swap {
    pub pool: Pubkey,
    pub trader: Pubkey,
    pub leg: LegKind,
    pub tenor: u8, // index into tenor tables
    pub notional: u64,
    pub fixed_bp: u16,
    pub collateral: u64,
    pub opened_slot: u64,
    pub opened_ts: i64,
    pub matures_ts: i64,
    pub index_accrual_start: u128, // benchmark cumulative accrual at open, for the average index
    pub client_seed: u64,
    pub state: SwapState,
    pub bump: u8,
    /// Limited-mode window this swap's notional was charged to, 0 when opened outside Limited mode (M-17).
    pub limited_window_start: i64,
    /// Basis swap link: the key of the other leg's `Swap` account, zero for an ordinary swap.
    /// Carved out of the former 56-byte reserve together with `link_flags`, so the account size is unchanged
    /// and every swap written before the link existed reads as unlinked.
    pub link: Pubkey,
    /// `LINK_BASIS` when this swap is one leg of a basis swap; `LINK_LEG_B` additionally on the receive-fixed
    /// leg. Zero on an ordinary swap.
    pub link_flags: u8,
    /// Forward-starting swap: the UTC midnight the floating accrual begins. Zero on a spot
    /// swap, and ignored unless `LINK_FORWARD` is set, so an account written before this field reads unchanged.
    pub start_ts: i64,
    pub _reserved: [u8; 15],
}

/// `Swap::link_flags` bit: the swap is one leg of a basis swap and `link` names the other leg.
pub const LINK_BASIS: u8 = 1;
/// `Swap::link_flags` bit: this is the second (receive-fixed) leg of the pair; leg A carries `LINK_BASIS` alone.
pub const LINK_LEG_B: u8 = 2;
/// `Swap::link_flags` bit: a forward-starting swap; `start_ts` is the first accrual instant.
pub const LINK_FORWARD: u8 = 4;
/// `Swap::link_flags` bit: the forward's start snapshot has been taken (`index_accrual_start` is set and the
/// book carries the ordinary terms). Meaningless without `LINK_FORWARD`.
pub const FORWARD_STARTED: u8 = 8;
/// Serialised size of `Swap` before and after the link and forward fields: a layout constant the tests pin.
pub const SWAP_SPACE: usize = 206;

impl Swap {
    /// True when this swap is one leg of a basis swap. A zero link is an ordinary swap whatever the flags say,
    /// so a pre-link swap (zero reserve) can never be read as linked.
    #[must_use]
    pub fn is_linked(&self) -> bool {
        self.link != Pubkey::default() && self.link_flags & LINK_BASIS != 0
    }
    /// True for a forward-starting swap.
    #[must_use]
    pub fn is_forward(&self) -> bool {
        self.link_flags & LINK_FORWARD != 0
    }
    /// True once a forward's start snapshot has been taken. Always true for a spot swap, whose accrual starts
    /// at open.
    #[must_use]
    pub fn is_started(&self) -> bool {
        !self.is_forward() || self.link_flags & FORWARD_STARTED != 0
    }
    /// The instant the floating accrual starts: `start_ts` for a forward, `opened_ts` otherwise. The only
    /// place the settlement arithmetic distinguishes the two.
    #[must_use]
    pub fn accrual_from(&self) -> i64 {
        if self.is_forward() {
            self.start_ts
        } else {
            self.opened_ts
        }
    }
    /// The book terms this swap currently contributes: the forward terms before its start, the ordinary terms
    /// from the start on. `open` adds and `close` subtracts exactly this, and the start crank swaps one for
    /// the other, so the book is exact at every instant.
    pub fn book_terms(&self) -> Result<BookTerms> {
        if self.is_started() {
            BookTerms::for_swap(
                self.leg,
                self.notional,
                self.index_accrual_start,
                self.fixed_bp,
                self.accrual_from(),
                self.matures_ts,
            )
        } else {
            BookTerms::for_forward(self.leg, self.notional, self.fixed_bp, self.start_ts, self.matures_ts)
        }
    }
    /// Flags for one leg of a basis swap.
    #[must_use]
    pub const fn link_flags_for(leg: LegKind) -> u8 {
        match leg {
            LegKind::PayFixed => LINK_BASIS,
            LegKind::ReceiveFixed => LINK_BASIS | LINK_LEG_B,
        }
    }
}

/// Governance-set correlation offset for one ordered benchmark pair: a basis swap pays fixed
/// on `pool_a` and receives fixed on `pool_b`. `correlation_bp` is the fraction of each leg's demand charge that
/// the pair's documented positive correlation offsets; it never touches the model or tenor components. The
/// account's absence means basis swaps are not enabled for the pair. Seeds `["basis", pool_a, pool_b]`.
#[account]
#[derive(InitSpace)]
pub struct BasisPair {
    pub pool_a: Pubkey,
    pub pool_b: Pubkey,
    pub correlation_bp: u16,
    pub bump: u8,
    pub _reserved: [u8; 32],
}

/// Largest correlation offset governance may set, bp. Half of the demand charge: the pair's correlation is
/// evidence for netting part of the imbalance risk, never all of it, so each leg keeps paying for at least half
/// of the book it extends.
pub const MAX_CORRELATION_BP: u16 = 5_000;

/// Events, emitted through `emit_cpi!` with the pool sequence so indexers detect gaps.
#[event]
pub struct SwapOpened {
    pub pool: Pubkey,
    pub swap: Pubkey,
    pub trader: Pubkey,
    pub leg: LegKind,
    pub tenor: u8,
    pub notional: u64,
    pub fixed_bp: u16,
    pub collateral: u64,
    pub seq: u64,
}
#[event]
pub struct SwapClosed {
    pub pool: Pubkey,
    pub swap: Pubkey,
    pub state: SwapState,
    pub payout: u64,
    pub bounty: u64,
    pub seq: u64,
    /// `swap::FixingKind` as `u8`: how the accrual at maturity was read (0 live, 1 segment, 2 fixing,
    /// 3 interpolated, 4 fallback). Always 0 for cancel before maturity and for liquidation.
    pub fixing_kind: u8,
}
/// A forward-starting swap's start snapshot was taken: `accrual_start` is the benchmark's
/// cumulative accrual at `start_ts`, `fixing_kind` says how it was read (`swap::FixingKind` as `u8`).
#[event]
pub struct ForwardStarted {
    pub pool: Pubkey,
    pub swap: Pubkey,
    pub start_ts: i64,
    pub accrual_start: u128,
    pub fixing_kind: u8,
    pub seq: u64,
}
#[event]
pub struct LiquidityChanged {
    pub pool: Pubkey,
    pub lp: Pubkey,
    pub amount: i64,
    pub shares: i64,
    pub share_price_e6: u64,
    pub seq: u64,
}
#[event]
pub struct FeeCollected {
    pub pool: Pubkey,
    pub kind: crate::instructions::fees::FeeKind,
    pub fee: u64,
    pub buyback: u64,
    pub treasury: u64,
    pub seq: u64,
}
#[event]
pub struct FeesSwept {
    pub pool: Pubkey,
    pub buyback: u64,
    pub treasury: u64,
}
#[event]
pub struct ModeChanged {
    pub mode: OperatingMode,
    pub by: Pubkey,
}
#[event]
pub struct CalibrationQueued {
    pub pool: Pubkey,
    pub effective_slot: u64,
}
#[event]
pub struct PoolCreated {
    pub pool: Pubkey,
    pub benchmark: Pubkey,
    pub share_mint: Pubkey,
    pub vault: Pubkey,
}
/// One basis swap opened: two linked legs, each also announced by its own `SwapOpened` on its pool.
#[event]
pub struct BasisSwapOpened {
    pub trader: Pubkey,
    pub pool_a: Pubkey,
    pub swap_a: Pubkey,
    pub pool_b: Pubkey,
    pub swap_b: Pubkey,
    pub tenor: u8,
    pub notional: u64,
    pub fixed_pay_bp: u16,
    pub fixed_receive_bp: u16,
    pub correlation_bp: u16,
    /// Demand charge removed across both legs by the correlation offset, bp.
    pub offset_bp: u16,
}
/// Both legs of a basis swap closed by the trader in one instruction; each leg also emits `SwapClosed`.
#[event]
pub struct BasisSwapClosed {
    pub trader: Pubkey,
    pub swap_a: Pubkey,
    pub swap_b: Pubkey,
    pub payout: u64,
}
/// A basis pair created or updated by governance.
#[event]
pub struct BasisPairSet {
    pub pool_a: Pubkey,
    pub pool_b: Pubkey,
    pub correlation_bp: u16,
}
