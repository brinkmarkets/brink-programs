//! Basis swaps on LiteSVM: one instruction opens a pay-fixed leg on pool A and a
//! receive-fixed leg on pool B, linked by key; the trader closes both with one floor; the cranks settle and
//! liquidate each leg on its own; an absent pair record refuses the pair.
use brink_svm_tests::harness::*;
use brink_svm_tests::*;
use solana_instruction::AccountMeta;
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signer::Signer;

const SEED: u64 = 11;
const NOTIONAL: u64 = 1_000_000 * USDC;

struct Two {
    e: Env,
    a: PoolKeys,
    b: PoolKeys,
    tr: Keypair,
}

/// Two pools with 10,000,000 USDC each: A on the default benchmark (684 bp), B on a second one at 500 bp.
fn two_pools() -> Two {
    let mut e = setup();
    let lp = e.lp.insecure_clone();
    let tr = e.trader.insecure_clone();
    e.deposit(&lp, 10_000_000 * USDC);
    let a = e.pool_keys();
    let bench_b = e.create_benchmark(*b"second-usdc\0\0\0\0\0", 500);
    let b = e.create_pool(bench_b, None, HookFlags::default()).unwrap();
    e.use_pool(&b);
    let m = e.share_mint;
    e.create_ata(&lp.pubkey(), &m);
    e.deposit(&lp, 10_000_000 * USDC);
    e.use_pool(&a);
    Two { e, a, b, tr }
}

fn args(limit_pay: u16, limit_receive: u16) -> OpenBasisSwapArgs {
    OpenBasisSwapArgs {
        tenor: 2,
        notional: NOTIONAL,
        limit_pay_bp: limit_pay,
        limit_receive_bp: limit_receive,
        client_seed: SEED,
    }
}

fn legs(t: &Two) -> (Pubkey, Pubkey) {
    (
        Env::swap_pda_on(&t.a.pool, &t.tr.pubkey(), SEED),
        Env::swap_pda_on(&t.b.pool, &t.tr.pubkey(), SEED),
    )
}

/// Publishes the current value on both benchmarks so neither leg's quote is stale after a warp.
fn refresh(t: &mut Two) {
    t.e.publish(684).unwrap();
    let a = t.e.use_pool(&t.b);
    t.e.publish(500).unwrap();
    t.e.use_pool(&a);
}

fn enable_pair(t: &mut Two, correlation_bp: u16) {
    let ix = t.e.set_basis_pair_ix(&t.a.pool, &t.b.pool, correlation_bp);
    let au = t.e.authority.insecure_clone();
    t.e.must(&[ix], &[&au]);
}

#[test]
fn basis_open_books_two_linked_legs_with_the_offset_on_demand_only() {
    let mut t = two_pools();
    // Governance bounds: the pair must be two pools, the offset at most MAX_CORRELATION_BP, authority only.
    let au = t.e.authority.insecure_clone();
    let ix = t.e.set_basis_pair_ix(&t.a.pool, &t.a.pool, 0);
    t.e.must_fail(&[ix], &[&au], "BasisSamePool");
    let ix = t.e.set_basis_pair_ix(&t.a.pool, &t.b.pool, 5_001);
    t.e.must_fail(&[ix], &[&au], "CorrelationBound");
    let stranger = Keypair::new();
    t.e.svm.airdrop(&stranger.pubkey(), 1_000_000_000).unwrap();
    let mut ix = t.e.set_basis_pair_ix(&t.a.pool, &t.b.pool, 0);
    ix.accounts[1] = AccountMeta::new_readonly(stranger.pubkey(), true);
    t.e.must_fail(&[ix], &[&stranger], "ConstraintHasOne");
    enable_pair(&mut t, 0);
    let pair: BasisPair =
        t.e.acct("BasisPair", &Env::basis_pair_pda(&t.a.pool, &t.b.pool));
    assert_eq!(
        (pair.pool_a, pair.pool_b, pair.correlation_bp),
        (t.a.pool, t.b.pool, 0)
    );

    // Without an offset each leg is the ordinary quote on its pool: pay on A 684 + 31 + 2 + 7 = 724, receive
    // on B 500 - 14 - 2 - 7 = 477 (trapezoid demand over the fill, as in the single-swap e2e test).
    let tr = t.tr.insecure_clone();
    let ix = t.e.open_basis_ix(&tr.pubkey(), &t.a, &t.b, &args(724, 477));
    t.e.must(&[ix], &[&tr]);
    let (ka, kb) = legs(&t);
    let sa: Swap = t.e.acct("Swap", &ka);
    let sb: Swap = t.e.acct("Swap", &kb);
    assert_eq!(sa.fixed_bp, 724);
    assert_eq!(sb.fixed_bp, 477);
    assert_eq!(sa.leg, LegKind::PayFixed);
    assert_eq!(sb.leg, LegKind::ReceiveFixed);
    assert_eq!((sa.link, sa.link_flags), (kb, LINK_BASIS));
    assert_eq!((sb.link, sb.link_flags), (ka, LINK_BASIS | LINK_LEG_B));
    assert_eq!(sa.notional, NOTIONAL);
    assert_eq!(sb.notional, NOTIONAL);
    assert_eq!(sa.matures_ts, sb.matures_ts);
    assert_eq!(sa.collateral, 33_000 * USDC, "90-day floor is 330 bp");
    assert_eq!(sb.collateral, 33_000 * USDC);
    let pa: Pool = t.e.acct("Pool", &t.a.pool);
    let pb: Pool = t.e.acct("Pool", &t.b.pool);
    assert_eq!(
        (pa.open_swaps, pa.open_pay_notional, pa.util_pay_bp),
        (1, NOTIONAL, 1_000)
    );
    assert_eq!(
        (pb.open_swaps, pb.open_rec_notional, pb.util_rec_bp),
        (1, NOTIONAL, 1_000)
    );
    assert_eq!(pa.collateral_held, 33_000 * USDC);
    assert_eq!(pb.collateral_held, 33_000 * USDC);
    assert_eq!(
        t.e.token_amount(&t.a.vault),
        pa.tvl + pa.collateral_held + pa.fees_buyback_accrued + pa.fees_treasury_accrued
    );
    assert_eq!(
        t.e.token_amount(&t.b.vault),
        pb.tvl + pb.collateral_held + pb.fees_buyback_accrued + pb.fees_treasury_accrued
    );
    // Both legs' collateral and fees left the trader: 2 × (33,000 + 500).
    assert_eq!(
        t.e.token_amount(&ata(&tr.pubkey(), &USDC_DEVNET)),
        1_000_000 * USDC - 2 * (33_000 + 500) * USDC
    );

    // A second basis swap with the offset raised to half: demand at 10 to 20 percent fill is
    // round(45 × (1000 + 2000) / 2 / 10000) = 7 on each pool, so pay 684 + 31 + 7 + 7 = 729 falls by
    // floor(7 × 0.5) = 3 to 726 and receive 500 - 14 - 7 - 7 = 472 rises by 3 to 475. Model and term unchanged.
    let ix = t.e.update_basis_pair_ix(&t.a.pool, &t.b.pool, 5_000);
    t.e.must(&[ix], &[&au]);
    let mut second = args(726, 475);
    second.client_seed = SEED + 1;
    let ix = t.e.open_basis_ix(&tr.pubkey(), &t.a, &t.b, &second);
    t.e.must(&[ix], &[&tr]);
    let sa2: Swap =
        t.e.acct("Swap", &Env::swap_pda_on(&t.a.pool, &tr.pubkey(), SEED + 1));
    let sb2: Swap =
        t.e.acct("Swap", &Env::swap_pda_on(&t.b.pool, &tr.pubkey(), SEED + 1));
    assert_eq!(sa2.fixed_bp, 726);
    assert_eq!(sb2.fixed_bp, 475);
    // Each leg's own limit is enforced: one basis point tighter on either side refuses the whole open.
    let mut third = args(725, 475);
    third.client_seed = SEED + 2;
    let ix = t.e.open_basis_ix(&tr.pubkey(), &t.a, &t.b, &third);
    t.e.must_fail(&[ix], &[&tr], "LimitRate");
    let mut third = args(726, 476);
    third.client_seed = SEED + 2;
    let ix = t.e.open_basis_ix(&tr.pubkey(), &t.a, &t.b, &third);
    t.e.must_fail(&[ix], &[&tr], "LimitRate");
    assert!(t
        .e
        .svm
        .get_account(&Env::swap_pda_on(&t.a.pool, &tr.pubkey(), SEED + 2))
        .is_none());
}

#[test]
fn linked_cancel_pays_the_net_with_one_floor_and_single_leg_cancel_is_refused() {
    let mut t = two_pools();
    enable_pair(&mut t, 2_500);
    let tr = t.tr.insecure_clone();
    let ix = t.e.open_basis_ix(&tr.pubkey(), &t.a, &t.b, &args(9_999, 0));
    t.e.must(&[ix], &[&tr]);
    let (ka, kb) = legs(&t);
    t.e.warp(10 * 216_000, 10 * DAY);
    refresh(&mut t);

    // Neither leg can be cancelled on its own while the other is open.
    let ix = t.e.close_ix(
        "trader_cancel_swap",
        &tr.pubkey(),
        SEED,
        &tr.pubkey(),
        &0u64,
    );
    t.e.must_fail(&[ix], &[&tr], "LinkedLeg");
    let old = t.e.use_pool(&t.b);
    let ix = t.e.close_ix(
        "trader_cancel_swap",
        &tr.pubkey(),
        SEED,
        &tr.pubkey(),
        &0u64,
    );
    t.e.must_fail(&[ix], &[&tr], "LinkedLeg");
    // Naming an unrelated live account as the partner does not satisfy the orphan rule either.
    let mut ix = t.e.close_ix(
        "trader_cancel_swap",
        &tr.pubkey(),
        SEED,
        &tr.pubkey(),
        &0u64,
    );
    ix.accounts.push(AccountMeta::new_readonly(ka, false));
    t.e.must_fail(&[ix], &[&tr], "LinkedLeg");
    t.e.use_pool(&old);
    // Nor through the basis path with the legs in the wrong order or with a stranger's swap.
    let ix = t.e.cancel_basis_ix(&tr.pubkey(), &t.b, &t.a, SEED, 0);
    t.e.must_fail(&[ix], &[&tr], "LinkNotPaired");
    let sa: Swap = t.e.acct("Swap", &ka);
    let sb: Swap = t.e.acct("Swap", &kb);
    assert_eq!(sa.state, SwapState::Open);
    assert_eq!(sb.state, SwapState::Open);

    // The floor applies to the sum: more than both collaterals is refused, zero passes.
    let before = t.e.token_amount(&ata(&tr.pubkey(), &USDC_DEVNET));
    let ix = t.e.cancel_basis_ix(
        &tr.pubkey(),
        &t.a,
        &t.b,
        SEED,
        sa.collateral + sb.collateral + 1,
    );
    t.e.must_fail(&[ix], &[&tr], "Slippage");
    let ix = t.e.cancel_basis_ix(&tr.pubkey(), &t.a, &t.b, SEED, 1);
    let logs = t.e.must(&[ix], &[&tr]);
    let after = t.e.token_amount(&ata(&tr.pubkey(), &USDC_DEVNET));
    assert!(
        after > before && after - before < sa.collateral + sb.collateral,
        "net payout returns both collaterals less the two unwinds"
    );
    // Two per-leg closes and one basis event (three event CPIs from the swap program).
    let emitted = logs
        .iter()
        .filter(|l| l.contains(&format!("Program {SWAP_AMM} invoke [2]")))
        .count();
    assert_eq!(
        emitted,
        3,
        "two SwapClosed and one BasisSwapClosed\n{}",
        logs.join("\n")
    );
    assert!(
        t.e.svm.get_account(&ka).is_none(),
        "leg A closed to the trader"
    );
    assert!(
        t.e.svm.get_account(&kb).is_none(),
        "leg B closed to the trader"
    );
    let pa: Pool = t.e.acct("Pool", &t.a.pool);
    let pb: Pool = t.e.acct("Pool", &t.b.pool);
    assert_eq!(
        (pa.open_swaps, pa.collateral_held, pa.open_pay_notional),
        (0, 0, 0)
    );
    assert_eq!(
        (pb.open_swaps, pb.collateral_held, pb.open_rec_notional),
        (0, 0, 0)
    );
    assert_eq!(
        t.e.token_amount(&t.a.vault),
        pa.tvl + pa.fees_buyback_accrued + pa.fees_treasury_accrued
    );
    assert_eq!(
        t.e.token_amount(&t.b.vault),
        pb.tvl + pb.fees_buyback_accrued + pb.fees_treasury_accrued
    );
}

#[test]
fn liquidation_of_one_leg_leaves_the_other_and_frees_the_orphan() {
    let mut t = two_pools();
    enable_pair(&mut t, 5_000);
    let tr = t.tr.insecure_clone();
    let ix = t.e.open_basis_ix(&tr.pubkey(), &t.a, &t.b, &args(9_999, 0));
    t.e.must(&[ix], &[&tr]);
    let (ka, kb) = legs(&t);
    let sb: Swap = t.e.acct("Swap", &kb);
    let cranker = t.e.payer.pubkey();

    // Pool B's index runs up a day at a time until the receive-fixed leg is exhausted; pool A stays at 684.
    // Each leg is fully collateralised in its own pool, so liquidating B neither needs nor touches A.
    let mut liquidated = false;
    for _ in 0..40 {
        t.e.warp(216_000, DAY);
        t.e.publish(684).unwrap();
        let old = t.e.use_pool(&t.b);
        let bb: Benchmark = t.e.acct("Benchmark", &t.b.benchmark);
        t.e.publish((bb.ema_bp + 300).min(30_000)).unwrap();
        let ix =
            t.e.close_ix("crank_liquidate_swap", &tr.pubkey(), SEED, &cranker, &());
        let r = t.e.send(&[ix], &[]);
        t.e.use_pool(&old);
        match r {
            Ok(_) => {
                liquidated = true;
                break;
            }
            Err(err) => assert!(err.contains("NotLiquidatable"), "{err}"),
        }
    }
    assert!(
        liquidated,
        "the receive-fixed leg must exhaust as its index runs up"
    );
    assert!(
        t.e.svm.get_account(&kb).is_none(),
        "leg B closed by the crank"
    );
    let sa: Swap = t.e.acct("Swap", &ka);
    assert_eq!(sa.state, SwapState::Open);
    assert_eq!((sa.link, sa.link_flags), (kb, LINK_BASIS));
    let pa: Pool = t.e.acct("Pool", &t.a.pool);
    let pb: Pool = t.e.acct("Pool", &t.b.pool);
    assert_eq!((pa.open_swaps, pa.collateral_held), (1, sa.collateral));
    assert_eq!((pb.open_swaps, pb.collateral_held), (0, 0));
    assert!(
        pb.tvl >= 10_000_000 * USDC + sb.collateral * 9_900 / 10_000 - 1,
        "pool B kept the exhausted collateral"
    );
    // The linked cancel can no longer pair the legs.
    let ix = t.e.cancel_basis_ix(&tr.pubkey(), &t.a, &t.b, SEED, 0);
    t.e.must_fail(&[ix], &[&tr], "AccountNotInitialized");
    // The orphaned leg A: the ordinary cancel is refused without the partner, and allowed once the partner
    // account is shown to be closed, at the ordinary opposite quote on pool A.
    let ix = t.e.close_ix(
        "trader_cancel_swap",
        &tr.pubkey(),
        SEED,
        &tr.pubkey(),
        &0u64,
    );
    t.e.must_fail(&[ix], &[&tr], "LinkedLeg");
    let before = t.e.token_amount(&ata(&tr.pubkey(), &USDC_DEVNET));
    let mut ix = t.e.close_ix(
        "trader_cancel_swap",
        &tr.pubkey(),
        SEED,
        &tr.pubkey(),
        &0u64,
    );
    ix.accounts.push(AccountMeta::new_readonly(kb, false));
    t.e.must(&[ix], &[&tr]);
    assert!(t.e.svm.get_account(&ka).is_none());
    let after = t.e.token_amount(&ata(&tr.pubkey(), &USDC_DEVNET));
    assert!(after > before && after - before < sa.collateral);
    let pa: Pool = t.e.acct("Pool", &t.a.pool);
    assert_eq!(
        (pa.open_swaps, pa.collateral_held, pa.open_pay_notional),
        (0, 0, 0)
    );
    assert_eq!(
        t.e.token_amount(&t.a.vault),
        pa.tvl + pa.fees_buyback_accrued + pa.fees_treasury_accrued
    );
}

#[test]
fn each_leg_settles_at_maturity_through_the_crank() {
    let mut t = two_pools();
    enable_pair(&mut t, 1_000);
    let tr = t.tr.insecure_clone();
    let ix = t.e.open_basis_ix(&tr.pubkey(), &t.a, &t.b, &args(9_999, 0));
    t.e.must(&[ix], &[&tr]);
    let (ka, kb) = legs(&t);
    let sa: Swap = t.e.acct("Swap", &ka);
    // Rates move half way: A up (the pay-fixed leg accrues a gain), B down (the receive-fixed leg gains).
    t.e.warp(45 * 216_000, 45 * DAY);
    t.e.publish(900).unwrap();
    let old = t.e.use_pool(&t.b);
    t.e.publish(300).unwrap();
    t.e.use_pool(&old);
    let to_maturity = sa.matures_ts - t.e.clock().unix_timestamp;
    t.e.warp((to_maturity / DAY) as u64 * 216_000 + 1, to_maturity);
    t.e.publish(900).unwrap();
    let old = t.e.use_pool(&t.b);
    t.e.publish(300).unwrap();
    t.e.use_pool(&old);
    let cranker = t.e.payer.pubkey();
    let before = t.e.token_amount(&ata(&tr.pubkey(), &USDC_DEVNET));
    // Leg A settles alone; leg B is open and linked until its own crank.
    let ix =
        t.e.close_ix("crank_settle_swap", &tr.pubkey(), SEED, &cranker, &());
    t.e.must(&[ix], &[]);
    assert!(t.e.svm.get_account(&ka).is_none());
    let sb: Swap = t.e.acct("Swap", &kb);
    assert_eq!(sb.state, SwapState::Open);
    let mid = t.e.token_amount(&ata(&tr.pubkey(), &USDC_DEVNET));
    assert!(
        mid > before + sa.collateral,
        "pay-fixed leg settles at a gain"
    );
    t.e.use_pool(&t.b);
    let ix =
        t.e.close_ix("crank_settle_swap", &tr.pubkey(), SEED, &cranker, &());
    t.e.must(&[ix], &[]);
    assert!(t.e.svm.get_account(&kb).is_none());
    let after = t.e.token_amount(&ata(&tr.pubkey(), &USDC_DEVNET));
    assert!(
        after > mid + sb.collateral,
        "receive-fixed leg settles at a gain"
    );
    let pa: Pool = t.e.acct("Pool", &t.a.pool);
    let pb: Pool = t.e.acct("Pool", &t.b.pool);
    assert_eq!((pa.open_swaps, pa.collateral_held), (0, 0));
    assert_eq!((pb.open_swaps, pb.collateral_held), (0, 0));
    assert_eq!(
        t.e.token_amount(&t.a.vault),
        pa.tvl + pa.fees_buyback_accrued + pa.fees_treasury_accrued
    );
    assert_eq!(
        t.e.token_amount(&t.b.vault),
        pb.tvl + pb.fees_buyback_accrued + pb.fees_treasury_accrued
    );
}

#[test]
fn absent_pair_and_a_failing_leg_b_refuse_the_whole_open() {
    let mut t = two_pools();
    let tr = t.tr.insecure_clone();
    let (ka, kb) = legs(&t);
    let snap_a: Pool = t.e.acct("Pool", &t.a.pool);
    let trader_before = t.e.token_amount(&ata(&tr.pubkey(), &USDC_DEVNET));
    // No pair record: refused before anything is read from the pools.
    let ix = t.e.open_basis_ix(&tr.pubkey(), &t.a, &t.b, &args(9_999, 0));
    t.e.must_fail(&[ix], &[&tr], "AccountNotInitialized");
    // The pair is ordered: enabling (A, B) does not enable (B, A).
    enable_pair(&mut t, 0);
    let ix = t.e.open_basis_ix(&tr.pubkey(), &t.b, &t.a, &args(9_999, 0));
    t.e.must_fail(&[ix], &[&tr], "AccountNotInitialized");
    // The same pool on both sides has no pair record (and the pair instruction refuses to create one).
    let ix = t.e.open_basis_ix(&tr.pubkey(), &t.a, &t.a, &args(9_999, 0));
    t.e.must_fail(&[ix], &[&tr], "AccountNotInitialized");
    assert!(t.e.svm.get_account(&ka).is_none());
    assert!(t.e.svm.get_account(&kb).is_none());

    // Leg B failing leaves no leg A behind: a third pool with too little capital for the receive leg.
    let bench_c = t.e.create_benchmark(*b"third-usdc\0\0\0\0\0\0", 450);
    let c =
        t.e.create_pool(bench_c, None, HookFlags::default())
            .unwrap();
    t.e.use_pool(&c);
    let lp_c = t.e.new_actor(10_000 * USDC, true);
    t.e.deposit(&lp_c, 1_500 * USDC);
    t.e.use_pool(&t.a);
    let ix = t.e.set_basis_pair_ix(&t.a.pool, &c.pool, 0);
    let au = t.e.authority.insecure_clone();
    t.e.must(&[ix], &[&au]);
    let ix = t.e.open_basis_ix(&tr.pubkey(), &t.a, &c, &args(9_999, 0));
    t.e.must_fail(&[ix], &[&tr], "LegCap");
    assert!(t.e.svm.get_account(&ka).is_none(), "no leg A left behind");
    assert!(t
        .e
        .svm
        .get_account(&Env::swap_pda_on(&c.pool, &tr.pubkey(), SEED))
        .is_none());
    let pa: Pool = t.e.acct("Pool", &t.a.pool);
    assert_eq!(
        (
            pa.open_swaps,
            pa.collateral_held,
            pa.open_pay_notional,
            pa.util_pay_bp,
            pa.tvl
        ),
        (
            snap_a.open_swaps,
            snap_a.collateral_held,
            snap_a.open_pay_notional,
            snap_a.util_pay_bp,
            snap_a.tvl
        )
    );
    assert_eq!(
        t.e.token_amount(&ata(&tr.pubkey(), &USDC_DEVNET)),
        trader_before
    );
    // And a halt refuses a basis open like any other entry.
    t.e.set_mode(&au, OperatingMode::Halted).unwrap();
    let ix = t.e.open_basis_ix(&tr.pubkey(), &t.a, &t.b, &args(9_999, 0));
    t.e.must_fail(&[ix], &[&tr], "Halted");
}
