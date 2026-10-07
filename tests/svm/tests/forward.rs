//! Forward-starting swaps on LiteSVM: a fixed rate agreed now for a swap whose accrual
//! begins 28 days ahead, quoted on the Vernier forward curve; the permissionless start crank takes the start
//! reading from the daily fixings and swaps the book terms; settlement pays over [start, maturity]; a cancel
//! before the start is priced on the forward curve; the horizon and the flag gates refuse what they should.
use brink_svm_tests::harness::*;
use brink_svm_tests::*;
use solana_signer::Signer;

const NOTIONAL: u64 = 1_000_000 * USDC;

fn fwd(leg: LegKind, start: u8, tenor: u8, limit: u16, seed: u64) -> OpenForwardSwapArgs {
    OpenForwardSwapArgs {
        leg,
        start,
        tenor,
        notional: NOTIONAL,
        limit_rate_bp: limit,
        client_seed: seed,
    }
}

fn setup_funded() -> Env {
    let mut e = setup();
    let lp = e.lp.insecure_clone();
    e.deposit(&lp, 10_000_000 * USDC);
    e
}

#[test]
fn forward_open_start_and_settle() {
    let mut e = setup_funded();
    let tr = e.trader.insecure_clone();
    // Pay fixed, start 28 days, tenor 60 days. Vernier forward: ref 684 + forward model 39 (curve 11/22/31/54
    // over 28/60/90/180: (m(88)·88 − m(28)·28)/60 with m(88) = 30.4) + demand 2 + term 5 = 730. The spot 60-day
    // quote would be 684 + 22 + 2 + 5 = 713.
    let ix = e.open_forward_ix(&tr.pubkey(), &fwd(LegKind::PayFixed, 0, 1, 729, 1));
    e.must_fail(&[ix], &[&tr], "LimitRate");
    let ix = e.open_forward_ix(&tr.pubkey(), &fwd(LegKind::PayFixed, 0, 1, 730, 1));
    e.must(&[ix], &[&tr]);
    let s: Swap = e.acct("Swap", &e.swap_pda(&tr.pubkey(), 1));
    let opened = e.clock().unix_timestamp;
    assert_eq!(s.fixed_bp, 730);
    assert_eq!(s.link_flags, LINK_FORWARD);
    assert_eq!(s.start_ts, opened + 28 * DAY);
    assert_eq!(s.matures_ts, s.start_ts + 60 * DAY);
    assert_eq!(
        s.index_accrual_start, 0,
        "start reading waits for the crank"
    );
    // Collateral for a 60-day tenor is 230 bp; the opening fee 5 bp; both at open.
    assert_eq!(s.collateral, 23_000 * USDC);
    let p: Pool = e.acct("Pool", &e.pool);
    assert_eq!(p.open_pay_notional, NOTIONAL, "notional counts from open");
    assert_eq!(p.util_pay_bp, 1_000);
    assert_eq!(p.open_swaps, 1);
    assert_eq!(p.fees_buyback_accrued, 250 * USDC);
    // The forward book terms: no accrued leg, the remaining-term leg over the swap's own term.
    assert_eq!(p.book_pay.notional, 0);
    assert_eq!(p.book_pay.accrual_start, 0);
    assert_eq!(
        p.book_pay.maturity_weight,
        i128::from(NOTIONAL) * 60 * i128::from(DAY)
    );
    assert_eq!(
        p.book_pay.fixed_leg,
        i128::from(NOTIONAL) * 730 * 60 * i128::from(DAY)
    );

    // The crank is refused before the start, and on a spot swap.
    let ix = e.start_forward_ix(&tr.pubkey(), 1);
    e.must_fail(&[ix], &[], "ForwardNotDue");
    let ix = e.open_ix(
        &tr.pubkey(),
        &open_args(LegKind::ReceiveFixed, 0, 1_000 * USDC, 0, 9),
    );
    e.must(&[ix], &[&tr]);
    let ix = e.start_forward_ix(&tr.pubkey(), 9);
    e.must_fail(&[ix], &[], "NotForward");
    let trader_before = e.token_amount(&ata(&tr.pubkey(), &USDC_DEVNET));

    // Floating stays at 684 to the start, then rises to 900 for the whole accrual period.
    e.warp(14 * 216_000, 14 * DAY);
    e.publish(684).unwrap();
    e.warp(14 * 216_000, 14 * DAY);
    assert_eq!(e.clock().unix_timestamp, s.start_ts);
    e.publish(900).unwrap();
    let b: Benchmark = e.acct("Benchmark", &e.benchmark);
    let accrual_at_start = b.accrual_e18;
    e.warp(216_000, DAY);
    e.publish(900).unwrap();
    // Anyone cranks the start once it has passed; the reading is the accrual at the start midnight.
    let ix = e.start_forward_ix(&tr.pubkey(), 1);
    e.must(&[ix], &[]);
    let s: Swap = e.acct("Swap", &e.swap_pda(&tr.pubkey(), 1));
    assert_eq!(s.link_flags, LINK_FORWARD | FORWARD_STARTED);
    assert_eq!(s.index_accrual_start, accrual_at_start);
    let p: Pool = e.acct("Pool", &e.pool);
    assert_eq!(
        p.book_pay.notional,
        i128::from(NOTIONAL),
        "ordinary terms from the start on"
    );
    // The book keeps the accrual in whole units (`accrual_e18 / 1e18`), signed by the leg and scaled by notional.
    assert_eq!(
        p.book_pay.accrual_start,
        i128::from(NOTIONAL)
            * i128::try_from(accrual_at_start / 1_000_000_000_000_000_000).unwrap()
    );
    let ix = e.start_forward_ix(&tr.pubkey(), 1);
    e.must_fail(&[ix], &[], "ForwardAlreadyStarted");

    // To maturity at 900, then settle: pnl = (900 − 730) bp × 1,000,000 × 60/365, fee 10 percent.
    e.warp(59 * 216_000, 59 * DAY);
    e.publish(900).unwrap();
    assert_eq!(e.clock().unix_timestamp, s.matures_ts);
    let ix = e.close_ix("crank_settle_swap", &tr.pubkey(), 1, &e.payer.pubkey(), &());
    e.must(&[ix], &[]);
    let pnl = 170u128 * 1_000_000 * u128::from(USDC) * 60 / (10_000 * 365);
    let payout = 23_000 * u128::from(USDC) + pnl - pnl / 10;
    let trader_after = e.token_amount(&ata(&tr.pubkey(), &USDC_DEVNET));
    assert_eq!(
        u128::from(trader_after - trader_before),
        payout,
        "forward settles over its own term"
    );
    let p: Pool = e.acct("Pool", &e.pool);
    assert_eq!(p.open_swaps, 1, "only the spot swap remains");
    assert_eq!(
        p.book_pay,
        BookSide::default(),
        "book returns to zero for the pay side"
    );
}

#[test]
fn forward_cancel_before_start_prices_on_the_curve_and_clears_the_book() {
    let mut e = setup_funded();
    let tr = e.trader.insecure_clone();
    let ix = e.open_forward_ix(&tr.pubkey(), &fwd(LegKind::ReceiveFixed, 1, 0, 0, 2));
    e.must(&[ix], &[&tr]);
    let s: Swap = e.acct("Swap", &e.swap_pda(&tr.pubkey(), 2));
    // Receive fixed, start 60, tenor 28: ref 684 − forward model 18 − demand 2 − term 3 = 661.
    assert_eq!(s.fixed_bp, 661);
    assert_eq!(s.start_ts - s.opened_ts, 60 * DAY);
    let before = e.token_amount(&ata(&tr.pubkey(), &USDC_DEVNET));
    e.warp(10 * 216_000, 10 * DAY);
    e.publish(684).unwrap();
    // Cancel 50 days before the start: nothing has accrued, the unwind is the pay-fixed forward quote for a
    // start 50 days out. Payout is collateral less the mark loss (the bid-ask of the curve), never more than
    // collateral plus the mark gain, and the account closes.
    let ix = e.close_ix("trader_cancel_swap", &tr.pubkey(), 2, &tr.pubkey(), &0u64);
    e.must(&[ix], &[&tr]);
    let after = e.token_amount(&ata(&tr.pubkey(), &USDC_DEVNET));
    let got = after - before;
    assert!(
        got > 0 && got < s.collateral,
        "cancel pays collateral less the curve spread, got {got}"
    );
    let p: Pool = e.acct("Pool", &e.pool);
    assert_eq!(p.open_swaps, 0);
    assert_eq!(p.book_rec, BookSide::default());
    assert_eq!(p.collateral_held, 0);
    assert!(e
        .svm
        .get_account(&e.swap_pda(&tr.pubkey(), 2))
        .map(|a| a.lamports == 0 || a.data.is_empty())
        .unwrap_or(true));
}

#[test]
fn forward_refuses_the_horizon_and_unknown_starts() {
    let mut e = setup_funded();
    let tr = e.trader.insecure_clone();
    // start 90 + tenor 180 runs past the curve.
    let ix = e.open_forward_ix(&tr.pubkey(), &fwd(LegKind::PayFixed, 2, 3, 2_000, 3));
    e.must_fail(&[ix], &[&tr], "ForwardHorizon");
    // start 28 + tenor 180 likewise; a 180-day tenor is never forward-startable.
    let ix = e.open_forward_ix(&tr.pubkey(), &fwd(LegKind::PayFixed, 0, 3, 2_000, 4));
    e.must_fail(&[ix], &[&tr], "ForwardHorizon");
    // An unknown start index.
    let ix = e.open_forward_ix(&tr.pubkey(), &fwd(LegKind::PayFixed, 3, 0, 2_000, 5));
    e.must_fail(&[ix], &[&tr], "ForwardHorizon");
    // start 90 + tenor 90 is the last allowed point.
    let ix = e.open_forward_ix(&tr.pubkey(), &fwd(LegKind::PayFixed, 2, 2, 2_000, 6));
    e.must(&[ix], &[&tr]);
    let s: Swap = e.acct("Swap", &e.swap_pda(&tr.pubkey(), 6));
    assert_eq!(s.matures_ts - s.opened_ts, 180 * DAY);
    // ref 684 + forward model 77 + demand 2 + term 7 = 770.
    assert_eq!(s.fixed_bp, 770);
}
