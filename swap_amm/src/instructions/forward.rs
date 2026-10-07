//! Forward-starting swaps: a fixed rate agreed now for a swap whose floating accrual begins
//! at a later UTC midnight, 28, 60 or 90 days ahead, with start plus tenor inside the 180-day curve. The leg is
//! an ordinary `Swap` opened by the same `open_leg` the single-swap instruction runs, with `Pricing::Forward`:
//! collateral and the opening fee are paid at open, the notional counts against the pool's caps and imbalance
//! from open, and the pool book carries the forward's own terms (`BookTerms::for_forward`) until the start
//! crank replaces them with the ordinary ones. Pricing is the Vernier forward curve (`vernier::quote_forward`):
//! the model term becomes the implied forward of the pool's per-tenor model spreads; reference, demand and
//! tenor terms are the spot swap's.
//!
//! `crank_start_forward` is permissionless. Once the chain clock has passed `start_ts` it records the benchmark's
//! cumulative accrual at the start from the daily fixings ring (exact: a start is at most 90 days back and the
//! ring holds 128), swaps the book terms and sets `FORWARD_STARTED`. The close paths (`cancel`, `settle`,
//! `liquidate`) take the same reading themselves if they meet an unstarted forward, so a missed crank never
//! blocks a close.
use super::swap::{
    accrual_lookup, emit_event, open_leg, OpenLeg, OpenShared, OpenSwapArgs, Pricing,
};
use crate::{errors::BrinkError, state::*};
use anchor_lang::prelude::*;
use anchor_spl::token_interface::{Mint, TokenAccount, TokenInterface};
use brink_index::Benchmark;

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy)]
pub struct OpenForwardSwapArgs {
    pub leg: LegKind,
    /// Index into `vernier::START_DAYS` (0: 28 days, 1: 60 days, 2: 90 days).
    pub start: u8,
    pub tenor: u8,
    pub notional: u64,
    pub limit_rate_bp: u16,
    pub client_seed: u64,
}

#[event_cpi]
#[derive(Accounts)]
#[instruction(args: OpenForwardSwapArgs)]
pub struct TraderOpenForwardSwap<'info> {
    /// Read-only: fees accrue on the pool (review F-29, ADR-001).
    #[account(seeds = [b"global"], bump = global.bump, has_one = usdc_mint, has_one = token_program)]
    pub global: Box<Account<'info, Global>>,
    #[account(mut, seeds = [b"pool", pool.benchmark.as_ref()], bump = pool.bump, has_one = benchmark, has_one = vault)]
    pub pool: Box<Account<'info, Pool>>,
    /// Owner check is implicit: `Account<Benchmark>` requires the index program as owner.
    pub benchmark: Box<Account<'info, Benchmark>>,
    #[account(init, payer = trader, space = 8 + Swap::INIT_SPACE, seeds = [b"swap", pool.key().as_ref(), trader.key().as_ref(), &args.client_seed.to_le_bytes()], bump)]
    pub swap: Box<Account<'info, Swap>>,
    #[account(mut)]
    pub trader: Signer<'info>,
    #[account(mut, constraint = trader_usdc.owner == trader.key() @ BrinkError::TokenOwner, constraint = trader_usdc.mint == usdc_mint.key() @ BrinkError::SettlementMint)]
    pub trader_usdc: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut)]
    pub vault: Box<InterfaceAccount<'info, TokenAccount>>,
    pub usdc_mint: Box<InterfaceAccount<'info, Mint>>,
    /// CHECK: validated against pool.hook_program in `hooks::call`.
    pub hook_program: Option<UncheckedAccount<'info>>,
    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

pub fn open_forward_swap<'info>(
    ctx: Context<'info, TraderOpenForwardSwap<'info>>,
    a: OpenForwardSwapArgs,
) -> Result<()> {
    let clock = Clock::get()?;
    let swap_bump = ctx.bumps.swap;
    let start_days = vernier::START_DAYS
        .get(usize::from(a.start))
        .copied()
        .ok_or(BrinkError::ForwardHorizon)?;
    let x = &mut *ctx.accounts;
    let sh = OpenShared {
        global: &x.global,
        trader: &x.trader,
        trader_usdc: &x.trader_usdc,
        usdc_mint: &x.usdc_mint,
        token_program: &x.token_program,
        event_authority: x.event_authority.to_account_info(),
    };
    let mut leg = OpenLeg {
        pool: &mut x.pool,
        benchmark: &x.benchmark,
        swap: &mut x.swap,
        swap_bump,
        vault: &mut x.vault,
        hook_program: x.hook_program.as_ref(),
        hook_accounts: ctx.remaining_accounts,
    };
    let args = OpenSwapArgs {
        leg: a.leg,
        tenor: a.tenor,
        notional: a.notional,
        limit_rate_bp: a.limit_rate_bp,
        client_seed: a.client_seed,
    };
    open_leg(
        &sh,
        &mut leg,
        &args,
        &clock,
        Pricing::Forward(start_days),
        None,
    )?;
    leg.vault.reload()?;
    leg.pool.assert_invariants(leg.vault.amount)
}

#[event_cpi]
#[derive(Accounts)]
pub struct CrankStartForward<'info> {
    #[account(mut, seeds = [b"pool", pool.benchmark.as_ref()], bump = pool.bump, has_one = benchmark)]
    pub pool: Box<Account<'info, Pool>>,
    pub benchmark: Box<Account<'info, Benchmark>>,
    #[account(mut, has_one = pool, constraint = swap.state == SwapState::Open @ BrinkError::AlreadySettled)]
    pub swap: Box<Account<'info, Swap>>,
}

/// Takes a forward's start reading once its start has passed and moves it onto the ordinary book terms.
pub fn start_forward(ctx: Context<CrankStartForward>) -> Result<()> {
    let clock = Clock::get()?;
    let x = &mut *ctx.accounts;
    let pool = &mut *x.pool;
    let s = &mut *x.swap;
    require!(s.is_forward(), BrinkError::NotForward);
    require!(!s.is_started(), BrinkError::ForwardAlreadyStarted);
    require!(
        clock.unix_timestamp >= s.start_ts,
        BrinkError::ForwardNotDue
    );
    let b = &*x.benchmark;
    require!(b.published, BrinkError::BenchmarkNotPublished);
    // The start is a UTC midnight, so while it is within the fixings ring the lookup answers exactly. Once it
    // has left the ring the reading would be a backward extrapolation from the latest segment, which the rate
    // published most recently could steer, so the crank refuses to store it (external scan 2, finding 21); the
    // settlement path then values the forward from the part of its own term still on chain (`settle_leg`).
    let (accrual_start, kind) = accrual_lookup(b, s.start_ts)?;
    require!(
        kind != brink_index::FixingKind::Fallback,
        BrinkError::StartUnavailable
    );
    pool.book_sub(&s.book_terms()?)?;
    s.index_accrual_start = accrual_start;
    s.link_flags |= FORWARD_STARTED;
    pool.book_add(&s.book_terms()?)?;
    pool.event_seq = pool.event_seq.checked_add(1).ok_or(BrinkError::Sequence)?;
    emit_event(
        &x.event_authority.to_account_info(),
        &ForwardStarted {
            pool: pool.key(),
            swap: s.key(),
            start_ts: s.start_ts,
            accrual_start,
            fixing_kind: kind as u8,
            seq: pool.event_seq,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn swap(leg: LegKind, flags: u8, start_ts: i64, opened_ts: i64, matures_ts: i64) -> Swap {
        Swap {
            pool: Pubkey::new_unique(),
            trader: Pubkey::new_unique(),
            leg,
            tenor: 1,
            notional: 1_000_000_000,
            fixed_bp: 700,
            collateral: 23_000_000,
            opened_slot: 0,
            opened_ts,
            matures_ts,
            index_accrual_start: 0,
            client_seed: 0,
            state: SwapState::Open,
            bump: 0,
            limited_window_start: 0,
            link: Pubkey::default(),
            link_flags: flags,
            start_ts,
            _reserved: [0; 15],
        }
    }

    /// The forward flag gates everything: a legacy account (zero reserve) is a spot swap that accrues from open.
    #[test]
    fn flags_and_accrual_from() {
        let spot = swap(LegKind::PayFixed, 0, 0, 1_000, 6_184_000);
        assert!(!spot.is_forward() && spot.is_started());
        assert_eq!(spot.accrual_from(), 1_000);
        let fwd = swap(LegKind::PayFixed, LINK_FORWARD, 2_505_600, 1_000, 7_689_600);
        assert!(fwd.is_forward() && !fwd.is_started());
        assert_eq!(fwd.accrual_from(), 2_505_600);
        let started = swap(
            LegKind::PayFixed,
            LINK_FORWARD | FORWARD_STARTED,
            2_505_600,
            1_000,
            7_689_600,
        );
        assert!(started.is_forward() && started.is_started());
        // A started flag without the forward flag is meaningless and reads as a spot swap.
        let stray = swap(
            LegKind::PayFixed,
            FORWARD_STARTED,
            2_505_600,
            1_000,
            7_689_600,
        );
        assert!(!stray.is_forward() && stray.is_started() && stray.accrual_from() == 1_000);
    }

    /// Before its start a forward's book terms value to the forward mark, `(value − fixed) · notional · term`,
    /// with no accrued leg; after the start crank the ordinary terms take over; a close returns the book to zero.
    #[test]
    fn forward_book_terms_mark_the_forward_and_round_trip() {
        use crate::instructions::math::BP_SECONDS_PER_YEAR;
        let start = 2_505_600_i64; // a UTC midnight
        let matures = start + 60 * 86_400;
        let mut s = swap(LegKind::PayFixed, LINK_FORWARD, start, 1_000, matures);
        let mut p = crate::instructions::swap::test_pool();
        p.book_add(&s.book_terms().unwrap()).unwrap();
        let now = 1_500_i64;
        let a_now: u128 = 123_456 * brink_index::ACCRUAL_SCALE; // irrelevant before start: no accrued leg
        let value_bp = 750_u16;
        let got = p.book_value(a_now, value_bp, now).unwrap();
        let term = i128::from(matures - start);
        let want = (i128::from(value_bp) - 700) * i128::from(s.notional) * term
            / i128::try_from(BP_SECONDS_PER_YEAR).unwrap();
        assert_eq!(got, want);
        // Start: the forward terms are swapped for the ordinary ones with the start accrual recorded.
        let a_start: u128 = 50_000 * brink_index::ACCRUAL_SCALE;
        p.book_sub(&s.book_terms().unwrap()).unwrap();
        s.index_accrual_start = a_start;
        s.link_flags |= FORWARD_STARTED;
        p.book_add(&s.book_terms().unwrap()).unwrap();
        let ordinary =
            BookTerms::for_swap(LegKind::PayFixed, s.notional, a_start, 700, start, matures)
                .unwrap();
        assert_eq!(p.book_pay.notional, ordinary.notional);
        assert_eq!(p.book_pay.accrual_start, ordinary.accrual_start);
        assert_eq!(p.book_pay.maturity_weight, ordinary.maturity_weight);
        assert_eq!(p.book_pay.fixed_leg, ordinary.fixed_leg);
        // Close returns every aggregate to zero.
        p.book_sub(&s.book_terms().unwrap()).unwrap();
        assert_eq!(p.book_pay, BookSide::default());
        assert_eq!(p.book_value(a_now, value_bp, now).unwrap(), 0);
    }
}
