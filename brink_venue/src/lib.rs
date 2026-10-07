//! Brink venue adapter: the floating leg of a fixed vault.
//!
//! A fixed vault earns a benchmark's floating rate by placing USDC at the benchmark's venue and swaps that
//! floating for fixed on the Brink pool. The vault never talks to a venue directly; it talks to a venue adapter
//! with this interface, so a new venue is a new adapter and not a change to the vault:
//!
//! ```text
//! place(amount)      USDC in, receipt tokens out at the adapter's index
//! redeem(receipts)   receipt tokens in, USDC out at the adapter's index
//! Venue.index_e18    USDC per receipt base unit, scaled by 1e18
//! ```
//!
//! This crate carries the interface and the devnet venue. The devnet venue is a stand-in for the lending venues
//! the benchmarks are read from: it accrues exactly the published benchmark, in step with the index program's
//! cumulative accrual, and pays that accrual from a reserve the deployer funds with devnet USDC. It is created only
//! by this program's upgrade authority, one per benchmark, and every surface that shows it names it as the devnet
//! venue. On mainnet the same interface fronts the venue itself.
//!
//! * The index moves only forward and only with the benchmark, and it is a function of the benchmark's history
//!   alone, not of when the venue was touched: it compounds once a day on the benchmark's midnight fixings and
//!   accrues simply inside the day (the construction of an overnight-rate index). `Venue::index_at` reproduces
//!   the index at any instant from the account and the benchmark without a CPI, so the AMM prices LP shares
//!   with the reserve's accrued yield and the split program settles at the maturity midnight whatever happened
//!   since (external scan 2, findings 1, 5, 6 and 20).
//! * Principal is always covered and never pays another holder's yield: `place` moves USDC into the reserve and
//!   `redeem` pays each receipt its share of what the reserve holds, `value x min(reserve, claims) / claims`,
//!   so a reserve short of the yield it owes pays every holder the same funded fraction of their yield instead
//!   of paying the first in full from the principal of the rest (external scan 2, finding 1). A caller that
//!   will not accept a haircut says so through `min_amount`. Anyone may `fund` the reserve.
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
use anchor_spl::token_interface::{
    self, Burn, Mint, MintTo, TokenAccount, TokenInterface, TransferChecked,
};
use brink_index::Benchmark;

declare_id!("Boj38zJFfsN72DBL2nrJEfk1ewskn11EbdikgHMBX7v2");

/// Index scale: USDC base units per receipt base unit times 1e18. A fresh venue starts at par.
pub const INDEX_SCALE: u128 = 1_000_000_000_000_000_000;
/// Basis-point seconds in a year: 10 000 bp times 31 536 000 s, the same constant the AMM accrues with.
pub const BP_SECONDS_PER_YEAR: u128 = 315_360_000_000;
/// Receipt decimals: the same as USDC, so one receipt is one USDC at par.
pub const RECEIPT_DECIMALS: u8 = 6;
/// Accrual units (`accrual_e18`, bp seconds times 1e18) are reduced by this before a growth step so that an index
/// up to about 1e19 times a day of accrual at the maximum rate stays inside `u128`; what the reduction drops is
/// below 1e-7 of a basis-point second.
const STEP_SCALE: u128 = 100_000_000_000;
/// One year of accrual in reduced units: the denominator of every growth step.
const YEAR_STEPS: u128 = BP_SECONDS_PER_YEAR * (brink_index::ACCRUAL_SCALE / STEP_SCALE);
/// Longest index walk one instruction performs, in midnights. A midnight costs about 1.2k compute units in the
/// fixings ring and about 1.7k on the benchmark's fallback beyond it, nearly all of it the two 128-bit divisions
/// of the growth step (software routines on SBF), so sixty midnights keep a touch after a gap of any length
/// near 100k units, inside the default budget with room for a calling program: `touch` moves the anchor by at
/// most this many days per call and converges, readers that need the index at `now` ask for a touch first, and
/// a reading further back than this takes one flat step beyond the bound (see `Venue::reading_at`).
pub const MAX_WALK_DAYS: u32 = 60;
/// Layout version written by this build. Version 1 accounts carry `anchor_day == 0` and are brought onto the
/// midnight anchor at their first accrual.
pub const VENUE_VERSION: u8 = 2;

#[program]
pub mod brink_venue {
    use super::*;

    /// Creates the devnet venue for a benchmark: its receipt mint and USDC reserve. Upgrade authority only.
    pub fn create_venue(ctx: Context<CreateVenue>) -> Result<()> {
        let clock = Clock::get()?;
        let b = &ctx.accounts.benchmark;
        let v = &mut ctx.accounts.venue;
        let now = clock.unix_timestamp;
        v.version = VENUE_VERSION;
        v.authority = ctx.accounts.authority.key();
        v.benchmark = b.key();
        v.usdc_mint = ctx.accounts.usdc_mint.key();
        v.reserve = ctx.accounts.reserve.key();
        v.receipt_mint = ctx.accounts.receipt_mint.key();
        v.index_e18 = INDEX_SCALE;
        v.bench_accrual_e18 = b.accrual_e18;
        v.tail_bps = tail_bps(b, now)?;
        v.last_ts = now;
        v.principal = 0;
        v.receipts = 0;
        v.funded = 0;
        v.paused = false;
        v.bump = ctx.bumps.venue;
        // Par at the midnight that starts today, so the index at creation is par plus today's accrual so far
        // and every later reading follows from the fixings.
        let day = Benchmark::day_of(now)?;
        v.anchor_day = day;
        v.anchor_accrual = brink_index::accrual_at(b, Benchmark::day_start(day)?)?;
        v.anchor_index_e18 = INDEX_SCALE;
        v.reserved = [0; 28];
        v.index_e18 = v.index_at(b, now)?;
        emit_cpi!(VenueCreated {
            venue: v.key(),
            benchmark: v.benchmark,
            receipt_mint: v.receipt_mint,
            reserve: v.reserve,
        });
        Ok(())
    }

    /// Places USDC and mints receipts at the current index.
    pub fn place(ctx: Context<Place>, amount: u64, min_receipts: u64) -> Result<()> {
        require!(amount > 0, VenueError::Zero);
        let now = Clock::get()?.unix_timestamp;
        let v = &mut ctx.accounts.venue;
        require!(!v.paused, VenueError::Paused);
        require!(
            v.accrue(ctx.accounts.benchmark.key(), &ctx.accounts.benchmark, now)?,
            VenueError::NeedsTouch
        );
        // Receipts are fungible and a short reserve pays every receipt the same, so a placement into a short
        // reserve would have the newcomer's principal fund earlier holders' yield. A placement is accepted only
        // while the reserve funds every claim: then the reserve per receipt is at least the index, redemptions
        // and fundings never lower it, and every later placement at a higher index keeps it at or above that
        // index, so each holder's receipts are always worth at least what was placed for them. Principal is
        // never paid to another holder (review of external scan 2, finding 1).
        require!(
            Venue::fully_funded(
                ctx.accounts.receipt_mint.supply.max(v.receipts),
                v.index_e18,
                ctx.accounts.reserve.amount
            )?,
            VenueError::Underfunded
        );
        let receipts = receipts_for(amount, v.index_e18)?;
        require!(receipts > 0, VenueError::Zero);
        require!(receipts >= min_receipts, VenueError::BelowMinimum);
        token_interface::transfer_checked(
            CpiContext::new(
                ctx.accounts.token_program.key(),
                TransferChecked {
                    from: ctx.accounts.owner_usdc.to_account_info(),
                    mint: ctx.accounts.usdc_mint.to_account_info(),
                    to: ctx.accounts.reserve.to_account_info(),
                    authority: ctx.accounts.owner.to_account_info(),
                },
            ),
            amount,
            ctx.accounts.usdc_mint.decimals,
        )?;
        let seeds: &[&[u8]] = &[Venue::SEED, v.benchmark.as_ref(), &[v.bump]];
        token_interface::mint_to(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.key(),
                MintTo {
                    mint: ctx.accounts.receipt_mint.to_account_info(),
                    to: ctx.accounts.owner_receipts.to_account_info(),
                    authority: v.to_account_info(),
                },
                &[seeds],
            ),
            receipts,
        )?;
        v.principal = v
            .principal
            .checked_add(amount)
            .ok_or(VenueError::Overflow)?;
        v.receipts = v
            .receipts
            .checked_add(receipts)
            .ok_or(VenueError::Overflow)?;
        emit_cpi!(Placed {
            venue: v.key(),
            owner: ctx.accounts.owner.key(),
            amount,
            receipts,
            index_e18: v.index_e18
        });
        Ok(())
    }

    /// Burns receipts and pays their value at the current index, scaled by the fraction of all claims the
    /// reserve holds (`Venue::payout`). `min_amount` is the caller's floor: pass the full value to refuse a
    /// haircut.
    pub fn redeem(ctx: Context<Redeem>, receipts: u64, min_amount: u64) -> Result<()> {
        require!(receipts > 0, VenueError::Zero);
        let now = Clock::get()?.unix_timestamp;
        let v = &mut ctx.accounts.venue;
        require!(
            v.accrue(ctx.accounts.benchmark.key(), &ctx.accounts.benchmark, now)?,
            VenueError::NeedsTouch
        );
        // Only the venue mints receipts, so the live supply is the claims outstanding; a holder who burned
        // receipts directly forfeited their claim to the rest, and the mirror follows the mint.
        let supply = ctx.accounts.receipt_mint.supply;
        require!(supply <= v.receipts, VenueError::Overflow);
        v.receipts = supply;
        let value = amount_for(receipts, v.index_e18)?;
        let amount = v.payout(receipts, ctx.accounts.reserve.amount, value)?;
        require!(amount >= min_amount, VenueError::BelowMinimum);
        let principal_part = u64::try_from(
            u128::from(receipts)
                .checked_mul(u128::from(v.principal))
                .ok_or(VenueError::Overflow)?
                .checked_div(u128::from(supply))
                .ok_or(VenueError::Overflow)?,
        )
        .map_err(|_| VenueError::Overflow)?;
        token_interface::burn(
            CpiContext::new(
                ctx.accounts.token_program.key(),
                Burn {
                    mint: ctx.accounts.receipt_mint.to_account_info(),
                    from: ctx.accounts.owner_receipts.to_account_info(),
                    authority: ctx.accounts.owner.to_account_info(),
                },
            ),
            receipts,
        )?;
        let seeds: &[&[u8]] = &[Venue::SEED, v.benchmark.as_ref(), &[v.bump]];
        token_interface::transfer_checked(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.key(),
                TransferChecked {
                    from: ctx.accounts.reserve.to_account_info(),
                    mint: ctx.accounts.usdc_mint.to_account_info(),
                    to: ctx.accounts.owner_usdc.to_account_info(),
                    authority: v.to_account_info(),
                },
                &[seeds],
            ),
            amount,
            ctx.accounts.usdc_mint.decimals,
        )?;
        // The receipts' pro rata share of deposited principal leaves the books; the rest of the payment is yield
        // from the funded reserve. The reserve never falls below the principal still owed (finding 1).
        v.principal = v.principal.saturating_sub(principal_part);
        v.receipts = v
            .receipts
            .checked_sub(receipts)
            .ok_or(VenueError::Overflow)?;
        emit_cpi!(Redeemed {
            venue: v.key(),
            owner: ctx.accounts.owner.key(),
            receipts,
            amount,
            index_e18: v.index_e18,
            value,
            shortfall: value.saturating_sub(amount),
        });
        Ok(())
    }

    /// Anyone adds USDC to the reserve that pays accrued yield.
    pub fn fund(ctx: Context<Fund>, amount: u64) -> Result<()> {
        require!(amount > 0, VenueError::Zero);
        token_interface::transfer_checked(
            CpiContext::new(
                ctx.accounts.token_program.key(),
                TransferChecked {
                    from: ctx.accounts.funder_usdc.to_account_info(),
                    mint: ctx.accounts.usdc_mint.to_account_info(),
                    to: ctx.accounts.reserve.to_account_info(),
                    authority: ctx.accounts.funder.to_account_info(),
                },
            ),
            amount,
            ctx.accounts.usdc_mint.decimals,
        )?;
        let v = &mut ctx.accounts.venue;
        v.funded = v.funded.checked_add(amount).ok_or(VenueError::Overflow)?;
        emit_cpi!(Funded {
            venue: v.key(),
            funder: ctx.accounts.funder.key(),
            amount
        });
        Ok(())
    }

    /// Brings the index up to date without moving funds, so readers see the current value. After a gap of more
    /// than `MAX_WALK_DAYS` one call advances the anchor by that many midnights; call it until `Touched.complete`.
    pub fn touch(ctx: Context<Touch>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let complete = ctx.accounts.venue.accrue(
            ctx.accounts.benchmark.key(),
            &ctx.accounts.benchmark,
            now,
        )?;
        emit_cpi!(Touched {
            venue: ctx.accounts.venue.key(),
            index_e18: ctx.accounts.venue.index_e18,
            anchor_day: ctx.accounts.venue.anchor_day,
            complete,
        });
        Ok(())
    }

    /// Pauses or resumes placements. Redemptions are never paused.
    pub fn set_paused(ctx: Context<SetPaused>, paused: bool) -> Result<()> {
        ctx.accounts.venue.paused = paused;
        Ok(())
    }
}

/// Benchmark accrual not yet in `accrual_e18`: the current value held from the last publication to `now`,
/// in basis-point seconds.
fn tail_bps(b: &Benchmark, now: i64) -> Result<u128> {
    let dt = u128::try_from(now.checked_sub(b.unix_ts).unwrap_or(0).max(0))
        .map_err(|_| VenueError::Overflow)?;
    u128::from(b.value_bp)
        .checked_mul(dt)
        .ok_or_else(|| VenueError::Overflow.into())
}

/// `value x min(reserve, claims) / claims`, floored: what a claim worth `value` is paid when every outstanding
/// claim is worth `claims` and the reserve holds `reserve`. Zero when nothing is outstanding.
pub fn funded_value(value: u64, claims: u64, reserve: u64) -> Result<u64> {
    if claims == 0 {
        return Ok(0);
    }
    if reserve >= claims {
        return Ok(value);
    }
    u64::try_from(
        u128::from(value)
            .checked_mul(u128::from(reserve))
            .ok_or(VenueError::Overflow)?
            .checked_div(u128::from(claims))
            .ok_or(VenueError::Overflow)?,
    )
    .map_err(|_| VenueError::Overflow.into())
}

pub fn receipts_for(amount: u64, index_e18: u128) -> Result<u64> {
    u64::try_from(
        u128::from(amount)
            .checked_mul(INDEX_SCALE)
            .ok_or(VenueError::Overflow)?
            .checked_div(index_e18)
            .ok_or(VenueError::Overflow)?,
    )
    .map_err(|_| VenueError::Overflow.into())
}

pub fn amount_for(receipts: u64, index_e18: u128) -> Result<u64> {
    u64::try_from(
        u128::from(receipts)
            .checked_mul(index_e18)
            .ok_or(VenueError::Overflow)?
            .checked_div(INDEX_SCALE)
            .ok_or(VenueError::Overflow)?,
    )
    .map_err(|_| VenueError::Overflow.into())
}

// ---------------------------------------------------------------- accounts

#[account]
#[derive(InitSpace)]
pub struct Venue {
    pub version: u8,
    pub authority: Pubkey,
    pub benchmark: Pubkey,
    pub usdc_mint: Pubkey,
    pub reserve: Pubkey,
    pub receipt_mint: Pubkey,
    /// USDC per receipt base unit, scaled by 1e18.
    pub index_e18: u128,
    /// The benchmark's cumulative accrual at the last touch.
    pub bench_accrual_e18: u128,
    /// Accrual counted at the last touch beyond the benchmark's last publication, in bp seconds.
    pub tail_bps: u128,
    pub last_ts: i64,
    /// USDC placed and not yet redeemed.
    pub principal: u64,
    /// Receipt supply mirror.
    pub receipts: u64,
    /// USDC added to the reserve by `fund`, cumulative.
    pub funded: u64,
    pub paused: bool,
    pub bump: u8,
    /// The index at 00:00 UTC of `anchor_day`, exact: the compounding anchor every reading is reconstructed from.
    pub anchor_index_e18: u128,
    /// UTC day number of the anchor midnight; zero on a version 1 account until its first accrual under this
    /// build, which places the anchor from the stored index.
    pub anchor_day: u32,
    /// The benchmark's cumulative accrual at the anchor midnight, in `accrual_e18` units.
    pub anchor_accrual: u128,
    pub reserved: [u8; 28],
}

/// A reconstructed reading: the index at an instant and the midnight anchor of its day.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reading {
    pub index_e18: u128,
    pub day: u32,
    pub day_index_e18: u128,
    pub day_accrual: u128,
}

impl Venue {
    pub const SEED: &'static [u8] = b"venue";

    /// `index + floor(index x accrual / year)`: one simple-accrual step over `accrual` units.
    fn grow(index: u128, accrual: u128) -> Result<u128> {
        let steps = accrual / STEP_SCALE;
        index
            .checked_mul(steps)
            .ok_or(VenueError::Overflow)?
            .checked_div(YEAR_STEPS)
            .ok_or(VenueError::Overflow)?
            .checked_add(index)
            .ok_or_else(|| VenueError::Overflow.into())
    }
    /// The inverse of `grow`, floored: `floor(index x year / (year + accrual))`.
    fn shrink(index: u128, accrual: u128) -> Result<u128> {
        let steps = accrual / STEP_SCALE;
        index
            .checked_mul(YEAR_STEPS)
            .ok_or(VenueError::Overflow)?
            .checked_div(YEAR_STEPS.checked_add(steps).ok_or(VenueError::Overflow)?)
            .ok_or_else(|| VenueError::Overflow.into())
    }

    /// The index at `t`, a pure function of this account and the benchmark's history: the anchor compounded
    /// midnight by midnight on the benchmark's daily accrual to the midnight that starts `t`'s day, forward or
    /// backward, then accrued simply to `t`. Every program that holds the two accounts reads the same number for
    /// the same instant, so no CPI is needed to value receipts at `now` or at a past midnight. A backward step
    /// is the floored inverse of the forward one, so a reading behind the anchor can differ from the value the
    /// venue held at the time by a unit of 1e-18 per day between them.
    pub fn reading_at(&self, b: &Benchmark, t: i64) -> Result<Reading> {
        self.reading_within(b, t, MAX_WALK_DAYS)
    }

    /// `reading_at` with the backward walk bounded at `back_days` exact midnights instead of `MAX_WALK_DAYS`.
    /// A settlement, which is its own instruction with its own budget, walks back as far as the fixings ring
    /// reaches (`brink_index::FIXING_DAYS`), so a venue market settled late is still read exactly at its
    /// maturity midnight; readers inside other instructions keep the smaller bound.
    pub fn reading_within(&self, b: &Benchmark, t: i64, back_days: u32) -> Result<Reading> {
        require!(self.anchor_day != 0, VenueError::NotAnchored);
        let target = Benchmark::day_of(t)?;
        require!(
            target <= self.anchor_day.saturating_add(MAX_WALK_DAYS),
            VenueError::NeedsTouch
        );
        let mut day = self.anchor_day;
        let mut index = self.anchor_index_e18;
        let mut accrual = self.anchor_accrual;
        // Further back than the bound, the days beyond it are taken in one flat step at the benchmark's own
        // reading for the target midnight: those midnights are older than the fixings ring holds, so the
        // benchmark itself extrapolates them flat, and the bound keeps the read inside one budget.
        let floor_day = self.anchor_day.saturating_sub(back_days);
        if target < floor_day {
            while day > floor_day {
                let prev_day = day.checked_sub(1).ok_or(VenueError::Overflow)?;
                let prev = brink_index::accrual_at(b, Benchmark::day_start(prev_day)?)?;
                index = Self::shrink(index, accrual.saturating_sub(prev))?;
                accrual = prev;
                day = prev_day;
            }
            let at_target = brink_index::accrual_at(b, Benchmark::day_start(target)?)?;
            index = Self::shrink(index, accrual.saturating_sub(at_target))?;
            accrual = at_target;
            day = target;
        }
        while day < target {
            let next_day = day.checked_add(1).ok_or(VenueError::Overflow)?;
            let next = brink_index::accrual_at(b, Benchmark::day_start(next_day)?)?;
            index = Self::grow(index, next.saturating_sub(accrual))?;
            accrual = next;
            day = next_day;
        }
        while day > target {
            let prev_day = day.checked_sub(1).ok_or(VenueError::Overflow)?;
            let prev = brink_index::accrual_at(b, Benchmark::day_start(prev_day)?)?;
            index = Self::shrink(index, accrual.saturating_sub(prev))?;
            accrual = prev;
            day = prev_day;
        }
        let at_t = brink_index::accrual_at(b, t)?;
        Ok(Reading {
            index_e18: Self::grow(index, at_t.saturating_sub(accrual))?,
            day,
            day_index_e18: index,
            day_accrual: accrual,
        })
    }

    /// The index at `t`; see `reading_at`. A version 1 account not yet anchored has no midnight definition to
    /// read from and answers `NotAnchored` until its first touch; the previous build's continuous formula is
    /// not used for a reading, since the two definitions differ by the intra-day step.
    pub fn index_at(&self, b: &Benchmark, t: i64) -> Result<u128> {
        Ok(self.reading_at(b, t)?.index_e18)
    }

    /// `index_at` with the backward walk bounded at `back_days`; see `reading_within`.
    pub fn index_within(&self, b: &Benchmark, t: i64, back_days: u32) -> Result<u128> {
        Ok(self.reading_within(b, t, back_days)?.index_e18)
    }

    /// Brings the index to `now` and moves the anchor to the midnight that starts today. A version 1 account is
    /// anchored first: its stored index is taken back to the midnight of its last touch by the inverse of the
    /// step the previous build applied since then. After a gap of more than `MAX_WALK_DAYS` the anchor advances
    /// by that many midnights and `Ok(false)` says the index is not yet at `now`: `touch` accepts the partial
    /// step and is called again, every other instruction requires `true`.
    pub fn accrue(&mut self, benchmark_key: Pubkey, b: &Benchmark, now: i64) -> Result<bool> {
        require!(benchmark_key == self.benchmark, VenueError::Benchmark);
        if self.anchor_day == 0 {
            let day = Benchmark::day_of(self.last_ts)?;
            let at_midnight = brink_index::accrual_at(b, Benchmark::day_start(day)?)?;
            let at_last = brink_index::accrual_at(b, self.last_ts)?;
            self.anchor_index_e18 =
                Self::shrink(self.index_e18, at_last.saturating_sub(at_midnight))?;
            self.anchor_day = day;
            self.anchor_accrual = at_midnight;
            self.version = VENUE_VERSION;
        }
        if now < self.last_ts {
            // A clock that ran backwards changes nothing rather than failing.
            return Ok(true);
        }
        let today = Benchmark::day_of(now)?;
        let stop = self.anchor_day.saturating_add(MAX_WALK_DAYS);
        let (at, complete) = if today > stop {
            (Benchmark::day_start(stop)?, false)
        } else {
            (now, true)
        };
        let r = self.reading_at(b, at)?;
        if r.day >= self.anchor_day {
            self.anchor_day = r.day;
            self.anchor_index_e18 = r.day_index_e18;
            self.anchor_accrual = r.day_accrual;
        }
        // The index never moves backwards: a reconstruction that rounds a unit under the stored value is held.
        self.index_e18 = r.index_e18.max(self.index_e18);
        self.bench_accrual_e18 = b.accrual_e18;
        self.tail_bps = tail_bps(b, at)?;
        self.last_ts = at;
        Ok(complete)
    }

    /// What `receipts` worth `value` are paid when the reserve holds `reserve`: the value scaled by the fraction
    /// of all outstanding claims the reserve covers, `value x min(reserve, claims) / claims`, floored. The ratio of
    /// reserve to claims is unchanged by a redemption, so every holder is paid the same funded fraction of their
    /// yield whatever the order, and because the reserve is never below deposited principal the payment is never
    /// below the receipts' share of principal (external scan 2, finding 1).
    pub fn payout(&self, receipts: u64, reserve: u64, value: u64) -> Result<u64> {
        if receipts == 0 {
            return Ok(0);
        }
        funded_value(value, amount_for(self.receipts, self.index_e18)?, reserve)
    }

    /// Whether the reserve covers every outstanding claim at `index`, so a redemption is paid in full.
    pub fn fully_funded(receipts_supply: u64, index_e18: u128, reserve: u64) -> Result<bool> {
        Ok(reserve >= amount_for(receipts_supply, index_e18)?)
    }
}

#[event_cpi]
#[derive(Accounts)]
pub struct CreateVenue<'info> {
    #[account(mut)]
    pub authority: Signer<'info>,
    /// This program's own `ProgramData`; binds `authority` to the upgrade authority.
    #[account(
        address = bpf_loader_upgradeable::get_program_data_address(&crate::ID) @ VenueError::NotUpgradeAuthority,
        constraint = program_data.upgrade_authority_address == Some(authority.key()) @ VenueError::NotUpgradeAuthority,
    )]
    pub program_data: Account<'info, ProgramData>,
    pub benchmark: Box<Account<'info, Benchmark>>,
    pub usdc_mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(init, payer = authority, space = 8 + Venue::INIT_SPACE, seeds = [Venue::SEED, benchmark.key().as_ref()], bump)]
    pub venue: Box<Account<'info, Venue>>,
    #[account(init, payer = authority, seeds = [b"receipt", venue.key().as_ref()], bump, mint::decimals = RECEIPT_DECIMALS, mint::authority = venue, mint::token_program = token_program)]
    pub receipt_mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(init, payer = authority, seeds = [b"reserve", venue.key().as_ref()], bump, token::mint = usdc_mint, token::authority = venue, token::token_program = token_program)]
    pub reserve: Box<InterfaceAccount<'info, TokenAccount>>,
    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

#[event_cpi]
#[derive(Accounts)]
pub struct Place<'info> {
    #[account(mut, seeds = [Venue::SEED, venue.benchmark.as_ref()], bump = venue.bump, has_one = benchmark, has_one = usdc_mint, has_one = reserve, has_one = receipt_mint)]
    pub venue: Box<Account<'info, Venue>>,
    pub benchmark: Box<Account<'info, Benchmark>>,
    pub owner: Signer<'info>,
    #[account(mut, constraint = owner_usdc.owner == owner.key() @ VenueError::TokenOwner, constraint = owner_usdc.mint == usdc_mint.key() @ VenueError::Mint)]
    pub owner_usdc: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, constraint = owner_receipts.owner == owner.key() @ VenueError::TokenOwner, constraint = owner_receipts.mint == receipt_mint.key() @ VenueError::Mint)]
    pub owner_receipts: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut)]
    pub reserve: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut)]
    pub receipt_mint: Box<InterfaceAccount<'info, Mint>>,
    pub usdc_mint: Box<InterfaceAccount<'info, Mint>>,
    pub token_program: Interface<'info, TokenInterface>,
}

#[event_cpi]
#[derive(Accounts)]
pub struct Redeem<'info> {
    #[account(mut, seeds = [Venue::SEED, venue.benchmark.as_ref()], bump = venue.bump, has_one = benchmark, has_one = usdc_mint, has_one = reserve, has_one = receipt_mint)]
    pub venue: Box<Account<'info, Venue>>,
    pub benchmark: Box<Account<'info, Benchmark>>,
    pub owner: Signer<'info>,
    #[account(mut, constraint = owner_usdc.mint == usdc_mint.key() @ VenueError::Mint)]
    pub owner_usdc: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, constraint = owner_receipts.owner == owner.key() @ VenueError::TokenOwner, constraint = owner_receipts.mint == receipt_mint.key() @ VenueError::Mint)]
    pub owner_receipts: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut)]
    pub reserve: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut)]
    pub receipt_mint: Box<InterfaceAccount<'info, Mint>>,
    pub usdc_mint: Box<InterfaceAccount<'info, Mint>>,
    pub token_program: Interface<'info, TokenInterface>,
}

#[event_cpi]
#[derive(Accounts)]
pub struct Fund<'info> {
    #[account(mut, seeds = [Venue::SEED, venue.benchmark.as_ref()], bump = venue.bump, has_one = usdc_mint, has_one = reserve)]
    pub venue: Box<Account<'info, Venue>>,
    pub funder: Signer<'info>,
    #[account(mut, constraint = funder_usdc.owner == funder.key() @ VenueError::TokenOwner, constraint = funder_usdc.mint == usdc_mint.key() @ VenueError::Mint)]
    pub funder_usdc: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut)]
    pub reserve: Box<InterfaceAccount<'info, TokenAccount>>,
    pub usdc_mint: Box<InterfaceAccount<'info, Mint>>,
    pub token_program: Interface<'info, TokenInterface>,
}

#[event_cpi]
#[derive(Accounts)]
pub struct Touch<'info> {
    #[account(mut, seeds = [Venue::SEED, venue.benchmark.as_ref()], bump = venue.bump, has_one = benchmark)]
    pub venue: Box<Account<'info, Venue>>,
    pub benchmark: Box<Account<'info, Benchmark>>,
}

#[derive(Accounts)]
pub struct SetPaused<'info> {
    #[account(mut, seeds = [Venue::SEED, venue.benchmark.as_ref()], bump = venue.bump, has_one = authority @ VenueError::NotAuthority)]
    pub venue: Box<Account<'info, Venue>>,
    pub authority: Signer<'info>,
}

// ---------------------------------------------------------------- events

#[event]
pub struct Touched {
    pub venue: Pubkey,
    pub index_e18: u128,
    pub anchor_day: u32,
    /// False when the walk bound stopped the anchor short of today; touch again.
    pub complete: bool,
}
#[event]
pub struct VenueCreated {
    pub venue: Pubkey,
    pub benchmark: Pubkey,
    pub receipt_mint: Pubkey,
    pub reserve: Pubkey,
}
#[event]
pub struct Placed {
    pub venue: Pubkey,
    pub owner: Pubkey,
    pub amount: u64,
    pub receipts: u64,
    pub index_e18: u128,
}
#[event]
pub struct Redeemed {
    pub venue: Pubkey,
    pub owner: Pubkey,
    pub receipts: u64,
    /// USDC paid.
    pub amount: u64,
    pub index_e18: u128,
    /// The receipts' value at the index; `amount` is below it by `shortfall` when the reserve is short of the
    /// yield it owes.
    pub value: u64,
    pub shortfall: u64,
}
#[event]
pub struct Funded {
    pub venue: Pubkey,
    pub funder: Pubkey,
    pub amount: u64,
}

// ---------------------------------------------------------------- errors

#[error_code]
pub enum VenueError {
    #[msg("Only the venue's authority may do this")]
    NotAuthority,
    #[msg("Only the program's upgrade authority may create a venue")]
    NotUpgradeAuthority,
    #[msg("Amount must be positive")]
    Zero,
    #[msg("Output below the caller's minimum")]
    BelowMinimum,
    #[msg("The reserve cannot cover this redemption; fund it")]
    /// Retained for the error-code table; a short reserve now pays pro rata (external scan 2, finding 1).
    ReserveShort,
    #[msg("Placements are paused")]
    Paused,
    #[msg("Arithmetic overflow")]
    Overflow,
    #[msg("Token account owner mismatch")]
    TokenOwner,
    #[msg("Token mint mismatch")]
    Mint,
    #[msg("Benchmark does not match the venue")]
    Benchmark,
    #[msg("Venue has no midnight anchor yet; touch it first")]
    NotAnchored,
    #[msg("The index is more than the walk bound behind now; touch the venue first")]
    NeedsTouch,
    #[msg("The reserve does not fund every claim; fund it before placing")]
    Underfunded,
}

#[cfg(test)]
mod tests {
    use super::*;
    use brink_index::{ACCRUAL_SCALE, SECONDS_PER_DAY};

    const DAY: i64 = SECONDS_PER_DAY;

    /// The index `days` midnights after `t0` on a venue created at `t0`, accrued one midnight at a time.
    fn direct_reading(b: &Benchmark, t0: i64, days: i64) -> u128 {
        let mut v = venue(b, t0);
        for d in 1..=days {
            assert!(v.accrue(Pubkey::default(), b, t0 + d * DAY).unwrap());
        }
        v.index_e18
    }

    fn benchmark() -> Benchmark {
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
            version: brink_index::LAYOUT_VERSION,
            max_drift_bp: 400,
            drift_window_slots: 216_000,
            drift_anchor_bp: 0,
            drift_anchor_slot: 0,
            clamped: false,
            disputed: false,
            support: 1,
            observations: [brink_index::Observation::default(); brink_index::MAX_PUBLISHERS],
            ema_milli_bp: 0,
            fixing_first_day: 0,
            fixing_head_day: 0,
            fixings: Box::new([0; brink_index::FIXING_BYTES]),
            ema_slot: 0,
            accepted_slot: 0,
            support_lost: false,
            _reserved: [0; 11],
        }
    }
    /// The accrual part of `brink_index::publish`, for host tests.
    fn publish(b: &mut Benchmark, now: i64, value_bp: u16) {
        if b.published {
            b.record_fixings(now).unwrap();
            b.accrual_e18 +=
                u128::from(b.value_bp) * u128::try_from(now - b.unix_ts).unwrap() * ACCRUAL_SCALE;
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
    fn venue(b: &Benchmark, created: i64) -> Venue {
        let day = Benchmark::day_of(created).unwrap();
        let mut v = Venue {
            version: VENUE_VERSION,
            authority: Pubkey::default(),
            benchmark: Pubkey::default(),
            usdc_mint: Pubkey::default(),
            reserve: Pubkey::default(),
            receipt_mint: Pubkey::default(),
            index_e18: INDEX_SCALE,
            bench_accrual_e18: b.accrual_e18,
            tail_bps: 0,
            last_ts: created,
            principal: 0,
            receipts: 0,
            funded: 0,
            paused: false,
            bump: 0,
            anchor_index_e18: INDEX_SCALE,
            anchor_day: day,
            anchor_accrual: brink_index::accrual_at(b, Benchmark::day_start(day).unwrap()).unwrap(),
            reserved: [0; 28],
        };
        v.index_e18 = v.index_at(b, created).unwrap();
        v
    }
    /// `1 + bp x secs / year` applied to `index`, exactly as a reader would compute one step.
    fn step(index: u128, bp: u128, secs: u128) -> u128 {
        index + index * bp * secs / BP_SECONDS_PER_YEAR
    }

    #[test]
    fn the_index_compounds_daily_on_the_fixings_and_accrues_simply_inside_the_day() {
        let mut b = benchmark();
        let t0 = 20_000 * DAY; // a midnight
        publish(&mut b, t0, 500);
        let v = venue(&b, t0 + 3_600);
        // Created at 01:00: par at the midnight, one hour of 500 bp since.
        assert_eq!(v.anchor_index_e18, INDEX_SCALE);
        assert_eq!(v.index_e18, step(INDEX_SCALE, 500, 3_600));
        // Ten days at 500 bp, then the rate doubles for ten more, published on the way.
        publish(&mut b, t0 + 10 * DAY, 1_000);
        publish(&mut b, t0 + 20 * DAY, 1_000);
        let mut expect = INDEX_SCALE;
        for d in 0..20 {
            let bp = if d < 10 { 500 } else { 1_000 };
            expect = step(expect, bp, 86_400);
            let at = v.index_at(&b, t0 + (d + 1) * DAY).unwrap();
            assert!(
                at.abs_diff(expect) <= u128::try_from(d + 1).unwrap(),
                "day {d}: {at} vs {expect}"
            );
        }
        // Daily compounding is above simple accrual over the same twenty days and below continuous.
        let simple = step(INDEX_SCALE, 750, 20 * 86_400);
        let at = v.index_at(&b, t0 + 20 * DAY).unwrap();
        assert!(at > simple);
        assert!(at < simple + simple / 100_000);
        // Inside a day the accrual is simple from that day's midnight.
        let mid = v.index_at(&b, t0 + 15 * DAY).unwrap();
        let noon = v.index_at(&b, t0 + 15 * DAY + 43_200).unwrap();
        assert_eq!(noon, step(mid, 1_000, 43_200));
    }

    #[test]
    fn readings_do_not_depend_on_when_the_venue_was_touched() {
        let mut b = benchmark();
        let t0 = 20_000 * DAY;
        publish(&mut b, t0, 684);
        let mut touched = venue(&b, t0);
        let untouched = venue(&b, t0);
        for d in 1..=40 {
            publish(
                &mut b,
                t0 + d * DAY + 7_200 * (d % 5),
                684 + u16::try_from(d).unwrap() * 3,
            );
            if d % 3 == 0 {
                touched
                    .accrue(Pubkey::default(), &b, t0 + d * DAY + 7_200 * (d % 5))
                    .unwrap();
            }
        }
        // The touched venue's anchor moved to the last touch's day; its readings agree with the untouched one
        // at every midnight, in both directions, to a unit per day of reconstruction.
        assert_eq!(
            touched.anchor_day,
            Benchmark::day_of(t0 + 39 * DAY).unwrap()
        );
        for d in 0..=40 {
            let t = t0 + d * DAY;
            let a = touched.index_at(&b, t).unwrap();
            let c = untouched.index_at(&b, t).unwrap();
            assert!(a.abs_diff(c) <= 40, "day {d}: {a} vs {c}");
        }
        // Inside a day, too, and ahead of the last publication (the live value held flat).
        let t = t0 + 40 * DAY + 50_000;
        let a = touched.index_at(&b, t).unwrap();
        let c = untouched.index_at(&b, t).unwrap();
        assert!(a.abs_diff(c) <= 40);
        assert!(a > touched.index_e18);
        // Accruing to that instant stores exactly the reading.
        touched.accrue(Pubkey::default(), &b, t).unwrap();
        assert_eq!(touched.index_e18, a);
        assert_eq!(touched.last_ts, t);
        // The stored index never moves backwards.
        touched.accrue(Pubkey::default(), &b, t - DAY).unwrap();
        assert_eq!(touched.index_e18, a);
    }

    #[test]
    fn a_version_one_account_is_anchored_from_its_stored_index() {
        let mut b = benchmark();
        let t0 = 20_000 * DAY;
        publish(&mut b, t0, 684);
        publish(&mut b, t0 + 30 * DAY, 684);
        // The previous build held the index at the last touch; the anchor is placed at that day's midnight.
        let last = t0 + 20 * DAY + 30_000;
        let old_index = step(INDEX_SCALE, 684, 20 * 86_400 + 30_000);
        let mut v = venue(&b, t0);
        v.version = 1;
        v.anchor_day = 0;
        v.anchor_index_e18 = 0;
        v.anchor_accrual = 0;
        v.index_e18 = old_index;
        v.last_ts = last;
        // Before anchoring there is no midnight definition to read from; readers are told to touch first.
        let later = last + 10_000;
        assert!(matches!(
            v.index_at(&b, later),
            Err(e) if e == VenueError::NotAnchored.into()
        ));
        v.accrue(Pubkey::default(), &b, later).unwrap();
        assert_eq!(v.version, VENUE_VERSION);
        assert_eq!(v.anchor_day, Benchmark::day_of(last).unwrap());
        // The anchor reproduces the old index at the old touch to a unit; the new index is above the old one and
        // within 1e-8 of the old formula's continuation (the intra-day step now runs from the midnight value).
        let back = v.index_at(&b, last).unwrap();
        assert!(back.abs_diff(old_index) <= 1, "{back} vs {old_index}");
        let continued = step(old_index, 684, 10_000);
        assert!(v.index_e18 > old_index);
        assert!(v.index_e18.abs_diff(continued) * 100_000_000 < continued);
    }

    #[test]
    fn a_short_reserve_pays_every_holder_the_same_funded_fraction_and_never_touches_principal() {
        let mut b = benchmark();
        let t0 = 20_000 * DAY;
        publish(&mut b, t0, 1_000);
        let mut v = venue(&b, t0);
        // Two holders place 1 000 at par; a year passes at 1 000 bp; the reserve holds the principal and only
        // half the yield.
        v.principal = 2_000_000_000;
        v.receipts = 2_000_000_000;
        publish(&mut b, t0 + 365 * DAY, 1_000);
        // A year is more than the walk bound: each touch advances the anchor by the bound and reports the index
        // as not yet current until the last one completes it.
        assert!(!v.accrue(Pubkey::default(), &b, t0 + 365 * DAY).unwrap());
        assert_eq!(v.anchor_day, Benchmark::day_of(t0).unwrap() + MAX_WALK_DAYS);
        assert_eq!(v.last_ts, Benchmark::day_start(v.anchor_day).unwrap());
        assert!(
            v.index_at(&b, t0 + 365 * DAY).is_err(),
            "readers are told to touch first"
        );
        let mut touches = 1;
        loop {
            touches += 1;
            if v.accrue(Pubkey::default(), &b, t0 + 365 * DAY).unwrap() {
                break;
            }
        }
        assert_eq!(touches, 365 / i64::from(MAX_WALK_DAYS) + 1);
        assert_eq!(v.last_ts, t0 + 365 * DAY);
        // The chunked walk lands on the same index as the midnight-by-midnight definition.
        let mut direct = venue(&b, t0);
        direct.principal = v.principal;
        direct.receipts = v.receipts;
        let mut t = t0;
        while t < t0 + 365 * DAY {
            t += 50 * DAY;
            assert!(direct
                .accrue(Pubkey::default(), &b, t.min(t0 + 365 * DAY))
                .unwrap());
        }
        assert_eq!(direct.index_e18, v.index_e18);
        // A reading further back than the bound takes one flat step beyond it: a flat step removes less than
        // compounding would, so it sits above the midnight-by-midnight path by the second-order term, here
        // 205 days at 1 000 bp, (0.056)^2 / 2, under a fifth of a percent. Those midnights are older than the
        // fixings ring holds, so the benchmark could not have reconstructed them either.
        let far = v.index_at(&b, t0 + 100 * DAY).unwrap();
        let near = direct_reading(&b, t0, 100);
        assert!(
            far >= near && far.abs_diff(near) * 500 < near,
            "{far} vs {near}"
        );
        let claims = amount_for(v.receipts, v.index_e18).unwrap();
        let yield_total = claims - v.principal;
        assert!(
            yield_total > 200_000_000 && yield_total < 212_000_000,
            "{yield_total}"
        );
        let reserve = v.principal + yield_total / 2;
        // First holder redeems everything: half their yield, all their principal.
        let value = amount_for(1_000_000_000, v.index_e18).unwrap();
        let paid = v.payout(1_000_000_000, reserve, value).unwrap();
        assert!(paid >= 1_000_000_000);
        let got_yield = paid - 1_000_000_000;
        assert!(
            got_yield.abs_diff(yield_total / 4) <= 1,
            "{got_yield} vs {}",
            yield_total / 4
        );
        // The books after the first redemption; the second holder is paid the same fraction.
        let reserve_after = reserve - paid;
        v.principal -= 1_000_000_000;
        v.receipts -= 1_000_000_000;
        let paid2 = v.payout(1_000_000_000, reserve_after, value).unwrap();
        assert!(paid2.abs_diff(paid) <= 2, "{paid2} vs {paid}");
        assert!(paid2 >= 1_000_000_000);
        assert!(reserve_after >= v.principal);
        // Fully funded pays the value; an empty venue pays nothing.
        assert_eq!(v.payout(1_000_000_000, claims, value).unwrap(), value);
        assert!(Venue::fully_funded(v.receipts, v.index_e18, value).unwrap());
        assert!(!Venue::fully_funded(v.receipts, v.index_e18, value - 1).unwrap());
        v.receipts = 0;
        assert_eq!(v.payout(1, reserve_after, 1).unwrap(), 0);
    }
}
