//! Basis swaps: one trader pays fixed on benchmark A and receives fixed on benchmark B over
//! the same tenor and notional, in one instruction. Each leg is an ordinary `Swap` in its own pool, opened by the
//! same `open_leg` the single-swap instruction runs, with USDC collateral posted to each pool independently, so
//! every LP protection audited in phase 1 (collateral floors, caps, Limited-mode budget, queue priority, the
//! matured gate, hooks, per-leg book clamps, cap-out) applies to each leg unchanged. The two legs are linked by
//! key (`Swap::link`) so the trader closes them together with one floor on the net payout; settlement and
//! liquidation stay per leg through the permissionless cranks.
//!
//! Pricing: each leg is quoted against its own pool exactly as a single swap would be, then the pair's
//! governance-set correlation offset (`BasisPair::correlation_bp`) removes the same fraction of each leg's
//! demand component (`vernier::quote_basis`). Model and tenor components are untouched. Absent the pair account,
//! basis swaps are not enabled for that ordered pair.
use super::swap::{
    cancel_leg, emit_event, open_leg, CloseLeg, CloseShared, Link, OpenLeg, OpenShared,
    OpenSwapArgs, Pricing,
};
use crate::{errors::BrinkError, state::*};
use anchor_lang::prelude::*;
use anchor_spl::token_interface::{Mint, TokenAccount, TokenInterface};
use brink_index::Benchmark;

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy)]
pub struct OpenBasisSwapArgs {
    pub tenor: u8,
    pub notional: u64,
    /// Highest fixed rate accepted on the pay-fixed leg (pool A), bp.
    pub limit_pay_bp: u16,
    /// Lowest fixed rate accepted on the receive-fixed leg (pool B), bp.
    pub limit_receive_bp: u16,
    /// Seed of both legs' swap PDAs (one per pool, so the keys differ).
    pub client_seed: u64,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy)]
pub struct SetBasisPairArgs {
    pub correlation_bp: u16,
}

#[event_cpi]
#[derive(Accounts)]
pub struct AdminSetBasisPair<'info> {
    #[account(seeds = [b"global"], bump = global.bump, has_one = authority)]
    pub global: Box<Account<'info, Global>>,
    pub authority: Signer<'info>,
    #[account(mut)]
    pub payer: Signer<'info>,
    #[account(seeds = [b"pool", pool_a.benchmark.as_ref()], bump = pool_a.bump)]
    pub pool_a: Box<Account<'info, Pool>>,
    #[account(seeds = [b"pool", pool_b.benchmark.as_ref()], bump = pool_b.bump)]
    pub pool_b: Box<Account<'info, Pool>>,
    #[account(init, payer = payer, space = 8 + BasisPair::INIT_SPACE, seeds = [b"basis", pool_a.key().as_ref(), pool_b.key().as_ref()], bump)]
    pub pair: Box<Account<'info, BasisPair>>,
    pub system_program: Program<'info, System>,
}

// The governance bound sits inside the library's: the on-chain cap is the tighter one.
const _: () = assert!(MAX_CORRELATION_BP <= vernier::MAX_OFFSET_BP);

/// The correlation offset governance may record for a pair: at most `MAX_CORRELATION_BP`.
pub(crate) fn check_correlation_bound(correlation_bp: u16) -> Result<()> {
    require!(
        correlation_bp <= MAX_CORRELATION_BP,
        BrinkError::CorrelationBound
    );
    Ok(())
}

/// Creates the pair record for pay-fixed on `pool_a` against receive-fixed on `pool_b`, enabling basis swaps
/// for that ordered pair. Authority only (the timelock after bootstrap), like every calibration.
pub fn set_basis_pair(ctx: Context<AdminSetBasisPair>, a: SetBasisPairArgs) -> Result<()> {
    require!(
        ctx.accounts.pool_a.key() != ctx.accounts.pool_b.key(),
        BrinkError::BasisSamePool
    );
    check_correlation_bound(a.correlation_bp)?;
    let pair = &mut ctx.accounts.pair;
    pair.pool_a = ctx.accounts.pool_a.key();
    pair.pool_b = ctx.accounts.pool_b.key();
    pair.correlation_bp = a.correlation_bp;
    pair.bump = ctx.bumps.pair;
    pair._reserved = [0; 32];
    emit_cpi!(BasisPairSet {
        pool_a: pair.pool_a,
        pool_b: pair.pool_b,
        correlation_bp: pair.correlation_bp
    });
    Ok(())
}

#[event_cpi]
#[derive(Accounts)]
pub struct AdminUpdateBasisPair<'info> {
    #[account(seeds = [b"global"], bump = global.bump, has_one = authority)]
    pub global: Box<Account<'info, Global>>,
    pub authority: Signer<'info>,
    #[account(mut, seeds = [b"basis", pair.pool_a.as_ref(), pair.pool_b.as_ref()], bump = pair.bump)]
    pub pair: Box<Account<'info, BasisPair>>,
}

/// Changes an existing pair's offset. The same bound applies; there is no step rule because the offset only
/// ever returns part of a charge the position would otherwise pay in full.
pub fn update_basis_pair(ctx: Context<AdminUpdateBasisPair>, a: SetBasisPairArgs) -> Result<()> {
    check_correlation_bound(a.correlation_bp)?;
    let pair = &mut ctx.accounts.pair;
    pair.correlation_bp = a.correlation_bp;
    emit_cpi!(BasisPairSet {
        pool_a: pair.pool_a,
        pool_b: pair.pool_b,
        correlation_bp: pair.correlation_bp
    });
    Ok(())
}

#[event_cpi]
#[derive(Accounts)]
#[instruction(args: OpenBasisSwapArgs)]
pub struct TraderOpenBasisSwap<'info> {
    #[account(seeds = [b"global"], bump = global.bump, has_one = usdc_mint, has_one = token_program)]
    pub global: Box<Account<'info, Global>>,
    /// The pair record: its presence enables the pair, its offset prices it. Seeds bind it to this ordering.
    #[account(seeds = [b"basis", pool_a.key().as_ref(), pool_b.key().as_ref()], bump = pair.bump, has_one = pool_a, has_one = pool_b)]
    pub pair: Box<Account<'info, BasisPair>>,
    #[account(mut, seeds = [b"pool", pool_a.benchmark.as_ref()], bump = pool_a.bump, constraint = pool_a.benchmark == benchmark_a.key() @ BrinkError::BenchmarkNotPublished, constraint = pool_a.vault == vault_a.key() @ BrinkError::TokenOwner)]
    pub pool_a: Box<Account<'info, Pool>>,
    pub benchmark_a: Box<Account<'info, Benchmark>>,
    #[account(init, payer = trader, space = 8 + Swap::INIT_SPACE, seeds = [b"swap", pool_a.key().as_ref(), trader.key().as_ref(), &args.client_seed.to_le_bytes()], bump)]
    pub swap_a: Box<Account<'info, Swap>>,
    #[account(mut)]
    pub vault_a: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, seeds = [b"pool", pool_b.benchmark.as_ref()], bump = pool_b.bump, constraint = pool_b.benchmark == benchmark_b.key() @ BrinkError::BenchmarkNotPublished, constraint = pool_b.vault == vault_b.key() @ BrinkError::TokenOwner)]
    pub pool_b: Box<Account<'info, Pool>>,
    pub benchmark_b: Box<Account<'info, Benchmark>>,
    #[account(init, payer = trader, space = 8 + Swap::INIT_SPACE, seeds = [b"swap", pool_b.key().as_ref(), trader.key().as_ref(), &args.client_seed.to_le_bytes()], bump)]
    pub swap_b: Box<Account<'info, Swap>>,
    #[account(mut)]
    pub vault_b: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut)]
    pub trader: Signer<'info>,
    #[account(mut, constraint = trader_usdc.owner == trader.key() @ BrinkError::TokenOwner, constraint = trader_usdc.mint == usdc_mint.key() @ BrinkError::SettlementMint)]
    pub trader_usdc: Box<InterfaceAccount<'info, TokenAccount>>,
    pub usdc_mint: Box<InterfaceAccount<'info, Mint>>,
    /// CHECK: validated against pool_a.hook_program in `hooks::call`.
    pub hook_program_a: Option<UncheckedAccount<'info>>,
    /// CHECK: validated against pool_b.hook_program in `hooks::call`.
    pub hook_program_b: Option<UncheckedAccount<'info>>,
    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

/// The two pools and benchmarks a basis quote reads.
pub(crate) struct BasisLegs<'a> {
    pub pool_a: &'a Pool,
    pub bench_a: &'a Benchmark,
    pub pool_b: &'a Pool,
    pub bench_b: &'a Benchmark,
}

/// Both legs' fixed rates for a basis open, quoted against the two pools' current state with the pair's offset.
/// Pure: reads the pools after their pending calibrations are applied and writes nothing, so a failure on
/// either side (no capital, a stale benchmark, a pricer mismatch) is reported before any leg is booked.
pub(crate) fn quote_basis_legs(
    legs: &BasisLegs<'_>,
    slot: u64,
    tenor: vernier::Tenor,
    notional: u64,
    correlation_bp: u16,
) -> Result<vernier::BasisQuote> {
    let (pool_a, pool_b) = (legs.pool_a, legs.pool_b);
    require!(
        matches!(pool_a.pricer, Pricer::Vernier) && matches!(pool_b.pricer, Pricer::Vernier),
        BrinkError::PricerMismatch
    );
    let (spot_a, ema_a) = super::swap::benchmark_for_quote(legs.bench_a, slot)?;
    let (spot_b, ema_b) = super::swap::benchmark_for_quote(legs.bench_b, slot)?;
    let pa: vernier::Params = pool_a.params.into();
    let pb: vernier::Params = pool_b.params.into();
    let vpa = pool_a.vernier_pool();
    let vpb = pool_b.vernier_pool();
    let a = vernier::BasisSide {
        spot_bp: spot_a,
        ema_bp: ema_a,
        pool: &vpa,
        params: &pa,
    };
    let b = vernier::BasisSide {
        spot_bp: spot_b,
        ema_bp: ema_b,
        pool: &vpb,
        params: &pb,
    };
    vernier::quote_basis(&a, &b, tenor, notional, correlation_bp)
        .map_err(|e| BrinkError::from(e).into())
}

/// Entry checks both legs must pass before either is booked, in `open_leg`'s order: capital, capacity and
/// queue priority. `open_leg` repeats them; running them first for both pools means a basis open that leg B
/// would refuse fails before leg A has moved any money, which is what the transaction's atomicity guarantees
/// on chain and what the host tests check directly.
pub(crate) fn check_basis_entry(
    pool_a: &Pool,
    pool_b: &Pool,
    notional: u64,
    slot: u64,
) -> Result<()> {
    for (pool, leg) in [(pool_a, vernier::Leg::Pay), (pool_b, vernier::Leg::Receive)] {
        require!(pool.tvl > 0, BrinkError::PoolInvariant);
        require!(notional >= pool.min_notional, BrinkError::NotionalTooSmall);
        require!(notional <= pool.max_notional, BrinkError::NotionalTooLarge);
        require!(notional <= pool.leg_capacity(leg), BrinkError::LegCap);
        super::queue::require_queue_priority(pool, leg, notional, slot)?;
    }
    Ok(())
}

/// Opens the two legs. Pool A and pool B must differ (the pair record guarantees it), the benchmarks are each
/// pool's own, the legs share tenor, notional and seed and are linked to each other by key.
pub fn open_basis_swap<'info>(
    ctx: Context<'info, TraderOpenBasisSwap<'info>>,
    a: OpenBasisSwapArgs,
) -> Result<()> {
    let clock = Clock::get()?;
    require!(
        ctx.accounts.pool_a.key() != ctx.accounts.pool_b.key(),
        BrinkError::BasisSamePool
    );
    let tenor = super::math::tenor_from(a.tenor)?;
    // Price both legs on the state both pools will be in when the legs are booked.
    super::swap::apply_pending(&mut ctx.accounts.pool_a, clock.slot);
    super::swap::apply_pending(&mut ctx.accounts.pool_b, clock.slot);
    check_basis_entry(
        &ctx.accounts.pool_a,
        &ctx.accounts.pool_b,
        a.notional,
        clock.slot,
    )?;
    let correlation_bp = ctx.accounts.pair.correlation_bp;
    let q = quote_basis_legs(
        &BasisLegs {
            pool_a: &ctx.accounts.pool_a,
            bench_a: &ctx.accounts.benchmark_a,
            pool_b: &ctx.accounts.pool_b,
            bench_b: &ctx.accounts.benchmark_b,
        },
        clock.slot,
        tenor,
        a.notional,
        correlation_bp,
    )?;

    let swap_a_key = ctx.accounts.swap_a.key();
    let swap_b_key = ctx.accounts.swap_b.key();
    let bump_a = ctx.bumps.swap_a;
    let bump_b = ctx.bumps.swap_b;
    let x = &mut *ctx.accounts;
    let sh = OpenShared {
        global: &x.global,
        trader: &x.trader,
        trader_usdc: &x.trader_usdc,
        usdc_mint: &x.usdc_mint,
        token_program: &x.token_program,
        event_authority: x.event_authority.to_account_info(),
    };
    let args_a = OpenSwapArgs {
        leg: LegKind::PayFixed,
        tenor: a.tenor,
        notional: a.notional,
        limit_rate_bp: a.limit_pay_bp,
        client_seed: a.client_seed,
    };
    let args_b = OpenSwapArgs {
        leg: LegKind::ReceiveFixed,
        tenor: a.tenor,
        notional: a.notional,
        limit_rate_bp: a.limit_receive_bp,
        client_seed: a.client_seed,
    };
    let opened_a = {
        let mut leg = OpenLeg {
            pool: &mut x.pool_a,
            benchmark: &x.benchmark_a,
            swap: &mut x.swap_a,
            swap_bump: bump_a,
            vault: &mut x.vault_a,
            hook_program: x.hook_program_a.as_ref(),
            hook_accounts: ctx.remaining_accounts,
        };
        let o = open_leg(
            &sh,
            &mut leg,
            &args_a,
            &clock,
            Pricing::Given(q.pay.fixed_bp),
            Some(Link {
                other: swap_b_key,
                flags: Swap::link_flags_for(LegKind::PayFixed),
            }),
        )?;
        leg.vault.reload()?;
        leg.pool.assert_invariants(leg.vault.amount)?;
        o
    };
    let opened_b = {
        let mut leg = OpenLeg {
            pool: &mut x.pool_b,
            benchmark: &x.benchmark_b,
            swap: &mut x.swap_b,
            swap_bump: bump_b,
            vault: &mut x.vault_b,
            hook_program: x.hook_program_b.as_ref(),
            hook_accounts: ctx.remaining_accounts,
        };
        let o = open_leg(
            &sh,
            &mut leg,
            &args_b,
            &clock,
            Pricing::Given(q.receive.fixed_bp),
            Some(Link {
                other: swap_a_key,
                flags: Swap::link_flags_for(LegKind::ReceiveFixed),
            }),
        )?;
        leg.vault.reload()?;
        leg.pool.assert_invariants(leg.vault.amount)?;
        o
    };
    emit_event(
        &sh.event_authority,
        &BasisSwapOpened {
            trader: x.trader.key(),
            pool_a: x.pool_a.key(),
            swap_a: swap_a_key,
            pool_b: x.pool_b.key(),
            swap_b: swap_b_key,
            tenor: a.tenor,
            notional: a.notional,
            fixed_pay_bp: opened_a.fixed_bp,
            fixed_receive_bp: opened_b.fixed_bp,
            correlation_bp,
            offset_bp: q.offset_bp,
        },
    )
}

#[event_cpi]
#[derive(Accounts)]
pub struct TraderCancelBasisSwap<'info> {
    #[account(seeds = [b"global"], bump = global.bump, has_one = usdc_mint, has_one = token_program)]
    pub global: Box<Account<'info, Global>>,
    #[account(mut, seeds = [b"pool", pool_a.benchmark.as_ref()], bump = pool_a.bump, constraint = pool_a.benchmark == benchmark_a.key() @ BrinkError::BenchmarkNotPublished, constraint = pool_a.vault == vault_a.key() @ BrinkError::TokenOwner)]
    pub pool_a: Box<Account<'info, Pool>>,
    pub benchmark_a: Box<Account<'info, Benchmark>>,
    #[account(mut, close = trader, has_one = trader, constraint = swap_a.pool == pool_a.key() @ BrinkError::LinkNotPaired, constraint = swap_a.state == SwapState::Open @ BrinkError::AlreadySettled)]
    pub swap_a: Box<Account<'info, Swap>>,
    #[account(mut)]
    pub vault_a: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, seeds = [b"pool", pool_b.benchmark.as_ref()], bump = pool_b.bump, constraint = pool_b.benchmark == benchmark_b.key() @ BrinkError::BenchmarkNotPublished, constraint = pool_b.vault == vault_b.key() @ BrinkError::TokenOwner)]
    pub pool_b: Box<Account<'info, Pool>>,
    pub benchmark_b: Box<Account<'info, Benchmark>>,
    #[account(mut, close = trader, has_one = trader, constraint = swap_b.pool == pool_b.key() @ BrinkError::LinkNotPaired, constraint = swap_b.state == SwapState::Open @ BrinkError::AlreadySettled)]
    pub swap_b: Box<Account<'info, Swap>>,
    #[account(mut)]
    pub vault_b: Box<InterfaceAccount<'info, TokenAccount>>,
    /// The trader signs: both legs are theirs (`has_one`), and the rent of both returns to them.
    #[account(mut)]
    pub trader: Signer<'info>,
    #[account(mut, constraint = trader_usdc.owner == trader.key() @ BrinkError::TokenOwner, constraint = trader_usdc.mint == usdc_mint.key() @ BrinkError::SettlementMint)]
    pub trader_usdc: Box<InterfaceAccount<'info, TokenAccount>>,
    pub usdc_mint: Box<InterfaceAccount<'info, Mint>>,
    pub token_program: Interface<'info, TokenInterface>,
}

/// Both legs must name each other and carry the basis flags: the link is checked in both directions so that
/// neither an ordinary swap nor a leg of some other pair can be closed through this path.
pub(crate) fn check_linked(a: &Swap, a_key: Pubkey, b: &Swap, b_key: Pubkey) -> Result<()> {
    require!(
        a.is_linked() && b.is_linked() && a.link == b_key && b.link == a_key && a_key != b_key,
        BrinkError::LinkNotPaired
    );
    require!(
        a.link_flags == Swap::link_flags_for(LegKind::PayFixed)
            && b.link_flags == Swap::link_flags_for(LegKind::ReceiveFixed)
            && a.leg == LegKind::PayFixed
            && b.leg == LegKind::ReceiveFixed,
        BrinkError::LinkNotPaired
    );
    Ok(())
}

/// Trader closes both legs early, each at its own pool's opposite quote (or, once matured, each at its own
/// settlement), with `min_payout` enforced on the sum of the two payouts. Each leg's close is the one `cancel`
/// performs, including its own pool's conservation check, and emits its own `SwapClosed`.
pub fn cancel_basis_swap(ctx: Context<TraderCancelBasisSwap>, min_payout: u64) -> Result<()> {
    let clock = Clock::get()?;
    let swap_a_key = ctx.accounts.swap_a.key();
    let swap_b_key = ctx.accounts.swap_b.key();
    check_linked(
        &ctx.accounts.swap_a,
        swap_a_key,
        &ctx.accounts.swap_b,
        swap_b_key,
    )?;
    let matured = clock.unix_timestamp >= ctx.accounts.swap_a.matures_ts;
    // Before maturity a cancel is an entry into the opposite leg, so it observes the halt like `cancel`; at
    // maturity it is settlement and runs in any mode.
    if !matured {
        require!(
            ctx.accounts.global.mode != OperatingMode::Halted,
            BrinkError::Halted
        );
    }
    let x = &mut *ctx.accounts;
    let sh = CloseShared {
        signer: x.trader.key(),
        trader_usdc: &x.trader_usdc,
        cranker_usdc: None,
        usdc_mint: &x.usdc_mint,
        token_program: &x.token_program,
        event_authority: x.event_authority.to_account_info(),
    };
    let payout_a = {
        let mut leg = CloseLeg {
            pool: &mut x.pool_a,
            benchmark: &x.benchmark_a,
            swap: &mut x.swap_a,
            vault: &mut x.vault_a,
        };
        cancel_leg(&sh, &mut leg, &clock, None)?
    };
    let payout_b = {
        let mut leg = CloseLeg {
            pool: &mut x.pool_b,
            benchmark: &x.benchmark_b,
            swap: &mut x.swap_b,
            vault: &mut x.vault_b,
        };
        cancel_leg(&sh, &mut leg, &clock, None)?
    };
    let payout = payout_a.checked_add(payout_b).ok_or(BrinkError::Overflow)?;
    require!(payout >= min_payout, BrinkError::Slippage);
    emit_event(
        &sh.event_authority,
        &BasisSwapClosed {
            trader: x.trader.key(),
            swap_a: swap_a_key,
            swap_b: swap_b_key,
            payout,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instructions::math::SECONDS_PER_DAY;

    fn pool(tvl: u64) -> Pool {
        Pool {
            benchmark: Pubkey::new_unique(),
            share_mint: Pubkey::default(),
            vault: Pubkey::default(),
            hook_program: Pubkey::default(),
            hooks: HookFlags::default(),
            pricer: Pricer::Vernier,
            params: VernierParamsOnChain {
                model_pay_bp: [11, 22, 31, 54],
                model_rec_bp: [9, 12, 14, 19],
                term_bp: [3, 5, 7, 12],
                demand_k_bp: 45,
                demand_cap_bp: 60,
                collateral_bp: [120, 230, 330, 600],
            },
            pending_params: VernierParamsOnChain {
                model_pay_bp: [11, 22, 31, 54],
                model_rec_bp: [9, 12, 14, 19],
                term_bp: [3, 5, 7, 12],
                demand_k_bp: 45,
                demand_cap_bp: 60,
                collateral_bp: [120, 230, 330, 600],
            },
            pending_effective_slot: 0,
            tvl,
            collateral_held: 0,
            util_pay_bp: 0,
            util_rec_bp: 0,
            open_pay_notional: 0,
            open_rec_notional: 0,
            open_swaps: 0,
            fees_lifetime: 0,
            event_seq: 0,
            min_notional: 1,
            max_notional: u64::MAX,
            bump: 0,
            fees_buyback_accrued: 0,
            fees_treasury_accrued: 0,
            withdraw_reserved: 0,
            book_pay: BookSide::default(),
            book_rec: BookSide::default(),
            collateral_pay: 0,
            collateral_rec: 0,
            ladder: [0; LADDER_BYTES],
            share_supply: 0,
            queued_shares: 0,
            queue_first_slot: 0,
            limited_window_start: 0,
            limited_window_notional: 0,
            reserve_placed: 0,
            reserve_active: 0,
            _reserved: [0; 15],
        }
    }
    fn benchmark(value_bp: u16, slot: u64) -> Benchmark {
        Benchmark {
            registry: Pubkey::default(),
            id: [1; 16],
            source: Pubkey::default(),
            value_bp,
            ema_bp: value_bp,
            slot,
            unix_ts: 20_000 * SECONDS_PER_DAY,
            accrual_e18: 0,
            max_staleness_slots: 1_000,
            band_bp: 300,
            half_life_slots: 1,
            min_interval_slots: 0,
            published: true,
            publish_count: 1,
            bump: 0,
            prev_value_bp: value_bp,
            prev_unix_ts: 20_000 * SECONDS_PER_DAY,
            version: brink_index::LAYOUT_VERSION,
            max_drift_bp: 400,
            drift_window_slots: 216_000,
            drift_anchor_bp: value_bp,
            drift_anchor_slot: slot,
            clamped: false,
            disputed: false,
            support: 1,
            observations: [brink_index::Observation::default(); brink_index::MAX_PUBLISHERS],
            ema_milli_bp: 0,
            fixing_first_day: 0,
            fixing_head_day: 0,
            fixings: Box::new([0; brink_index::FIXING_BYTES]),
            ema_slot: slot,
            accepted_slot: slot,
            support_lost: false,
            _reserved: [0; 11],
        }
    }
    fn swap(leg: LegKind, link: Pubkey, flags: u8) -> Swap {
        Swap {
            pool: Pubkey::new_unique(),
            trader: Pubkey::new_unique(),
            leg,
            tenor: 2,
            notional: 1,
            fixed_bp: 700,
            collateral: 1,
            opened_slot: 0,
            opened_ts: 0,
            matures_ts: 0,
            index_accrual_start: 0,
            client_seed: 0,
            state: SwapState::Open,
            bump: 0,
            limited_window_start: 0,
            link,
            link_flags: flags,
            start_ts: 0,
            _reserved: [0; 15],
        }
    }

    /// The link lives in the former reserve: the account size is unchanged, a zero link is an ordinary swap
    /// whatever the flags, and the flags identify the two legs.
    #[test]
    fn link_encoding_keeps_the_layout_and_a_zero_link_is_ordinary() {
        assert_eq!(Swap::INIT_SPACE, SWAP_SPACE);
        assert_eq!(32 + 1 + 8 + 15, 56);
        let plain = swap(LegKind::PayFixed, Pubkey::default(), 0);
        assert!(!plain.is_linked());
        // Flags without a key never read as linked (a pre-link swap's reserve is all zero, but be total).
        let flagged = swap(LegKind::PayFixed, Pubkey::default(), LINK_BASIS);
        assert!(!flagged.is_linked());
        // A key without the basis flag is not a link either.
        let keyed = swap(LegKind::PayFixed, Pubkey::new_unique(), 0);
        assert!(!keyed.is_linked());
        let a_key = Pubkey::new_unique();
        let b_key = Pubkey::new_unique();
        let a = swap(
            LegKind::PayFixed,
            b_key,
            Swap::link_flags_for(LegKind::PayFixed),
        );
        let b = swap(
            LegKind::ReceiveFixed,
            a_key,
            Swap::link_flags_for(LegKind::ReceiveFixed),
        );
        assert!(a.is_linked() && b.is_linked());
        assert_eq!(a.link_flags, LINK_BASIS);
        assert_eq!(b.link_flags, LINK_BASIS | LINK_LEG_B);
        // Borsh round trip of the whole account keeps the link where the reserve was.
        let mut bytes = Vec::new();
        a.serialize(&mut bytes).unwrap();
        // `INIT_SPACE` counts the widest `SwapState` variant (9 bytes); `Open` serialises as one byte.
        assert_eq!(bytes.len(), SWAP_SPACE - 8);
        let back = Swap::deserialize(&mut bytes.as_slice()).unwrap();
        assert_eq!((back.link, back.link_flags), (b_key, LINK_BASIS));
        // The old layout: the same prefix followed by 56 zero bytes decodes as an unlinked swap.
        let mut old = bytes[..bytes.len() - 56].to_vec();
        old.extend_from_slice(&[0u8; 56]);
        let legacy = Swap::deserialize(&mut old.as_slice()).unwrap();
        assert!(!legacy.is_linked());
        assert_eq!(legacy.link, Pubkey::default());
    }

    /// Pairing is checked in both directions, with the leg kinds and flags.
    #[test]
    fn linked_cancel_requires_mutual_links_and_the_right_legs() {
        let a_key = Pubkey::new_unique();
        let b_key = Pubkey::new_unique();
        let a = swap(LegKind::PayFixed, b_key, LINK_BASIS);
        let b = swap(LegKind::ReceiveFixed, a_key, LINK_BASIS | LINK_LEG_B);
        assert!(check_linked(&a, a_key, &b, b_key).is_ok());
        // Swapped order, an ordinary swap, a one-way link, a third swap's key, wrong flags.
        assert!(check_linked(&b, b_key, &a, a_key).is_err());
        let plain = swap(LegKind::ReceiveFixed, Pubkey::default(), 0);
        assert!(check_linked(&a, a_key, &plain, b_key).is_err());
        let stranger = swap(
            LegKind::ReceiveFixed,
            Pubkey::new_unique(),
            LINK_BASIS | LINK_LEG_B,
        );
        assert!(check_linked(&a, a_key, &stranger, b_key).is_err());
        let wrong_flags = swap(LegKind::ReceiveFixed, a_key, LINK_BASIS);
        assert!(check_linked(&a, a_key, &wrong_flags, b_key).is_err());
        assert!(check_linked(&a, a_key, &b, a_key).is_err());
    }

    /// The governance bound and the library bound compose: the on-chain cap is the tighter one.
    #[test]
    fn correlation_bound_is_half_the_demand_at_most() {
        assert!(check_correlation_bound(0).is_ok());
        assert!(check_correlation_bound(MAX_CORRELATION_BP).is_ok());
        assert!(check_correlation_bound(MAX_CORRELATION_BP + 1).is_err());
        assert_eq!(MAX_CORRELATION_BP, 5_000);
    }

    /// A basis open prices both legs together and the offset lands on the demand components only, with the
    /// pay leg lower and the receive leg higher than their single-swap quotes by their own offsets.
    #[test]
    fn basis_legs_are_quoted_against_their_own_pools_with_the_offset() {
        let mut pa = pool(10_000_000);
        pa.util_pay_bp = 2_000;
        let pb = pool(10_000_000);
        let ba = benchmark(684, 100);
        let bb = benchmark(420, 100);
        let n = 1_000_000;
        let legs = BasisLegs {
            pool_a: &pa,
            bench_a: &ba,
            pool_b: &pb,
            bench_b: &bb,
        };
        let q0 = quote_basis_legs(&legs, 100, vernier::Tenor::D90, n, 0).unwrap();
        let q = quote_basis_legs(&legs, 100, vernier::Tenor::D90, n, 5_000).unwrap();
        assert!(q0.pay.demand_bp > 0 && q0.receive.demand_bp < 0);
        assert_eq!(
            q.pay.fixed_bp,
            q0.pay.fixed_bp - q0.pay.demand_bp * 5_000 / 10_000
        );
        assert_eq!(
            q.receive.fixed_bp,
            q0.receive.fixed_bp + (-q0.receive.demand_bp) * 5_000 / 10_000
        );
        assert_eq!(q.pay.model_bp, q0.pay.model_bp);
        assert_eq!(q.receive.term_bp, q0.receive.term_bp);
        // Above the library bound the quote is refused; the chain bound is checked earlier and tighter.
        assert!(quote_basis_legs(&legs, 100, vernier::Tenor::D90, n, 10_001).is_err());
        // A stale benchmark on either side refuses the whole open.
        assert!(quote_basis_legs(&legs, 5_000, vernier::Tenor::D90, n, 0).is_err());
    }

    /// Leg B failing refuses the basis open before leg A is touched: the entry checks run for both pools first
    /// and are pure, so pool A is bit-for-bit what it was. On chain the transaction's atomicity gives the same
    /// guarantee after the money has moved; the SVM suite checks that side.
    #[test]
    fn basis_open_refuses_before_leg_a_when_leg_b_fails() {
        let pa = pool(10_000_000);
        let snapshot_a = pa.vernier_pool();
        let snapshot_book = (pa.open_pay_notional, pa.collateral_held, pa.open_swaps);
        // Pool B is at its receive cap.
        let mut pb = pool(10_000_000);
        pb.util_rec_bp = 4_800;
        pb.open_rec_notional = 4_800_000;
        let r = check_basis_entry(&pa, &pb, 1_000, 10);
        assert_eq!(
            r.unwrap_err(),
            anchor_lang::error::Error::from(BrinkError::LegCap)
        );
        assert_eq!(pa.vernier_pool(), snapshot_a);
        assert_eq!(
            (pa.open_pay_notional, pa.collateral_held, pa.open_swaps),
            snapshot_book
        );
        // Pool B with no capital, pool B below its minimum, and an eligible queue on pool B all refuse too.
        let empty = pool(0);
        assert!(check_basis_entry(&pa, &empty, 1_000, 10).is_err());
        let mut small = pool(10_000_000);
        small.min_notional = 2_000;
        assert!(check_basis_entry(&pa, &small, 1_000, 10).is_err());
        let mut queued = pool(1_000_000);
        queued.share_supply = 1_000_000_000;
        queued.queued_shares = 900_000_000;
        queued.queue_first_slot = 1;
        assert!(check_basis_entry(
            &pa,
            &queued,
            400_000,
            1 + super::super::queue::WITHDRAW_EPOCH_SLOTS
        )
        .is_err());
        // And a well-formed pair passes.
        assert!(check_basis_entry(&pa, &pool(10_000_000), 1_000, 10).is_ok());
    }
}
