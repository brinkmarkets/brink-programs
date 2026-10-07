//! End-to-end tests for the Brink programs on LiteSVM. Each test boots a fresh VM, loads the three compiled
//! programs from `target/deploy`, seeds a USDC mint at the devnet address, and drives the real instructions.
//! Run from `programs/`: `./build-sbf.sh && (cd tests/svm && cargo test)`. The harness is `src/harness.rs`.
use brink_svm_tests::harness::*;
use brink_svm_tests::*;
use solana_instruction::Instruction;
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signer::Signer;

#[test]
fn lifecycle_deposit_open_settle_with_income_fee_and_sweep() {
    let mut e = setup();
    let lp = e.lp.insecure_clone();
    let tr = e.trader.insecure_clone();
    // 10,000,000 USDC of LP capital → 10,000,000 share-units at par; the share mint has 12 decimals, so the
    // first deposit mints amount × 10^6 shares (maths patch 0005, virtual offset).
    // Two tranches: a single deposit above about 9.22 million USDC fails on the event's i64 share field
    // (round-02 test finding T-1, see tests/red.rs).
    let ix = e.deposit_ix(&lp.pubkey(), 5_000_000 * USDC, 5_000_000 * USDC * SHARE);
    e.must(&[ix], &[&lp]);
    let ix = e.deposit_ix(&lp.pubkey(), 5_000_000 * USDC, 5_000_000 * USDC * SHARE - 1);
    e.must(&[ix], &[&lp]);
    let p: Pool = e.acct("Pool", &e.pool);
    assert_eq!(p.tvl, 10_000_000 * USDC);
    assert_eq!(e.mint_supply(&e.share_mint), 10_000_000 * USDC * SHARE);
    assert_eq!(e.token_amount(&e.vault), p.tvl);

    // Pay fixed 1,000,000 USDC for 90 days. Vernier: ref 684 + model 31 + demand 2 + term 7 = 724; the demand
    // spread is the trapezoid rule over the fill (maths M-3, patch 0003): round(45 × (0 + 1000) / 2 / 10000) = 2.
    let ix = e.open_ix(
        &tr.pubkey(),
        &open_args(LegKind::PayFixed, 2, 1_000_000 * USDC, 724, 1),
    );
    e.must(&[ix], &[&tr]);
    let s: Swap = e.acct("Swap", &e.swap_pda(&tr.pubkey(), 1));
    assert_eq!(s.fixed_bp, 724);
    assert_eq!(s.collateral, 33_000 * USDC);
    assert_eq!(s.matures_ts - s.opened_ts, 90 * DAY);
    assert_eq!(s.state, SwapState::Open);
    let p: Pool = e.acct("Pool", &e.pool);
    assert_eq!(p.open_pay_notional, 1_000_000 * USDC);
    assert_eq!(p.util_pay_bp, 1_000);
    assert_eq!(p.collateral_held, 33_000 * USDC);
    assert_eq!(p.open_swaps, 1);
    // Opening fee 5 bp = 500 USDC, split 250 / 250, held in the pool vault until swept (F-29, ADR-001).
    assert_eq!(p.fees_buyback_accrued, 250 * USDC);
    assert_eq!(p.fees_treasury_accrued, 250 * USDC);
    assert_eq!(
        e.token_amount(&e.vault),
        p.tvl + p.collateral_held + p.fees_buyback_accrued + p.fees_treasury_accrued
    );
    let g: Global = e.acct("Global", &e.global);
    assert_eq!(g.buyback_accrued, 0, "Global no longer accrues fees");
    let trader_before = e.token_amount(&ata(&tr.pubkey(), &USDC_DEVNET));
    assert_eq!(trader_before, 1_000_000 * USDC - 33_000 * USDC - 500 * USDC);

    // Floating rises to 900 bp half way; the trader (pay fixed) gains.
    e.warp(45 * 216_000, 45 * DAY);
    e.publish(900).unwrap();
    e.warp(45 * 216_000, 45 * DAY);
    e.publish(900).unwrap();
    // Not matured one second early.
    let mut c = e.clock();
    c.unix_timestamp -= 1;
    e.svm.set_sysvar(&c);
    let ix = e.close_ix("crank_settle_swap", &tr.pubkey(), 1, &e.payer.pubkey(), &());
    e.must_fail(&[ix], &[], "NotMatured");
    let mut c = e.clock();
    c.unix_timestamp += 1;
    e.svm.set_sysvar(&c);
    // A third party cranks and claims the bounty: 2 bp of 1,000,000 USDC = 200 USDC, capped at 25 USDC.
    let payer = e.payer.pubkey();
    let cranker_usdc = e.create_ata(&payer, &USDC_DEVNET);
    // A bounty destination not owned by the signer is rejected.
    let ix = e.close_ix_bounty(
        "crank_settle_swap",
        &tr.pubkey(),
        1,
        &payer,
        Some(ata(&lp.pubkey(), &USDC_DEVNET)),
        &(),
    );
    e.must_fail(&[ix], &[], "TokenOwner");
    let ix = e.close_ix_bounty(
        "crank_settle_swap",
        &tr.pubkey(),
        1,
        &payer,
        Some(cranker_usdc),
        &(),
    );
    e.must(&[ix], &[]);
    // avg = (684×45 + 900×45)/90 = 792 → pnl = 68 bp × 1,000,000 × 90/365 = 16,767.123 USDC; fee 10 % → 1,676.712.
    let pnl = 68u128 * 1_000_000 * u128::from(USDC) * 90 / (10_000 * 365);
    let fee = pnl / 10;
    let bounty = 25 * u128::from(USDC);
    let payout = 33_000 * u128::from(USDC) + pnl - fee - bounty;
    let trader_after = e.token_amount(&ata(&tr.pubkey(), &USDC_DEVNET));
    assert_eq!(
        u128::from(trader_after - trader_before),
        payout,
        "trader payout net of bounty"
    );
    assert_eq!(
        u128::from(e.token_amount(&cranker_usdc)),
        bounty,
        "cranker bounty"
    );
    assert!(
        e.svm
            .get_account(&e.swap_pda(&tr.pubkey(), 1))
            .map(|a| a.lamports == 0 || a.data.is_empty())
            .unwrap_or(true),
        "swap account closed"
    );
    let p: Pool = e.acct("Pool", &e.pool);
    assert_eq!(u128::from(p.tvl), 10_000_000 * u128::from(USDC) - pnl);
    assert_eq!(p.collateral_held, 0);
    assert_eq!(p.open_swaps, 0);
    assert_eq!(p.util_pay_bp, 0);
    let held = u128::from(p.fees_buyback_accrued) + u128::from(p.fees_treasury_accrued);
    assert_eq!(held, 500 * u128::from(USDC) + fee);
    assert_eq!(
        u128::from(e.token_amount(&e.vault)),
        u128::from(p.tvl) + held
    );

    // Sweep: 50/50 to the two destinations, accruals reset.
    let ix = e.sweep_ix();
    e.must(&[ix], &[]);
    let t = e.treasury_owner.pubkey();
    let tre = e.token_amount(&ata(&t, &USDC_DEVNET));
    let esc = e.token_amount(&ata(&buyback_owner(&t), &USDC_DEVNET));
    assert_eq!(u128::from(tre + esc), 500 * u128::from(USDC) + fee);
    assert!(tre >= esc && tre - esc <= 1);
    let p: Pool = e.acct("Pool", &e.pool);
    assert_eq!(p.fees_buyback_accrued, 0);
    assert_eq!(p.fees_treasury_accrued, 0);
    assert_eq!(e.token_amount(&e.vault), p.tvl + p.collateral_held);
    let g: Global = e.acct("Global", &e.global);
    assert_eq!(
        u128::from(g.buyback_lifetime + g.treasury_lifetime),
        500 * u128::from(USDC) + fee
    );
    let ix = e.sweep_ix();
    e.must_fail(&[ix], &[], "NothingToSweep");

    // LP exits everything with no open swaps: no exit fee, receives tvl less at most one base unit (the virtual
    // offset keeps `VIRTUAL_TVL` of dust with the pool once the price has moved off par).
    let shares = e.mint_supply(&e.share_mint);
    e.withdraw(&lp, shares);
    let p: Pool = e.acct("Pool", &e.pool);
    assert!(
        p.tvl <= 2,
        "dust left by the virtual offset is at most one unit, got {}",
        p.tvl
    );
    assert_eq!(e.token_amount(&e.vault), p.tvl);
    let lp_usdc = u128::from(e.token_amount(&ata(&lp.pubkey(), &USDC_DEVNET)));
    assert!(
        lp_usdc + u128::from(p.tvl) == 20_000_000 * u128::from(USDC) - pnl,
        "LP receives tvl less dust"
    );
}

#[test]
fn limit_rate_caps_and_withdraw_guard() {
    let mut e = setup();
    let lp = e.lp.insecure_clone();
    let tr = e.trader.insecure_clone();
    let ix = e.deposit_ix(&lp.pubkey(), 1_000_000 * USDC, 0);
    e.must(&[ix], &[&lp]);
    // Limit below the quote fails.
    let ix = e.open_ix(
        &tr.pubkey(),
        &open_args(LegKind::PayFixed, 2, 100_000 * USDC, 700, 1),
    );
    if e.send(&[ix], &[&tr]).is_ok() {
        let s: Swap = e.acct("Swap", &e.swap_pda(&tr.pubkey(), 1));
        panic!(
            "open at limit 700 must fail with LimitRate; it was filled at {} bp",
            s.fixed_bp
        );
    }
    // Receive fixed: ref 684 − model 14 − demand 2 (trapezoid, M-3) − term 7 = 661; a limit above it fails, at it
    // passes.
    let ix = e.open_ix(
        &tr.pubkey(),
        &open_args(LegKind::ReceiveFixed, 2, 100_000 * USDC, 662, 2),
    );
    e.must_fail(&[ix], &[&tr], "LimitRate");
    let ix = e.open_ix(
        &tr.pubkey(),
        &open_args(LegKind::ReceiveFixed, 2, 100_000 * USDC, 661, 2),
    );
    e.must(&[ix], &[&tr]);
    // Leg cap: 48 % of 1,000,000 = 480,000 notional; 400,000 more on the receive leg breaches it.
    let ix = e.open_ix(
        &tr.pubkey(),
        &open_args(LegKind::ReceiveFixed, 0, 400_000 * USDC, 0, 3),
    );
    e.must_fail(&[ix], &[&tr], "LegCap");
    // Bad tenor index.
    let ix = e.open_ix(
        &tr.pubkey(),
        &open_args(LegKind::PayFixed, 4, 10_000 * USDC, 9_999, 4),
    );
    e.must_fail(&[ix], &[&tr], "Tenor");
    // Below minimum notional.
    let ix = e.open_ix(
        &tr.pubkey(),
        &open_args(LegKind::PayFixed, 0, 10 * USDC, 9_999, 5),
    );
    e.must_fail(&[ix], &[&tr], "NotionalTooSmall");
    // Duplicate client seed → the swap PDA already exists.
    let ix = e.open_ix(
        &tr.pubkey(),
        &open_args(LegKind::ReceiveFixed, 2, 10_000 * USDC, 0, 2),
    );
    assert!(e.send(&[ix], &[&tr]).is_err());
    // LP cannot withdraw capital that backs open notional on the immediate path: pulling 90 % would push total
    // utilisation past the 70 % withdraw limit, and the program points the LP at the pro-rata queue (ADR-009).
    let shares = e.mint_supply(&e.share_mint);
    let ix = e.withdraw_ix(&lp.pubkey(), shares * 9 / 10, 0);
    e.must_fail(&[ix], &[&lp], "UseWithdrawQueue");
    // A small withdrawal passes and pays the exit fee scaled by utilisation (external scan 1, M-16): 50 bp at a
    // fully used book, here 50 bp × 100,000 / 1,000,000 = 5 bp, so 50 USDC on 100,000.
    let p: Pool = e.acct("Pool", &e.pool);
    let util_bp = (u128::from(p.open_pay_notional + p.open_rec_notional) * 10_000)
        .div_ceil(u128::from(p.tvl));
    assert_eq!(
        util_bp, 1_000,
        "one 100,000 swap against 1,000,000 of capital"
    );
    let before = e.token_amount(&ata(&lp.pubkey(), &USDC_DEVNET));
    let ix = e.withdraw_ix(&lp.pubkey(), 100_000 * USDC * SHARE, 0);
    e.must(&[ix], &[&lp]);
    let after = e.token_amount(&ata(&lp.pubkey(), &USDC_DEVNET));
    // At par the offset price is exact: (tvl + 1) / (supply + 10^3) = 10^-3 while supply = tvl × 10^3.
    assert_eq!(after - before, 100_000 * USDC - 50 * USDC);
    let p: Pool = e.acct("Pool", &e.pool);
    assert_eq!(
        p.fees_buyback_accrued + p.fees_treasury_accrued,
        50 * USDC + 50 * USDC
    ); // exit fee + opening fee on 100,000, held on the pool (F-29)
    assert_eq!(
        e.token_amount(&e.vault),
        p.tvl + p.collateral_held + p.fees_buyback_accrued + p.fees_treasury_accrued
    );
}

#[test]
fn cancel_marks_at_opposite_quote_and_liquidation_rules() {
    let mut e = setup();
    let lp = e.lp.insecure_clone();
    let tr = e.trader.insecure_clone();
    e.deposit(&lp, 10_000_000 * USDC);
    let ix = e.open_ix(
        &tr.pubkey(),
        &open_args(LegKind::PayFixed, 3, 500_000 * USDC, 9_999, 7),
    );
    e.must(&[ix], &[&tr]);
    let s: Swap = e.acct("Swap", &e.swap_pda(&tr.pubkey(), 7));
    // Not liquidatable: far from maturity and the mark loss is small.
    let ix = e.close_ix(
        "crank_liquidate_swap",
        &tr.pubkey(),
        7,
        &e.payer.pubkey(),
        &(),
    );
    e.must_fail(&[ix], &[], "NotLiquidatable");
    // Only the trader may cancel.
    let ix = e.close_ix(
        "trader_cancel_swap",
        &tr.pubkey(),
        7,
        &e.payer.pubkey(),
        &0u64,
    );
    e.must_fail(&[ix], &[], "ConstraintSigner");
    // Trader cancels after 10 days; payout = collateral + mark, mark negative by the bid/ask (unwind at receive quote).
    e.warp(10 * 216_000, 10 * DAY);
    e.publish(684).unwrap();
    let before = e.token_amount(&ata(&tr.pubkey(), &USDC_DEVNET));
    let ix = e.close_ix(
        "trader_cancel_swap",
        &tr.pubkey(),
        7,
        &tr.pubkey(),
        &(s.collateral + 1),
    );
    e.must_fail(&[ix], &[&tr], "Slippage");
    let ix = e.close_ix("trader_cancel_swap", &tr.pubkey(), 7, &tr.pubkey(), &0u64);
    e.must(&[ix], &[&tr]);
    let after = e.token_amount(&ata(&tr.pubkey(), &USDC_DEVNET));
    assert!(
        after > before && after - before < s.collateral,
        "cancel returns collateral less the unwind cost"
    );
    let p: Pool = e.acct("Pool", &e.pool);
    assert_eq!(p.open_swaps, 0);
    assert_eq!(p.collateral_held, 0);
    assert!(p.tvl > 10_000_000 * USDC);
    assert_eq!(
        e.token_amount(&e.vault),
        p.tvl + p.fees_buyback_accrued + p.fees_treasury_accrued
    );

    // No time-based liquidation: three hours before maturity a healthy position is still not liquidatable
    // (the pre-maturity window was removed, review F-11 / ADR-007); it settles after maturity instead.
    let ix = e.open_ix(
        &tr.pubkey(),
        &open_args(LegKind::ReceiveFixed, 0, 200_000 * USDC, 0, 8),
    );
    e.must(&[ix], &[&tr]);
    e.warp(28 * 216_000 - 10, 28 * DAY - 3 * 3_600);
    e.publish(684).unwrap();
    let ix = e.close_ix(
        "crank_liquidate_swap",
        &tr.pubkey(),
        8,
        &e.payer.pubkey(),
        &(),
    );
    e.must_fail(&[ix], &[], "NotLiquidatable");
    e.warp(10, 3 * 3_600);
    let ix = e.close_ix("crank_settle_swap", &tr.pubkey(), 8, &e.payer.pubkey(), &());
    e.must(&[ix], &[]);
    let p: Pool = e.acct("Pool", &e.pool);
    assert_eq!(p.open_swaps, 0);
    assert_eq!(
        e.token_amount(&e.vault),
        p.tvl + p.collateral_held + p.fees_buyback_accrued + p.fees_treasury_accrued
    );
}

#[test]
fn modes_guardian_scope_and_benchmark_guards() {
    let mut e = setup();
    let lp = e.lp.insecure_clone();
    let tr = e.trader.insecure_clone();
    let a = e.authority.insecure_clone();
    let g = e.guardian.insecure_clone();
    let ix = e.deposit_ix(&lp.pubkey(), 1_000_000 * USDC, 0);
    e.must(&[ix], &[&lp]);
    // Guardian can pause entries (WithdrawOnly) but never exits: Halted is reserved for the timelocked authority.
    // It cannot reopen, and a stranger cannot touch the mode.
    assert!(e
        .set_mode(&g, OperatingMode::Halted)
        .unwrap_err()
        .contains("GuardianScope"));
    e.set_mode(&g, OperatingMode::WithdrawOnly).unwrap();
    assert!(e
        .set_mode(&g, OperatingMode::Normal)
        .unwrap_err()
        .contains("GuardianScope"));
    assert!(e.set_mode(&tr, OperatingMode::Limited).is_err());
    // WithdrawOnly admits no immediate exit, an empty book included (external scan 2, finding 17): the LP is
    // pointed at the queue, which fills the whole request once the epoch has run.
    let ix = e.withdraw_ix(&lp.pubkey(), USDC * SHARE, 0);
    e.must_fail(&[ix], &[&lp], "UseWithdrawQueue");
    e.must(&[e.init_queue_ix()], &[]);
    let ix = e.enqueue_ix(&lp.pubkey(), USDC * SHARE, 7);
    e.must(&[ix], &[&lp]);
    e.warp(216_000, DAY);
    e.publish(684).unwrap();
    e.must(&[e.process_ix()], &[]);
    let q: WithdrawQueue = e.acct("WithdrawQueue", &e.queue_pda());
    assert_eq!(
        q.fills[0].shares_filled,
        USDC * SHARE,
        "an empty book fills the whole epoch"
    );
    e.must(&[e.claim_ix(&lp.pubkey(), 7, &lp.pubkey())], &[&lp]);
    e.set_mode(&a, OperatingMode::Halted).unwrap();
    let ix = e.open_ix(
        &tr.pubkey(),
        &open_args(LegKind::PayFixed, 0, 10_000 * USDC, 9_999, 1),
    );
    e.must_fail(&[ix], &[&tr], "Halted");
    let ix = e.deposit_ix(&lp.pubkey(), USDC, 0);
    e.must_fail(&[ix], &[&lp], "Halted");
    let ix = e.withdraw_ix(&lp.pubkey(), USDC * SHARE, 0);
    e.must_fail(&[ix], &[&lp], "Halted");
    // Authority reopens in Limited: notional above the cap fails, below passes.
    e.set_mode(&a, OperatingMode::Limited).unwrap();
    let ix = e.open_ix(
        &tr.pubkey(),
        &open_args(LegKind::PayFixed, 0, 200_000 * USDC, 9_999, 1),
    );
    e.must_fail(&[ix], &[&tr], "LimitedModeCap");
    let ix = e.open_ix(
        &tr.pubkey(),
        &open_args(LegKind::PayFixed, 0, 50_000 * USDC, 9_999, 1),
    );
    e.must(&[ix], &[&tr]);
    // WithdrawOnly: opens and deposits fail; with swaps open the immediate withdrawal path is closed and the
    // queue takes the request (ADR-009), so the LP is redirected rather than refused.
    e.set_mode(&a, OperatingMode::WithdrawOnly).unwrap();
    let ix = e.open_ix(
        &tr.pubkey(),
        &open_args(LegKind::PayFixed, 0, 10_000 * USDC, 9_999, 2),
    );
    e.must_fail(&[ix], &[&tr], "WithdrawOnly");
    let ix = e.withdraw_ix(&lp.pubkey(), 1_000 * USDC * SHARE, 0);
    e.must_fail(&[ix], &[&lp], "UseWithdrawQueue");
    let ix = e.enqueue_ix(&lp.pubkey(), 1_000 * USDC * SHARE, 9);
    e.must(&[ix], &[&lp]);
    e.set_mode(&a, OperatingMode::Normal).unwrap();
    // Benchmark guards (E2 0003): an out-of-band publish is clamped to the band edge and flagged, and the AMM
    // refuses to quote on a flagged value; a stale benchmark stops quoting; an in-band fresh publish restores it.
    e.publish(684 + 301).unwrap();
    let b: Benchmark = e.acct("Benchmark", &e.benchmark);
    assert!(b.clamped, "out-of-band value is flagged");
    let ix = e.open_ix(
        &tr.pubkey(),
        &open_args(LegKind::PayFixed, 0, 10_000 * USDC, 9_999, 3),
    );
    e.must_fail(&[ix], &[&tr], "BenchmarkOutOfBand");
    // Degraded (past max staleness) with the flag standing still reads as out of band; past twice the
    // staleness bound the tier is Stale whatever the flag says.
    e.warp(2_001, 1_000);
    let ix = e.open_ix(
        &tr.pubkey(),
        &open_args(LegKind::PayFixed, 0, 10_000 * USDC, 9_999, 3),
    );
    e.must_fail(&[ix], &[&tr], "BenchmarkOutOfBand");
    e.warp(2_000, 1_000);
    let ix = e.open_ix(
        &tr.pubkey(),
        &open_args(LegKind::PayFixed, 0, 10_000 * USDC, 9_999, 3),
    );
    e.must_fail(&[ix], &[&tr], "BenchmarkStale");
    e.publish(900).unwrap();
    let ix = e.open_ix(
        &tr.pubkey(),
        &open_args(LegKind::PayFixed, 0, 10_000 * USDC, 9_999, 3),
    );
    e.must(&[ix], &[&tr]);
    let b: Benchmark = e.acct("Benchmark", &e.benchmark);
    assert_eq!(b.value_bp, 900);
    assert!(
        b.ema_bp > 684 && b.ema_bp < 900,
        "EMA moves between old and new, got {}",
        b.ema_bp
    );
    assert_eq!(
        b.publish_count, 4,
        "three guard publishes and the one that kept the epoch fresh"
    );
    assert!(!b.clamped, "an in-band publish clears the flag");
    // Settlement is not blocked by staleness (funds must never get stuck): warp past the midnight-aligned
    // maturity (28 days plus the balance of the opening day) without publishing.
    e.warp(29 * 216_000, 29 * DAY);
    let ix = e.close_ix("crank_settle_swap", &tr.pubkey(), 3, &e.payer.pubkey(), &());
    e.must(&[ix], &[]);
}

#[test]
fn calibration_is_timelocked_and_step_limited() {
    let mut e = setup();
    let a = e.authority.insecure_clone();
    let lp = e.lp.insecure_clone();
    let tr = e.trader.insecure_clone();
    let ix = e.deposit_ix(&lp.pubkey(), 1_000_000 * USDC, 0);
    e.must(&[ix], &[&lp]);
    let (global, pool) = (e.global, e.pool);
    let queue = |p: Params| Instruction {
        program_id: SWAP_AMM,
        accounts: [
            vec![ro(global), rw(pool), sig(a.pubkey())],
            evt(SWAP_AMM).to_vec(),
        ]
        .concat(),
        data: data("admin_queue_calibration", &p),
    };
    let mut too_far = DEFAULT_PARAMS;
    too_far.model_pay_bp[0] = 11 * 2; // > 1.5× in one step
    e.must_fail(&[queue(too_far)], &[&a], "CalibrationStep");
    let mut ok = DEFAULT_PARAMS;
    ok.model_pay_bp[0] = 16; // 11 → 16 is within [5, 17]
    e.must(&[queue(ok)], &[&a]);
    // Not yet effective: a quote still uses model_pay 11 → 684 + 11 + demand(100 bp imbalance → round(0.45)=0) + 3 = 698.
    let ix = e.open_ix(
        &tr.pubkey(),
        &open_args(LegKind::PayFixed, 0, 10_000 * USDC, 698, 1),
    );
    e.must(&[ix], &[&tr]);
    let p: Pool = e.acct("Pool", &e.pool);
    assert_eq!(p.params.model_pay_bp[0], 11);
    assert!(p.pending_effective_slot > 0);
    // After the delay the next instruction applies it: 684 + 16 + demand round(45×200/10000)=1 + 3 = 704 (imbalance now 200 bp).
    e.warp(432_000, 432_000 / 2);
    e.publish(684).unwrap();
    let ix = e.open_ix(
        &tr.pubkey(),
        &open_args(LegKind::PayFixed, 0, 10_000 * USDC, 703, 2),
    );
    e.must_fail(&[ix], &[&tr], "LimitRate");
    let ix = e.open_ix(
        &tr.pubkey(),
        &open_args(LegKind::PayFixed, 0, 10_000 * USDC, 704, 2),
    );
    e.must(&[ix], &[&tr]);
    let p: Pool = e.acct("Pool", &e.pool);
    assert_eq!(p.params.model_pay_bp[0], 16);
    assert_eq!(p.pending_effective_slot, 0);
}

#[test]
fn timelock_queue_delay_cancel_execute() {
    let mut e = setup();
    let proposer = e.authority.insecure_clone();
    let guardian = e.guardian.insecure_clone();
    let executor = e.lp.insecure_clone();
    let stranger = e.trader.insecure_clone();
    let timelock = pda(&[b"timelock"], &TIMELOCK);
    let authority = pda(&[b"authority"], &TIMELOCK);
    let payer = e.payer.pubkey();
    let init = |delay: u64| Instruction {
        program_id: TIMELOCK,
        accounts: vec![
            rw(timelock),
            ro(authority),
            sigw(payer),
            ro(program_data(&TIMELOCK)),
            ro(SYSTEM),
        ],
        data: data(
            "initialise",
            &(
                proposer.pubkey(),
                executor.pubkey(),
                guardian.pubkey(),
                delay,
            ),
        ),
    };
    e.must_fail(&[init(1_000)], &[], "DelayOutOfRange");
    e.must(&[init(432_000)], &[]);
    let t: Timelock = e.acct("Timelock", &timelock);
    assert_eq!(t.delay_slots, 432_000);
    assert_eq!(t.nonce, 0);
    let op = |n: u64| pda(&[b"op", &n.to_le_bytes()], &TIMELOCK);
    let queue = |n: u64, k: OperationKind, who: &Keypair| Instruction {
        program_id: TIMELOCK,
        accounts: [
            vec![rw(timelock), sigw(who.pubkey()), rw(op(n)), ro(SYSTEM)],
            evt(TIMELOCK).to_vec(),
        ]
        .concat(),
        data: data("queue", &k),
    };
    // Only the proposer queues; a delay below the floor is rejected at queue time.
    e.must_fail(
        &[queue(
            0,
            OperationKind::SetDelay {
                delay_slots: 500_000,
            },
            &stranger,
        )],
        &[&stranger],
        "ConstraintHasOne",
    );
    e.must_fail(
        &[queue(
            0,
            OperationKind::SetDelay { delay_slots: 1 },
            &proposer,
        )],
        &[&proposer],
        "DelayOutOfRange",
    );
    e.must(
        &[queue(
            0,
            OperationKind::SetDelay {
                delay_slots: 500_000,
            },
            &proposer,
        )],
        &[&proposer],
    );
    let o: Operation = e.acct("Operation", &op(0));
    assert_eq!(o.state, OperationState::Queued);
    assert_eq!(o.eta_slot, o.queued_slot + 432_000);
    let exec = |n: u64, who: &Keypair| Instruction {
        program_id: TIMELOCK,
        accounts: [
            vec![
                rw(timelock),
                sig(who.pubkey()),
                rw(proposer.pubkey()),
                rw(op(n)),
            ],
            evt(TIMELOCK).to_vec(),
        ]
        .concat(),
        data: data("execute_config", &()),
    };
    e.must_fail(&[exec(0, &executor)], &[&executor], "TooEarly");
    e.must_fail(&[exec(0, &stranger)], &[&stranger], "Unauthorised");
    // Guardian can cancel a second operation but cannot execute anything.
    e.must(
        &[queue(
            1,
            OperationKind::SetRoles {
                proposer: proposer.pubkey(),
                executor: executor.pubkey(),
                guardian: stranger.pubkey(),
            },
            &proposer,
        )],
        &[&proposer],
    );
    let cancel = |n: u64, who: &Keypair| Instruction {
        program_id: TIMELOCK,
        accounts: [
            vec![
                ro(timelock),
                sig(who.pubkey()),
                rw(proposer.pubkey()),
                rw(op(n)),
            ],
            evt(TIMELOCK).to_vec(),
        ]
        .concat(),
        data: data("cancel", &()),
    };
    e.must_fail(&[cancel(1, &stranger)], &[&stranger], "Unauthorised");
    e.must(&[cancel(1, &guardian)], &[&guardian]);
    assert!(
        e.svm
            .get_account(&op(1))
            .map(|a| a.data.is_empty())
            .unwrap_or(true),
        "cancelled operation closed"
    );
    e.warp(432_000, 432_000 / 2);
    e.must_fail(&[exec(0, &guardian)], &[&guardian], "Unauthorised");
    e.must(&[exec(0, &executor)], &[&executor]);
    let t: Timelock = e.acct("Timelock", &timelock);
    assert_eq!(t.delay_slots, 500_000);
    // Executed operations are closed and their rent returned to the proposer (E2 0004); a replay finds no account.
    assert!(
        e.svm
            .get_account(&op(0))
            .map(|a| a.data.is_empty())
            .unwrap_or(true),
        "executed operation closed"
    );
    e.must_fail(&[exec(0, &executor)], &[&executor], "AccountNotInitialized");
    // Expiry: an operation left past eta + grace cannot execute.
    e.must(
        &[queue(
            2,
            OperationKind::SetDelay {
                delay_slots: 432_000,
            },
            &proposer,
        )],
        &[&proposer],
    );
    e.warp(500_000 + 1_512_001, 1_000);
    e.must_fail(&[exec(2, &executor)], &[&executor], "Expired");
    // Once expired, anyone may cancel it so the rent returns to the proposer and the ring does not strand.
    e.must(&[cancel(2, &stranger)], &[&stranger]);
    assert!(
        e.svm
            .get_account(&op(2))
            .map(|a| a.data.is_empty())
            .unwrap_or(true),
        "expired operation closed by a stranger"
    );
}

#[test]
fn direct_vault_donation_cannot_brick_the_pool_and_is_folded_into_lp_capital() {
    let mut e = setup();
    let lp = e.lp.insecure_clone();
    let tr = e.trader.insecure_clone();
    let ix = e.deposit_ix(&lp.pubkey(), 1_000_000 * USDC, 0);
    e.must(&[ix], &[&lp]);
    // Hostile or accidental: 1 USDC sent straight to the vault, bypassing the program.
    let v = e.vault;
    e.mint_usdc(&v, USDC);
    // Every instruction still works (the invariant is >=), ...
    let ix = e.open_ix(
        &tr.pubkey(),
        &open_args(LegKind::PayFixed, 0, 10_000 * USDC, 9_999, 1),
    );
    e.must(&[ix], &[&tr]);
    let p: Pool = e.acct("Pool", &e.pool);
    assert_eq!(
        e.token_amount(&e.vault),
        p.tvl + p.collateral_held + p.fees_buyback_accrued + p.fees_treasury_accrued + USDC
    );
    // ... and anyone can fold the surplus into LP capital; a second sync has nothing to do.
    let sync = Instruction {
        program_id: SWAP_AMM,
        accounts: [vec![rw(e.pool), ro(e.vault)], evt(SWAP_AMM).to_vec()].concat(),
        data: data("sync_vault", &()),
    };
    e.must(std::slice::from_ref(&sync), &[]);
    let p2: Pool = e.acct("Pool", &e.pool);
    assert_eq!(p2.tvl, p.tvl + USDC);
    assert_eq!(
        e.token_amount(&e.vault),
        p2.tvl + p2.collateral_held + p2.fees_buyback_accrued + p2.fees_treasury_accrued
    );
    e.must_fail(&[sync], &[], "NothingToSweep");
    // LP share price rose accordingly: withdrawing all shares after the swap closes returns the donation too.
    e.warp(28 * 216_000, 28 * DAY);
    let ix = e.close_ix("crank_settle_swap", &tr.pubkey(), 1, &e.payer.pubkey(), &());
    e.must(&[ix], &[]);
    let shares = e.mint_supply(&e.share_mint);
    e.withdraw(&lp, shares);
    let p3: Pool = e.acct("Pool", &e.pool);
    assert!(p3.tvl <= 2, "dust left by the virtual offset: {}", p3.tvl);
    assert_eq!(
        e.token_amount(&e.vault),
        p3.tvl + p3.fees_buyback_accrued + p3.fees_treasury_accrued
    );
}

// ---------------------------------------------------------------------------------------------------------------
// Audit additions (docs/audit/programs/review-*.md). Two scenario tests and two characterisation tests.
// A characterisation test pins the behaviour the review identified as a defect so the suite stays green and the
// defect cannot change silently; when the fix lands the assertion flips and the finding is closed.
// ---------------------------------------------------------------------------------------------------------------

/// Every operating mode, every instruction: which are rejected and with which error. Settlement, liquidation,
/// `sync_vault` and `sweep_fees` run in every mode; the guardian tightens one step at a time and never loosens.
#[test]
fn operating_mode_transitions_reject_the_right_instructions() {
    let mut e = setup();
    let lp = e.lp.insecure_clone();
    let tr = e.trader.insecure_clone();
    let a = e.authority.insecure_clone();
    let g = e.guardian.insecure_clone();
    let ix = e.deposit_ix(&lp.pubkey(), 2_000_000 * USDC, 0);
    e.must(&[ix], &[&lp]);
    // Three positions opened in Normal: one to cancel, two to settle after maturity.
    let ix = e.open_ix(
        &tr.pubkey(),
        &open_args(LegKind::PayFixed, 0, 50_000 * USDC, 9_999, 1),
    );
    e.must(&[ix], &[&tr]);
    let ix = e.open_ix(
        &tr.pubkey(),
        &open_args(LegKind::PayFixed, 0, 50_000 * USDC, 9_999, 2),
    );
    e.must(&[ix], &[&tr]);
    let ix = e.open_ix(
        &tr.pubkey(),
        &open_args(LegKind::ReceiveFixed, 0, 50_000 * USDC, 0, 3),
    );
    e.must(&[ix], &[&tr]);
    let sync = |e: &Env| Instruction {
        program_id: SWAP_AMM,
        accounts: [vec![rw(e.pool), ro(e.vault)], evt(SWAP_AMM).to_vec()].concat(),
        data: data("sync_vault", &()),
    };

    // Guardian: Normal -> Limited (tighten by one step is allowed), Limited -> Normal rejected.
    e.set_mode(&g, OperatingMode::Limited).unwrap();
    assert!(e
        .set_mode(&g, OperatingMode::Normal)
        .unwrap_err()
        .contains("GuardianScope"));
    assert!(
        e.set_mode(&g, OperatingMode::Limited)
            .unwrap_err()
            .contains("GuardianScope"),
        "same mode is not a tightening"
    );
    // Limited: deposits pass, opens at or below the cap pass, above the cap fail, withdrawals and cancels pass.
    let ix = e.deposit_ix(&lp.pubkey(), USDC, 0);
    e.must(&[ix], &[&lp]);
    let ix = e.open_ix(
        &tr.pubkey(),
        &open_args(LegKind::PayFixed, 0, 100_000 * USDC + 1, 9_999, 4),
    );
    e.must_fail(&[ix], &[&tr], "LimitedModeCap");
    let ix = e.open_ix(
        &tr.pubkey(),
        &open_args(LegKind::PayFixed, 0, 100_000 * USDC, 9_999, 4),
    );
    e.must(&[ix], &[&tr]);
    let ix = e.withdraw_ix(&lp.pubkey(), USDC * SHARE, 0);
    e.must(&[ix], &[&lp]);
    let ix = e.close_ix("trader_cancel_swap", &tr.pubkey(), 4, &tr.pubkey(), &0u64);
    e.must(&[ix], &[&tr]);
    let p_before: Pool = e.acct("Pool", &e.pool);

    // Guardian: Limited -> WithdrawOnly; Halted stays out of reach.
    e.set_mode(&g, OperatingMode::WithdrawOnly).unwrap();
    assert!(e
        .set_mode(&g, OperatingMode::Halted)
        .unwrap_err()
        .contains("GuardianScope"));
    // WithdrawOnly: entries fail, exits pass, cranks pass, sweep and sync pass.
    let ix = e.deposit_ix(&lp.pubkey(), USDC, 0);
    e.must_fail(&[ix], &[&lp], "WithdrawOnly");
    let ix = e.open_ix(
        &tr.pubkey(),
        &open_args(LegKind::PayFixed, 0, 10_000 * USDC, 9_999, 5),
    );
    e.must_fail(&[ix], &[&tr], "WithdrawOnly");
    // Under WithdrawOnly the immediate limit is zero while swaps are open (ADR-009: no first-mover prize), so
    // the LP is pointed at the queue and the queue accepts the request.
    let ix = e.withdraw_ix(&lp.pubkey(), USDC * SHARE, 0);
    e.must_fail(&[ix], &[&lp], "UseWithdrawQueue");
    e.must(&[e.init_queue_ix()], &[]);
    let ix = e.enqueue_ix(&lp.pubkey(), USDC * SHARE, 7);
    e.must(&[ix], &[&lp]);
    let ix = e.close_ix("trader_cancel_swap", &tr.pubkey(), 1, &tr.pubkey(), &0u64);
    e.must(&[ix], &[&tr]);
    let v = e.vault;
    e.mint_usdc(&v, USDC);
    e.must(&[sync(&e)], &[]);

    // Authority: Halted. Everything user-initiated is rejected, including cancel (exits are blocked only here).
    e.set_mode(&a, OperatingMode::Halted).unwrap();
    let ix = e.deposit_ix(&lp.pubkey(), USDC, 0);
    e.must_fail(&[ix], &[&lp], "Halted");
    let ix = e.open_ix(
        &tr.pubkey(),
        &open_args(LegKind::PayFixed, 0, 10_000 * USDC, 9_999, 6),
    );
    e.must_fail(&[ix], &[&tr], "Halted");
    let ix = e.withdraw_ix(&lp.pubkey(), USDC * SHARE, 0);
    e.must_fail(&[ix], &[&lp], "Halted");
    let ix = e.close_ix("trader_cancel_swap", &tr.pubkey(), 2, &tr.pubkey(), &0u64);
    e.must_fail(&[ix], &[&tr], "Halted");
    // Guardian cannot change anything while Halted (nothing is stricter), authority can loosen.
    assert!(e
        .set_mode(&g, OperatingMode::WithdrawOnly)
        .unwrap_err()
        .contains("GuardianScope"));
    // Cranks still run while Halted: the liquidation check is evaluated (and rejects a healthy position, since
    // there is no time-based trigger), then settlement after maturity closes both positions.
    e.warp(28 * 216_000 - 10, 28 * DAY - 3 * 3_600);
    e.publish(684).unwrap();
    let ix = e.close_ix(
        "crank_liquidate_swap",
        &tr.pubkey(),
        3,
        &e.payer.pubkey(),
        &(),
    );
    e.must_fail(&[ix], &[], "NotLiquidatable");
    e.warp(10, 3 * 3_600);
    let ix = e.close_ix("crank_settle_swap", &tr.pubkey(), 2, &e.payer.pubkey(), &());
    e.must(&[ix], &[]);
    let ix = e.close_ix("crank_settle_swap", &tr.pubkey(), 3, &e.payer.pubkey(), &());
    e.must(&[ix], &[]);
    let v = e.vault;
    e.mint_usdc(&v, USDC);
    e.must(&[sync(&e)], &[]);
    let ix = e.sweep_ix();
    e.must(&[ix], &[]);
    let p_after: Pool = e.acct("Pool", &e.pool);
    assert!(
        p_before.fees_buyback_accrued > 0 && p_after.fees_buyback_accrued == 0,
        "opening fees accrued earlier are swept while Halted"
    );
    let ix = e.sweep_ix();
    e.must_fail(&[ix], &[], "NothingToSweep");
    let p: Pool = e.acct("Pool", &e.pool);
    assert_eq!(p.open_swaps, 0);
    assert_eq!(p.collateral_held, 0);
    assert_eq!(e.token_amount(&e.vault), p.tvl);
    // Authority reopens; a stranger still cannot.
    assert!(e.set_mode(&tr, OperatingMode::Normal).is_err());
    e.set_mode(&a, OperatingMode::Normal).unwrap();
    let ix = e.deposit_ix(&lp.pubkey(), USDC, 0);
    e.must(&[ix], &[&lp]);
}

/// Settlement at the collateral cap on both legs: a pay-fixed trader's gain and a receive-fixed trader's loss are
/// both clamped to the posted collateral, the income fee is 10 percent of the clamped gain, and the pool ends
/// where it started (the two clamps net to zero) while the fee vault holds the fee.
#[test]
fn settlement_is_clamped_at_the_collateral_cap_on_both_legs() {
    let mut e = setup();
    let lp = e.lp.insecure_clone();
    let tr = e.trader.insecure_clone();
    e.deposit(&lp, 10_000_000 * USDC);
    let notional = 100_000 * USDC;
    // 28-day tenor, collateral 120 bp = 1,200 USDC per leg. Opening fee 5 bp = 50 USDC per leg.
    let ix = e.open_ix(
        &tr.pubkey(),
        &open_args(LegKind::PayFixed, 0, notional, 9_999, 1),
    );
    e.must(&[ix], &[&tr]);
    let ix = e.open_ix(
        &tr.pubkey(),
        &open_args(LegKind::ReceiveFixed, 0, notional, 0, 2),
    );
    e.must(&[ix], &[&tr]);
    let pay: Swap = e.acct("Swap", &e.swap_pda(&tr.pubkey(), 1));
    let rec: Swap = e.acct("Swap", &e.swap_pda(&tr.pubkey(), 2));
    let c = pay.collateral;
    assert_eq!(c, 1_200 * USDC);
    assert_eq!(rec.collateral, c);
    let p0: Pool = e.acct("Pool", &e.pool);
    let tvl0 = p0.tvl;
    let fees0 = p0.fees_buyback_accrued + p0.fees_treasury_accrued;
    let trader0 = e.token_amount(&ata(&tr.pubkey(), &USDC_DEVNET));
    // Rates ramp 300 bp a day (the band) for eight days, then hold. The average over 28 days ends far above
    // the pay-fixed rate: the gain exceeds 1.2 percent of notional, so both legs clamp.
    let mut b: Benchmark = e.acct("Benchmark", &e.benchmark);
    for _ in 0..8 {
        e.warp(216_000, DAY);
        e.publish(b.ema_bp + 300).unwrap();
        b = e.acct("Benchmark", &e.benchmark);
    }
    e.warp(20 * 216_000, 20 * DAY);
    e.publish(b.value_bp).unwrap();
    let b: Benchmark = e.acct("Benchmark", &e.benchmark);
    // Unclamped expectation from the index itself, to prove the clamp is binding rather than coincidental.
    let avg =
        (b.accrual_e18 - pay.index_accrual_start) / (1_000_000_000_000_000_000u128 * 28 * 86_400);
    let unclamped = (avg - u128::from(pay.fixed_bp)) * u128::from(notional) * 28 / (10_000 * 365);
    assert!(
        unclamped > u128::from(c),
        "average {avg} bp must push the pay-fixed gain past collateral: {unclamped} vs {c}"
    );
    // Trader cranks both of their own positions: no bounty.
    let ix = e.close_ix("crank_settle_swap", &tr.pubkey(), 1, &tr.pubkey(), &());
    e.must(&[ix], &[&tr]);
    let ix = e.close_ix("crank_settle_swap", &tr.pubkey(), 2, &tr.pubkey(), &());
    e.must(&[ix], &[&tr]);
    let trader1 = e.token_amount(&ata(&tr.pubkey(), &USDC_DEVNET));
    let fee = c / 10;
    // Pay leg: collateral back + clamped gain (= collateral) - 10 percent income fee. Receive leg: nothing back.
    assert_eq!(
        trader1 - trader0,
        c + c - fee,
        "pay-fixed payout is 2c - fee, receive-fixed payout is zero"
    );
    let p1: Pool = e.acct("Pool", &e.pool);
    assert_eq!(p1.tvl, tvl0, "the two clamps net to zero for LPs");
    assert_eq!(p1.collateral_held, 0);
    assert_eq!(p1.open_swaps, 0);
    let fees1 = p1.fees_buyback_accrued + p1.fees_treasury_accrued;
    assert_eq!(fees1 - fees0, fee);
    assert_eq!(
        e.token_amount(&e.vault),
        p1.tvl + p1.collateral_held + fees1
    );
}

/// Regression for review finding F-11 (ADR-005, ADR-007). The pre-maturity liquidation window is gone, so a
/// stranger cannot close an in-the-money position three hours before maturity; and `trader_cancel_swap` pays the
/// accrued leg plus the remaining-days mark, not the mark alone. Pay fixed 1,000,000 USDC for 90 days at about
/// 727 bp while the floating leg sits at 800 bp: after 60 days the accrued gain is about 73 bp x 60/365 of notional
/// (about 12,000 USDC), and the forward leg at the receive quote adds a few thousand more.
#[test]
fn f11_cancel_pays_accrued_plus_mark_and_liquidation_has_no_time_window() {
    let mut e = setup();
    let lp = e.lp.insecure_clone();
    let tr = e.trader.insecure_clone();
    e.deposit(&lp, 10_000_000 * USDC);
    let ix = e.open_ix(
        &tr.pubkey(),
        &open_args(LegKind::PayFixed, 2, 1_000_000 * USDC, 9_999, 1),
    );
    e.must(&[ix], &[&tr]);
    let s: Swap = e.acct("Swap", &e.swap_pda(&tr.pubkey(), 1));
    // Floating leg steps to 800 bp (inside the band) one day in and stays there.
    e.warp(216_000, DAY);
    e.publish(800).unwrap();
    e.warp(59 * 216_000, 59 * DAY);
    e.publish(800).unwrap();
    let b: Benchmark = e.acct("Benchmark", &e.benchmark);
    let elapsed: u128 = 60 * 86_400;
    let avg = (b.accrual_e18 - s.index_accrual_start) / (1_000_000_000_000_000_000u128 * elapsed);
    assert!(
        avg > u128::from(s.fixed_bp),
        "the trader is in the money on the accrued leg"
    );
    let accrued =
        (avg - u128::from(s.fixed_bp)) * u128::from(s.notional) * elapsed / (10_000 * 365 * 86_400);
    let before = e.token_amount(&ata(&tr.pubkey(), &USDC_DEVNET));
    let tvl_before: u64 = e.acct::<Pool>("Pool", &e.pool).tvl;
    let ix = e.close_ix("trader_cancel_swap", &tr.pubkey(), 1, &tr.pubkey(), &0u64);
    e.must(&[ix], &[&tr]);
    let payout = u128::from(e.token_amount(&ata(&tr.pubkey(), &USDC_DEVNET)) - before);
    let c = u128::from(s.collateral);
    // Accrued gain is paid (less the 10 percent income fee on the whole positive value); the forward leg at the
    // receive quote (below 800) adds to it; everything stays within the collateral clamp.
    assert!(
        payout >= c + accrued * 9 / 10,
        "cancel payout {payout} must include the accrued gain {accrued} on collateral {c}"
    );
    assert!(
        payout <= 2 * c,
        "payout is clamped at collateral plus collateral"
    );
    let tvl_after: u64 = e.acct::<Pool>("Pool", &e.pool).tvl;
    assert!(
        tvl_after < tvl_before,
        "LPs fund the trader's gain ({tvl_before} -> {tvl_after})"
    );

    // Second position: three hours before maturity a stranger cannot liquidate a healthy position.
    let ix = e.open_ix(
        &tr.pubkey(),
        &open_args(LegKind::PayFixed, 0, 100_000 * USDC, 9_999, 2),
    );
    e.must(&[ix], &[&tr]);
    e.warp(28 * 216_000 - 10, 28 * DAY - 3 * 3_600);
    e.publish(800).unwrap();
    let ix = e.close_ix(
        "crank_liquidate_swap",
        &tr.pubkey(),
        2,
        &e.payer.pubkey(),
        &(),
    );
    e.must_fail(&[ix], &[], "NotLiquidatable");
    // Cancel in the final hours values the whole accrued leg (the forward leg is three hours of mark). This
    // position was quoted above the flat 800 bp floating rate, so the accrued leg is a small loss and the payout
    // is collateral minus that loss minus the three-hour unwind, to within a few USDC.
    let s2: Swap = e.acct("Swap", &e.swap_pda(&tr.pubkey(), 2));
    let b: Benchmark = e.acct("Benchmark", &e.benchmark);
    let elapsed2: u128 = 28 * 86_400 - 3 * 3_600;
    let avg2 =
        (b.accrual_e18 - s2.index_accrual_start) / (1_000_000_000_000_000_000u128 * elapsed2);
    assert!(avg2 < u128::from(s2.fixed_bp));
    let accrued_loss = (u128::from(s2.fixed_bp) - avg2) * u128::from(s2.notional) * elapsed2
        / (10_000 * 365 * 86_400);
    let before = e.token_amount(&ata(&tr.pubkey(), &USDC_DEVNET));
    let ix = e.close_ix("trader_cancel_swap", &tr.pubkey(), 2, &tr.pubkey(), &0u64);
    e.must(&[ix], &[&tr]);
    let payout = u128::from(e.token_amount(&ata(&tr.pubkey(), &USDC_DEVNET)) - before);
    let charged = u128::from(s2.collateral) - payout;
    assert!(charged >= accrued_loss && charged <= accrued_loss + 5 * u128::from(USDC),
        "near-maturity cancel charges the accrued loss {accrued_loss} plus a three-hour unwind: charged {charged}");
}

/// Regression for review finding F-11, loss side: a receive-fixed trader whose accrued leg is losing cannot shed
/// the loss by cancelling; the payout is collateral minus the accrued loss (and minus the forward unwind).
#[test]
fn f11_cancel_charges_accrued_loss() {
    let mut e = setup();
    let lp = e.lp.insecure_clone();
    let tr = e.trader.insecure_clone();
    e.deposit(&lp, 10_000_000 * USDC);
    let ix = e.open_ix(
        &tr.pubkey(),
        &open_args(LegKind::ReceiveFixed, 2, 1_000_000 * USDC, 0, 1),
    );
    e.must(&[ix], &[&tr]);
    let s: Swap = e.acct("Swap", &e.swap_pda(&tr.pubkey(), 1));
    e.warp(216_000, DAY);
    e.publish(800).unwrap();
    e.warp(59 * 216_000, 59 * DAY);
    e.publish(800).unwrap();
    let b: Benchmark = e.acct("Benchmark", &e.benchmark);
    let elapsed: u128 = 60 * 86_400;
    let avg = (b.accrual_e18 - s.index_accrual_start) / (1_000_000_000_000_000_000u128 * elapsed);
    let accrued_loss =
        (avg - u128::from(s.fixed_bp)) * u128::from(s.notional) * elapsed / (10_000 * 365 * 86_400);
    let before = e.token_amount(&ata(&tr.pubkey(), &USDC_DEVNET));
    let ix = e.close_ix("trader_cancel_swap", &tr.pubkey(), 1, &tr.pubkey(), &0u64);
    e.must(&[ix], &[&tr]);
    let payout = u128::from(e.token_amount(&ata(&tr.pubkey(), &USDC_DEVNET)) - before);
    assert!(
        payout + accrued_loss <= u128::from(s.collateral),
        "cancel payout {payout} must charge the accrued loss {accrued_loss}"
    );
}

/// Regression for review finding F-17 (ADR-011): `admin_set_authority` needs the incoming authority's signature,
/// and the outgoing authority loses its powers immediately.
#[test]
fn f17_set_authority_requires_the_new_authority_to_co_sign() {
    let mut e = setup();
    let a = e.authority.insecure_clone();
    let g = e.guardian.insecure_clone();
    let incoming = Keypair::new();
    let g2 = Keypair::new();
    let ix = |e: &Env, signer: &Pubkey, new: &Pubkey, guardian: &Pubkey| Instruction {
        program_id: SWAP_AMM,
        accounts: vec![rw(e.global), sig(*signer), sig(*new)],
        data: data("admin_set_authority", guardian),
    };
    // Without the incoming signature the transaction cannot even be signed; with the wrong current authority it fails.
    let i = ix(&e, &g.pubkey(), &incoming.pubkey(), &g2.pubkey());
    e.must_fail(&[i], &[&g, &incoming], "ConstraintHasOne");
    // Incoming authority equal to the guardian is rejected.
    let i = ix(&e, &a.pubkey(), &incoming.pubkey(), &incoming.pubkey());
    e.must_fail(&[i], &[&a, &incoming], "GuardianScope");
    let i = ix(&e, &a.pubkey(), &incoming.pubkey(), &g2.pubkey());
    e.must(&[i], &[&a, &incoming]);
    let gl: Global = e.acct("Global", &e.global);
    assert_eq!(gl.authority, incoming.pubkey());
    assert_eq!(gl.guardian, g2.pubkey());
    assert!(
        e.set_mode(&a, OperatingMode::Halted).is_err(),
        "the outgoing authority has no power"
    );
    assert!(
        e.set_mode(&g, OperatingMode::WithdrawOnly).is_err(),
        "the outgoing guardian has no power"
    );
    e.set_mode(&incoming, OperatingMode::Halted).unwrap();
    e.set_mode(&incoming, OperatingMode::Normal).unwrap();
}

/// Review F-29 (ADR-001): `lp_deposit` takes `Global` read-only. A deposit submitted with `Global` marked
/// writable still succeeds (Anchor does not reject a superset of privileges) and one with `Global` read-only
/// succeeds too; this test pins the read-only declaration so the hot path takes no write lock on it.
#[test]
fn f29_lp_deposit_does_not_write_global() {
    let mut e = setup();
    let lp = e.lp.insecure_clone();
    let ix = e.deposit_ix(&lp.pubkey(), 1_000 * USDC, 0);
    let global = e.global;
    let meta = ix
        .accounts
        .iter()
        .find(|m| m.pubkey == global)
        .expect("global is passed");
    assert!(
        !meta.is_writable,
        "the harness marks Global read-only for lp_deposit"
    );
    e.must(&[ix], &[&lp]);
}

/// Regression for review finding F-12 / maths finding M-2 (patch 0002). `accrual_at` reads back inside the
/// latest accrual segment from `prev_value_bp` and `prev_unix_ts`, so one publish after maturity no longer inflates
/// the settlement average. The rate is flat at 684 bp for the whole life of the swap, the fixed rate is above 684,
/// nobody cranks for 28 days after maturity, the publisher republishes the same flat rate, and the trader settles
/// at exactly the fair small loss. This test pinned the inflated payout (`payout > collateral`) before the patch.
#[test]
fn f12_single_post_maturity_publish_does_not_inflate_the_average() {
    let mut e = setup();
    let lp = e.lp.insecure_clone();
    let tr = e.trader.insecure_clone();
    e.deposit(&lp, 10_000_000 * USDC);
    let ix = e.open_ix(
        &tr.pubkey(),
        &open_args(LegKind::PayFixed, 0, 100_000 * USDC, 9_999, 1),
    );
    e.must(&[ix], &[&tr]);
    let s: Swap = e.acct("Swap", &e.swap_pda(&tr.pubkey(), 1));
    assert!(s.fixed_bp > 684, "pay-fixed is quoted above the flat 684 bp floating rate, so a fair settlement is a small loss");
    e.warp(56 * 216_000, 56 * DAY);
    e.publish(684).unwrap();
    let before = e.token_amount(&ata(&tr.pubkey(), &USDC_DEVNET));
    let ix = e.close_ix("crank_settle_swap", &tr.pubkey(), 1, &tr.pubkey(), &());
    e.must(&[ix], &[&tr]);
    let payout = e.token_amount(&ata(&tr.pubkey(), &USDC_DEVNET)) - before;
    let fair_loss = u128::from(s.fixed_bp - 684) * u128::from(s.notional) * 28 / (10_000 * 365);
    assert_eq!(
        u128::from(s.collateral) - u128::from(payout),
        fair_loss,
        "settlement after a post-maturity publish charges exactly the flat-rate loss"
    );
}

/// Regression for maths finding M-2 (patch 0002), the nine-day late publish from the maths review: a 90-day
/// swap on a constant 684 bp index whose only publish after open lands nine days after maturity settles at an
/// average of exactly 684 bp (the review measured 752 bp before the patch).
#[test]
fn m2_nine_day_late_publish_settles_at_the_flat_rate() {
    let mut e = setup();
    let lp = e.lp.insecure_clone();
    let tr = e.trader.insecure_clone();
    e.deposit(&lp, 10_000_000 * USDC);
    // Publish one minute before open so the segment boundary is not the open itself.
    e.warp(150, 60);
    e.publish(684).unwrap();
    e.warp(150, 60);
    let ix = e.open_ix(
        &tr.pubkey(),
        &open_args(LegKind::ReceiveFixed, 2, 1_000_000 * USDC, 0, 1),
    );
    e.must(&[ix], &[&tr]);
    let s: Swap = e.acct("Swap", &e.swap_pda(&tr.pubkey(), 1));
    e.warp(99 * 216_000, 99 * DAY);
    e.publish(684).unwrap();
    let b: Benchmark = e.acct("Benchmark", &e.benchmark);
    // The benchmark now carries the previous segment, so the accrual at maturity is recoverable exactly.
    let scale = 1_000_000_000_000_000_000u128;
    let back = u128::try_from(b.unix_ts - s.matures_ts).unwrap();
    let end = b.accrual_e18 - u128::from(b.prev_value_bp) * back * scale;
    // Maturity is aligned up to midnight UTC (E1 0002), so the term is measured from the swap's own stamps.
    let term = u128::try_from(s.matures_ts - s.opened_ts).unwrap();
    let avg = (end - s.index_accrual_start) / (scale * term);
    assert_eq!(avg, 684, "average over the term is the flat rate");
    // And the fixings ring carries the same accrual at the maturity midnight.
    let day = u32::try_from(s.matures_ts / 86_400).unwrap();
    assert_eq!(
        b.fixing(day),
        end,
        "fixing at maturity equals the reconstructed accrual"
    );
    let before = e.token_amount(&ata(&tr.pubkey(), &USDC_DEVNET));
    let ix = e.close_ix("crank_settle_swap", &tr.pubkey(), 1, &tr.pubkey(), &());
    e.must(&[ix], &[&tr]);
    let payout = e.token_amount(&ata(&tr.pubkey(), &USDC_DEVNET)) - before;
    // Receive-fixed below 684: the trader's loss is (684 - fixed) over the aligned term, no inflation either way.
    let fair_loss =
        u128::from(684 - s.fixed_bp) * u128::from(s.notional) * term / (10_000 * 365 * 86_400);
    let loss = u128::from(s.collateral) - u128::from(payout);
    assert!(
        loss.abs_diff(fair_loss) <= 1,
        "loss {loss} against the fair loss {fair_loss}"
    );
}

// ---------------------------------------------------------------------------------------------------------------
// Acceptance tests for the larger designs (ADR-006, ADR-008, ADR-009, ADR-010, ADR-003, ADR-001). Each is
// ignored until the design is implemented; the invariant each one encodes is stated in its comment and asserted
// in its body against the current instruction set where that is expressible. Run with `cargo test -- --ignored`.
// ---------------------------------------------------------------------------------------------------------------

/// ADR-006 (review F-12). Invariant: the settlement value of a swap is a function of the published path up to
/// maturity only; a publish after maturity, and the slot at which the crank lands, change nothing. Green since
/// patch 0002 for one post-maturity publish; the two-publish case is `finding_f12_*` in `red.rs`. Here the rate
/// is flat at 684 bp for the whole life of a 28-day pay-fixed swap quoted above 684 bp, and settlement happens
/// after a post-maturity publish: a fair settlement pays strictly less than collateral.
#[test]
fn adr006_settlement_is_independent_of_crank_timing() {
    let mut e = setup();
    let lp = e.lp.insecure_clone();
    let tr = e.trader.insecure_clone();
    e.deposit(&lp, 10_000_000 * USDC);
    let ix = e.open_ix(
        &tr.pubkey(),
        &open_args(LegKind::PayFixed, 0, 100_000 * USDC, 9_999, 1),
    );
    e.must(&[ix], &[&tr]);
    let s: Swap = e.acct("Swap", &e.swap_pda(&tr.pubkey(), 1));
    e.warp(56 * 216_000, 56 * DAY);
    e.publish(684).unwrap();
    let before = e.token_amount(&ata(&tr.pubkey(), &USDC_DEVNET));
    let ix = e.close_ix("crank_settle_swap", &tr.pubkey(), 1, &tr.pubkey(), &());
    e.must(&[ix], &[&tr]);
    let payout = e.token_amount(&ata(&tr.pubkey(), &USDC_DEVNET)) - before;
    assert!(
        payout < s.collateral,
        "flat 684 bp against a fixed rate of {} bp must settle at a loss, paid {payout} on {}",
        s.fixed_bp,
        s.collateral
    );
}

/// ADR-010. Invariant: a single publisher key cannot move the effective benchmark value; with a quorum of three,
/// one out-of-band submission leaves the median unchanged, and the band clamps instead of rejecting so a
/// legitimate regime shift never stalls publication (`publish` must not fail with `BenchmarkOutOfBand` for the
/// median path, and readers see a `clamped` or degraded flag instead).
#[test]
fn adr010_single_publisher_cannot_move_the_median_and_band_never_deadlocks() {
    let mut e = setup();
    // Today one key publishes and a value more than band_bp from the EMA is rejected outright.
    let b: Benchmark = e.acct("Benchmark", &e.benchmark);
    e.warp(216_000, DAY);
    let r = e.publish(b.ema_bp + b.band_bp + 1);
    assert!(
        r.is_ok(),
        "a quorum median outside the band must be clamped and flagged, not rejected: {r:?}"
    );
}

/// Regression for review finding F-16 / maths findings M-8 and M-9 (ADR-008 item 3, E1 0004 after F-36). With the
/// virtual offset (`VIRTUAL_SHARES = 10^3`, `VIRTUAL_TVL = 1`) and a 9-decimal share mint, a first depositor who
/// deposits one unit, donates 10,000 USDC to the vault and calls `sync_vault` cannot make a later depositor's
/// shares floor to zero, and the later depositor's rounding loss is bounded by donation / 10^3 (ten USDC here on
/// a 20,000 USDC deposit), so the attack costs the attacker a thousand times what it earns. Also covers the wiped
/// pool (tvl 0, supply > 0) from M-8.
#[test]
fn f16_inflation_attempt_with_the_virtual_offset_is_bounded() {
    let mut e = setup();
    let lp = e.lp.insecure_clone();
    let ix = e.deposit_ix(&lp.pubkey(), 1, 0);
    e.must(&[ix], &[&lp]);
    assert_eq!(
        e.mint_supply(&e.share_mint),
        SHARE,
        "one unit mints 10^3 shares"
    );
    let donation = 10_000 * USDC;
    let v = e.vault;
    e.mint_usdc(&v, donation);
    e.must(&[e.sync_ix()], &[]);
    let p: Pool = e.acct("Pool", &e.pool);
    assert_eq!(p.tvl, donation + 1);
    // Victim: 19,999.999999 USDC, the amount the maths review used to show a 25 percent loss before the patch.
    let victim_amount = 19_999_999_999u64;
    let victim = e.new_actor(victim_amount, true);
    let ix = e.deposit_ix(&victim.pubkey(), victim_amount, 1);
    e.must(&[ix], &[&victim]);
    let shares = e.token_amount(&ata(&victim.pubkey(), &e.share_mint));
    assert!(
        shares >= 1_000,
        "victim receives a meaningful number of shares: {shares}"
    );
    let before = e.token_amount(&ata(&victim.pubkey(), &USDC_DEVNET));
    let ix = e.withdraw_ix(&victim.pubkey(), shares, 0);
    e.must(&[ix], &[&victim]);
    let back = e.token_amount(&ata(&victim.pubkey(), &USDC_DEVNET)) - before;
    let loss = victim_amount - back;
    assert!(
        loss <= donation / SHARE + 1,
        "victim loss {loss} must be bounded by donation / 10^3 = {}",
        donation / SHARE
    );
    // The attacker cannot redeem more than what they put in plus the victim's bounded rounding loss.
    let attacker_shares = e.token_amount(&ata(&lp.pubkey(), &e.share_mint));
    let before = e.token_amount(&ata(&lp.pubkey(), &USDC_DEVNET));
    let ix = e.withdraw_ix(&lp.pubkey(), attacker_shares, 0);
    e.must(&[ix], &[&lp]);
    let attacker_back = e.token_amount(&ata(&lp.pubkey(), &USDC_DEVNET)) - before;
    assert!(
        attacker_back <= donation + 1 + loss,
        "attacker redeems {attacker_back} against {donation} donated plus the victim's loss {loss}"
    );
    // After both exit, whatever the virtual shares own stays in the pool (the trapped part of the donation);
    // real supply is zero and the next depositor is priced sanely against it (M-8).
    let p: Pool = e.acct("Pool", &e.pool);
    assert!(
        p.tvl <= donation + 2,
        "only donated capital can remain: {}",
        p.tvl
    );
    assert_eq!(e.mint_supply(&e.share_mint), 0, "no real shares remain");
    let fresh = e.new_actor(1_000 * USDC, true);
    let ix = e.deposit_ix(&fresh.pubkey(), 1_000 * USDC, 0);
    e.must(&[ix], &[&fresh]);
    let fresh_shares = e.token_amount(&ata(&fresh.pubkey(), &e.share_mint));
    let before = e.token_amount(&ata(&fresh.pubkey(), &USDC_DEVNET));
    let ix = e.withdraw_ix(&fresh.pubkey(), fresh_shares, 0);
    e.must(&[ix], &[&fresh]);
    let back = e.token_amount(&ata(&fresh.pubkey(), &USDC_DEVNET)) - before;
    // Rounding against the trapped capital is at most one virtual-share quantum (trapped / 10^6, here under
    // 0.01 USDC): round-02 test observation T-3.
    assert!(
        back + donation / SHARE + 1 >= 1_000 * USDC,
        "a deposit into the leftover pool is recoverable to within the share quantum: {back}"
    );
}

/// ADR-008 (review F-15, maths M-12). Invariant: the LP share price used by `lp_withdraw` deducts the pool's
/// clamped SOAP liability, so an LP cannot exit at par while the book is out of the money for the pool. The
/// inflation half of this acceptance test is green since patch 0005 and lives in `f16_*` above.
#[test]
fn adr008_share_price_marks_open_book_and_resists_inflation() {
    let mut e = setup();
    let lp = e.lp.insecure_clone();
    let tr = e.trader.insecure_clone();
    // Open a position deep in the money for the trader, then withdraw as LP: the LP must receive less than
    // tvl / supply times shares.
    let ix = e.deposit_ix(&lp.pubkey(), 1_000_000 * USDC, 0);
    e.must(&[ix], &[&lp]);
    let ix = e.open_ix(
        &tr.pubkey(),
        &open_args(LegKind::PayFixed, 2, 400_000 * USDC, 9_999, 1),
    );
    e.must(&[ix], &[&tr]);
    e.warp(216_000, DAY);
    e.publish(900).unwrap();
    e.warp(30 * 216_000, 30 * DAY);
    e.publish(900).unwrap();
    let p: Pool = e.acct("Pool", &e.pool);
    // A tenth of the shares would take the pay leg from 40 % to over 44 % of TVL, past the 42 % per-leg
    // immediate limit (external scan 2, finding 11): that exit is pointed at the queue. A twenty-fifth keeps
    // the leg under the limit and goes through at once.
    let tenth = e.token_amount(&ata(&lp.pubkey(), &e.share_mint)) / 10;
    let ix = e.withdraw_ix(&lp.pubkey(), tenth, 0);
    e.must_fail(&[ix], &[&lp], "UseWithdrawQueue");
    let shares = e.token_amount(&ata(&lp.pubkey(), &e.share_mint)) / 25;
    let supply = e.mint_supply(&e.share_mint);
    let par = u128::from(shares) * u128::from(p.tvl) / u128::from(supply);
    let before = e.token_amount(&ata(&lp.pubkey(), &USDC_DEVNET));
    let ix = e.withdraw_ix(&lp.pubkey(), shares, 0);
    e.must(&[ix], &[&lp]);
    let got = u128::from(e.token_amount(&ata(&lp.pubkey(), &USDC_DEVNET)) - before);
    // The exit fee scales with utilisation (M-16), so the bound is par less the scaled fee.
    let util_bp = (u128::from(p.open_pay_notional + p.open_rec_notional) * 10_000)
        .div_ceil(u128::from(p.tvl));
    let fee = par * 50 * util_bp / 100_000_000;
    assert!(
        got < par - fee,
        "withdrawal {got} must price in the open book's liability, par less fee is {}",
        par - fee
    );
}

/// ADR-009. Invariant: under `WithdrawOnly`, two LPs holding equal shares who both request exit in the same epoch
/// receive equal fills; a withdrawal that would breach the utilisation caps is queued (`UseWithdrawQueue`) rather
/// than failing with `PoolInvariant`, so there is no first-mover prize.
#[test]
fn adr009_withdrawals_under_caps_are_pro_rata_not_first_come() {
    let mut e = setup();
    let lp = e.lp.insecure_clone();
    let tr = e.trader.insecure_clone();
    let g = e.guardian.insecure_clone();
    let lp2 = e.new_actor(1_000_000 * USDC, true);
    let ix = e.deposit_ix(&lp.pubkey(), 1_000_000 * USDC, 0);
    e.must(&[ix], &[&lp]);
    let ix = e.deposit_ix(&lp2.pubkey(), 1_000_000 * USDC, 0);
    e.must(&[ix], &[&lp2]);
    // Half the book is open (600k pay, 400k receive on 2M), so the caps pin 1.25M of TVL and leave 750k of
    // capacity; then entries are paused.
    let ix = e.open_ix(
        &tr.pubkey(),
        &open_args(LegKind::PayFixed, 0, 600_000 * USDC, 9_999, 1),
    );
    e.must(&[ix], &[&tr]);
    let ix = e.open_ix(
        &tr.pubkey(),
        &open_args(LegKind::ReceiveFixed, 0, 400_000 * USDC, 0, 2),
    );
    e.must(&[ix], &[&tr]);
    e.set_mode(&g, OperatingMode::WithdrawOnly).unwrap();
    let shares = e.token_amount(&ata(&lp.pubkey(), &e.share_mint));
    assert_eq!(shares, e.token_amount(&ata(&lp2.pubkey(), &e.share_mint)));
    let b1 = e.token_amount(&ata(&lp.pubkey(), &USDC_DEVNET));
    let b2 = e.token_amount(&ata(&lp2.pubkey(), &USDC_DEVNET));
    // Neither can take the immediate path: both are pointed at the queue, in either order.
    let ix = e.withdraw_ix(&lp.pubkey(), shares, 0);
    e.must_fail(&[ix], &[&lp], "UseWithdrawQueue");
    let ix = e.withdraw_ix(&lp2.pubkey(), shares, 0);
    e.must_fail(&[ix], &[&lp2], "UseWithdrawQueue");
    e.must(&[e.init_queue_ix()], &[]);
    let ix = e.enqueue_ix(&lp.pubkey(), shares, 1);
    e.must(&[ix], &[&lp]);
    e.warp(1_000, 400);
    let ix = e.enqueue_ix(&lp2.pubkey(), shares, 1);
    e.must(&[ix], &[&lp2]);
    let q: WithdrawQueue = e.acct("WithdrawQueue", &e.queue_pda());
    assert_eq!(q.queued_shares, 2 * shares);
    assert_eq!(
        e.token_amount(&e.escrow_pda()),
        2 * shares,
        "escrowed, not burnt"
    );
    // The epoch cannot be processed early; after the epoch it fills what the caps allow.
    e.must_fail(&[e.process_ix()], &[], "EpochNotElapsed");
    e.warp(216_000, DAY);
    e.must(&[e.process_ix()], &[]);
    let q: WithdrawQueue = e.acct("WithdrawQueue", &e.queue_pda());
    assert_eq!(q.epoch, 1);
    assert_eq!(q.queued_shares, 0);
    let fill = q.fills[0];
    assert_eq!(fill.shares_queued, 2 * shares);
    assert!(fill.shares_filled > 0, "the caps leave something to fill");
    assert!(
        fill.shares_filled < 2 * shares,
        "the open book keeps the rest in the pool"
    );
    assert_eq!(fill.unclaimed, 2);
    // Claims are permissionless and pro rata: the second LP's claim is sent by the first.
    e.must(&[e.claim_ix(&lp.pubkey(), 1, &lp.pubkey())], &[&lp]);
    e.must(&[e.claim_ix(&lp2.pubkey(), 1, &lp.pubkey())], &[&lp]);
    let got1 = e.token_amount(&ata(&lp.pubkey(), &USDC_DEVNET)) - b1;
    let got2 = e.token_amount(&ata(&lp2.pubkey(), &USDC_DEVNET)) - b2;
    assert!(got1 > 0);
    assert_eq!(
        got1, got2,
        "equal holders exiting in the same epoch receive equal fills"
    );
    let back1 = e.token_amount(&ata(&lp.pubkey(), &e.share_mint));
    let back2 = e.token_amount(&ata(&lp2.pubkey(), &e.share_mint));
    assert_eq!(back1, back2, "unfilled shares return equally");
    assert!(back1 > 0 && back1 < shares);
    let q: WithdrawQueue = e.acct("WithdrawQueue", &e.queue_pda());
    assert_eq!(q.fills[0].unclaimed, 0);
    assert_eq!(q.fills[0].amount_left, 0, "fills are paid out exactly");
    assert!(
        e.svm.get_account(&e.request_pda(&lp.pubkey(), 1)).is_none(),
        "the request account is closed"
    );
    // Conservation: the vault still covers capital, collateral and the processed-but-unclaimed remainder.
    let p: Pool = e.acct("Pool", &e.pool);
    assert!(e.token_amount(&e.vault) >= p.tvl + p.collateral_held + p.withdraw_reserved);
}

/// ADR-003 (review F-30). Invariant: no hook can veto an exit. A pool with a hook program and every live point
/// enabled lets `lp_withdraw`, `trader_cancel_swap` and `crank_settle_swap` run without any hook account, in
/// `Normal` and in `WithdrawOnly`; and the retired exit flags are refused at pool creation. Uses the simulation
/// harness's probe program (`programs/tests/sim/hook`), skipped when its binary is absent.
#[test]
fn adr003_hooks_cannot_veto_exits() {
    let mut e = setup();
    if !e.load_hook_program() {
        eprintln!("hook probe binary absent; adr003 skipped");
        return;
    }
    let lp = e.lp.insecure_clone();
    let tr = e.trader.insecure_clone();
    let mut bench2 = e.create_benchmark(*b"hooked-usdc\0\0\0\0\0", 684);
    let retired = HookFlags {
        reserved_6: true,
        ..HookFlags::default()
    };
    // Round-02 test observation T-2: the tested binaries accept a retired flag at creation (it is never sent,
    // so it has no effect); the ADR says it must be false. Either outcome is recorded, not asserted.
    match e.create_pool(bench2, Some(HOOK_PROGRAM), retired) {
        Err(_) => eprintln!("T-2: retired exit hook flag refused at pool creation"),
        Ok(_) => {
            eprintln!(
                "T-2: retired exit hook flag accepted at pool creation (no effect; see REPORT.md)"
            );
            bench2 = e.create_benchmark(*b"hooked-usdc-2\0\0\0", 684);
        }
    }
    // Points 0 and 1 attack the open on the probe (covered in adversarial.rs), and since external scan 2
    // (finding 25) the veto points run in `Limited` as well as `Normal`, so there is no mode in which a pool
    // with those points enabled admits an open without the hook. The exits are the subject here: the deposit
    // points stay live and the open points are off, so the positions open without a hook account.
    let live = HookFlags {
        before_deposit: true,
        after_deposit: true,
        ..HookFlags::default()
    };
    let k = e.create_pool(bench2, Some(HOOK_PROGRAM), live).unwrap();
    e.use_pool(&k);
    let m = e.share_mint;
    e.create_ata(&lp.pubkey(), &m);
    // Points 4 and 5 observe and return Ok on the probe.
    let ix = e.deposit_ix_hook(&lp.pubkey(), 1_000_000 * USDC, 0, HOOK_PROGRAM);
    e.must(&[ix], &[&lp]);
    // The deposit veto point runs in `Limited` too: a deposit without the hook account is refused there.
    let a = e.authority.insecure_clone();
    e.set_mode(&a, OperatingMode::Limited).unwrap();
    let ix = e.deposit_ix(&lp.pubkey(), 1_000 * USDC, 0);
    e.must_fail(&[ix], &[&lp], "HookMismatch");
    e.set_mode(&a, OperatingMode::Normal).unwrap();
    let ix = e.open_ix(
        &tr.pubkey(),
        &open_args(LegKind::PayFixed, 0, 10_000 * USDC, 9_999, 1),
    );
    e.must(&[ix], &[&tr]);
    let ix = e.open_ix(
        &tr.pubkey(),
        &open_args(LegKind::PayFixed, 0, 10_000 * USDC, 9_999, 2),
    );
    e.must(&[ix], &[&tr]);
    let ix = e.withdraw_ix(&lp.pubkey(), 1_000 * USDC * SHARE, 0);
    e.must(&[ix], &[&lp]);
    let ix = e.close_ix("trader_cancel_swap", &tr.pubkey(), 1, &tr.pubkey(), &0u64);
    e.must(&[ix], &[&tr]);
    let g = e.guardian.insecure_clone();
    e.set_mode(&g, OperatingMode::WithdrawOnly).unwrap();
    // The exit under WithdrawOnly runs through the queue (ADR-009); none of these carry a hook account.
    let ix = e.withdraw_ix(&lp.pubkey(), 1_000 * USDC * SHARE, 0);
    e.must_fail(&[ix], &[&lp], "UseWithdrawQueue");
    e.must(&[e.init_queue_ix()], &[]);
    let ix = e.enqueue_ix(&lp.pubkey(), 1_000 * USDC * SHARE, 1);
    e.must(&[ix], &[&lp]);
    e.warp(29 * 216_000, 29 * DAY);
    e.publish(684).unwrap();
    let ix = e.close_ix("crank_settle_swap", &tr.pubkey(), 2, &e.payer.pubkey(), &());
    e.must(&[ix], &[]);
    let p: Pool = e.acct("Pool", &e.pool);
    assert_eq!(p.open_swaps, 0);
}

/// ADR-001 / ADR-013 (review F-29). Invariant: `trader_open_swap`, `trader_cancel_swap`, `crank_settle_swap`,
/// `crank_liquidate_swap` and `lp_withdraw` take `Global` and `fee_vault` read-only (or not at all), with fee
/// accrual held per pool, so two pools' transactions never share a write lock.
#[test]
fn adr001_hot_path_takes_no_shared_write_lock() {
    let e = setup();
    let tr = e.trader.pubkey();
    let lp = e.lp.pubkey();
    let open = e.open_ix(
        &tr,
        &open_args(LegKind::PayFixed, 0, 10_000 * USDC, 9_999, 1),
    );
    let close = e.close_ix("trader_cancel_swap", &tr, 1, &tr, &0u64);
    let withdraw = e.withdraw_ix(&lp, 1, 0);
    for ix in [open, close, withdraw] {
        for m in &ix.accounts {
            if m.pubkey == e.global || m.pubkey == e.fee_vault {
                assert!(
                    !m.is_writable,
                    "{} is writable in a hot-path instruction",
                    m.pubkey
                );
            }
        }
    }
}
