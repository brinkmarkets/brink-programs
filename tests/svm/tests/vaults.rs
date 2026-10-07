//! Fixed vaults: subscribe, hedge (receive fixed on the pool, place the rest at the venue), settle at maturity,
//! redeem pro rata. Cancellation paths return deposits 1:1.
use brink_svm_tests::harness::*;
use brink_svm_tests::products::*;
use brink_svm_tests::*;
use solana_signer::Signer;

const WINDOW: i64 = 2 * 86_400;
const SEED: u64 = 77;

fn series_args(id: u16, tenor: u8, cap: u64, min_total: u64, fee_bp: u16) -> CreateSeriesArgs {
    CreateSeriesArgs {
        series_id: id,
        tenor,
        subscribe_seconds: WINDOW,
        cap,
        min_total,
        fee_bp,
        client_seed: SEED,
        min_fixed_bp: 0,
    }
}

/// Venue on the default benchmark, a funded reserve, LP capital in the pool, one series.
fn vault_env(args: &CreateSeriesArgs) -> Env {
    let mut e = setup_products();
    let a = e.authority.insecure_clone();
    let b = e.benchmark;
    let ix = e.create_venue_ix(&b, &a.pubkey());
    e.must(&[ix], &[&a]);
    let lp = e.lp.insecure_clone();
    e.deposit(&lp, 5_000_000 * USDC);
    let ix = e.fund_ix(&b, &lp.pubkey(), 200_000 * USDC);
    e.must(&[ix], &[&lp]);
    let ix = e.create_series_ix(&a.pubkey(), args);
    e.must(&[ix], &[&a]);
    e
}

#[test]
fn creation_is_gated_and_checked() {
    let mut e = setup_products();
    let a = e.authority.insecure_clone();
    let b = e.benchmark;
    let args = series_args(1, 0, 100_000 * USDC, 10_000 * USDC, 1_000);
    // No venue yet: the series cannot name one.
    let ix = e.create_series_ix(&a.pubkey(), &args);
    assert!(e.send(&[ix], &[&a]).is_err());
    let ix = e.create_venue_ix(&b, &a.pubkey());
    e.must(&[ix], &[&a]);
    let stranger = e.new_actor(0, false);
    let ix = e.create_series_ix(&stranger.pubkey(), &args);
    e.must_fail(&[ix], &[&stranger], "NotUpgradeAuthority");
    let mut bad = args;
    bad.fee_bp = 2_001;
    let ix = e.create_series_ix(&a.pubkey(), &bad);
    e.must_fail(&[ix], &[&a], "Fee");
    let mut bad = args;
    bad.subscribe_seconds = 60;
    let ix = e.create_series_ix(&a.pubkey(), &bad);
    e.must_fail(&[ix], &[&a], "Window");
    let mut bad = args;
    bad.tenor = 4;
    let ix = e.create_series_ix(&a.pubkey(), &bad);
    e.must_fail(&[ix], &[&a], "Tenor");
    let mut bad = args;
    bad.min_total = bad.cap + 1;
    let ix = e.create_series_ix(&a.pubkey(), &bad);
    e.must_fail(&[ix], &[&a], "Caps");
    let ix = e.create_series_ix(&a.pubkey(), &args);
    e.must(&[ix], &[&a]);
    let s: Series = e.acct("Series", &series_pda(&e.pool, 1));
    assert_eq!(s.status, SeriesStatus::Subscribing);
    assert_eq!(s.subscribe_until_ts, e.clock().unix_timestamp + WINDOW);
    assert_eq!(s.cap, 100_000 * USDC);
    assert!(
        e.svm
            .get_balance(&series_trader(&series_pda(&e.pool, 1)))
            .unwrap()
            >= 10_000_000
    );
}

#[test]
fn subscribe_hedge_settle_redeem() {
    let args = series_args(1, 0, 1_000_000 * USDC, 10_000 * USDC, 1_000);
    let mut e = vault_env(&args);
    let series = series_pda(&e.pool, 1);
    let shares = series_shares(&series);
    let u1 = e.actor_with(100_000 * USDC, &[shares]);
    let u2 = e.actor_with(100_000 * USDC, &[shares]);
    let ix = e.series_deposit_ix(1, &u1.pubkey(), 60_000 * USDC);
    e.must(&[ix], &[&u1]);
    let ix = e.series_deposit_ix(1, &u2.pubkey(), 60_000 * USDC);
    e.must(&[ix], &[&u2]);
    assert_eq!(e.token_amount(&ata(&u1.pubkey(), &shares)), 60_000 * USDC);
    // u2 changes their mind about a third of it: 1:1 out before the hedge.
    let ix = e.series_withdraw_ix(1, &u2.pubkey(), 20_000 * USDC);
    e.must(&[ix], &[&u2]);
    assert_eq!(
        e.token_amount(&ata(&u2.pubkey(), &USDC_DEVNET)),
        60_000 * USDC
    );
    let s: Series = e.acct("Series", &series);
    assert_eq!(s.deposits, 100_000 * USDC);
    // Too early to hedge; a deposit over the cap is refused.
    let payer = e.payer.pubkey();
    let ix = e.hedge_ix(1, SEED, &payer, 0);
    e.must_fail(&[ix], &[], "WindowOpen");
    let ix = e.series_deposit_ix(1, &u1.pubkey(), 1_000_000 * USDC);
    e.must_fail(&[ix], &[&u1], "Cap");
    e.warp(WINDOW as u64 / 400 * 1_000, WINDOW);
    e.publish(684).unwrap();
    // Window closed: no more deposits; anyone hedges. A limit the quote cannot meet refuses.
    let ix = e.series_deposit_ix(1, &u1.pubkey(), 1_000 * USDC);
    e.must_fail(&[ix], &[&u1], "WindowClosed");
    let ix = e.hedge_ix(1, SEED, &payer, 5_000);
    e.must_fail(&[ix], &[], "LimitRate");
    let ix = e.hedge_ix(1, SEED, &payer, 0);
    e.must(&[ix], &[]);
    let s: Series = e.acct("Series", &series);
    assert_eq!(s.status, SeriesStatus::Hedged);
    assert!(s.fixed_bp >= s.min_fixed_bp);
    // The largest notional whose collateral (DEFAULT_PARAMS, tenor 0) still fits beside it in the deposits.
    assert!(
        s.notional + s.collateral <= s.deposits,
        "{} + {} <= {}",
        s.notional,
        s.collateral,
        s.deposits
    );
    let cb = u64::from(DEFAULT_PARAMS.collateral_bp[0]);
    let expected = s.deposits * 10_000 / (10_000 + cb);
    assert!(
        s.notional + 1 >= expected && s.notional <= expected,
        "notional {} vs {expected}",
        s.notional
    );
    assert_eq!(
        s.collateral,
        s.notional * cb / 10_000 + u64::from(!(s.notional * cb).is_multiple_of(10_000))
    );
    assert!(s.fixed_bp > 0);
    let sw: Swap = e.acct("Swap", &s.swap);
    assert_eq!(sw.leg, LegKind::ReceiveFixed);
    assert_eq!(sw.trader, series_trader(&series));
    assert_eq!(sw.notional, s.notional);
    assert_eq!(s.matures_ts, sw.matures_ts);
    // Everything the swap did not take sits at the venue, as receipts.
    assert_eq!(e.token_amount(&series_usdc(&series)), 0);
    let fee = open_fee(s.notional);
    assert!(
        s.placed + s.collateral + fee >= s.deposits - 2
            && s.placed + s.collateral + fee <= s.deposits,
        "placed {} collateral {} fee {fee} deposits {}",
        s.placed,
        s.collateral,
        s.deposits
    );
    assert!(e.token_amount(&series_receipts(&series)) > 0);
    // No withdrawals while hedged; nothing to settle before maturity.
    let ix = e.series_withdraw_ix(1, &u1.pubkey(), 1_000 * USDC);
    e.must_fail(&[ix], &[&u1], "Status");
    let ix = e.series_settle_ix(1, SEED, &payer);
    e.must_fail(&[ix], &[], "NotMatured");
    // Run to maturity with the benchmark steady, then settle in one transaction (swap first, then the venue).
    let to_go = s.matures_ts - e.clock().unix_timestamp;
    e.warp((to_go / 400 * 1_000) as u64 + 10, to_go);
    e.publish(684).unwrap();
    let treasury = ata(&e.treasury_owner.pubkey(), &USDC_DEVNET);
    let escrow = ata(&buyback_owner(&e.treasury_owner.pubkey()), &USDC_DEVNET);
    let (t0, b0) = (e.token_amount(&treasury), e.token_amount(&escrow));
    let ix = e.series_settle_ix(1, SEED, &payer);
    e.must(&[ix], &[]);
    let s: Series = e.acct("Series", &series);
    assert_eq!(s.status, SeriesStatus::Settled);
    assert!(
        e.svm
            .get_account(&s.swap)
            .map(|a| a.data.is_empty())
            .unwrap_or(true),
        "swap closed"
    );
    assert_eq!(e.token_amount(&series_receipts(&series)), 0);
    // A steady benchmark: the floating leg and the fixed leg roughly cancel, so the series earns about the
    // benchmark on what it placed, less the opening fee and the collateral drag. The fee on yield is 10 percent,
    // split half and half.
    assert!(
        s.settled_assets > s.deposits,
        "settled {} > deposits {}",
        s.settled_assets,
        s.deposits
    );
    let yield_ = e.token_amount(&series_usdc(&series)) + s.fee_paid - s.deposits;
    assert_eq!(s.fee_paid, yield_ * 1_000 / 10_000);
    assert_eq!(e.token_amount(&treasury) - t0, s.fee_paid / 2);
    assert_eq!(e.token_amount(&escrow) - b0, s.fee_paid - s.fee_paid / 2);
    assert_eq!(e.token_amount(&series_usdc(&series)), s.settled_assets);
    // Redemptions are pro rata and exhaust the account to dust.
    let ix = e.series_redeem_ix(1, &u1.pubkey(), 60_000 * USDC);
    e.must(&[ix], &[&u1]);
    let ix = e.series_redeem_ix(1, &u2.pubkey(), 40_000 * USDC);
    e.must(&[ix], &[&u2]);
    let got1 = e.token_amount(&ata(&u1.pubkey(), &USDC_DEVNET)) - 40_000 * USDC;
    let got2 = e.token_amount(&ata(&u2.pubkey(), &USDC_DEVNET)) - 60_000 * USDC;
    assert!(
        got1 > 60_000 * USDC && got2 > 40_000 * USDC,
        "{got1} {got2}"
    );
    assert!(
        got1 * 2 / 3 >= got2 - 1 && got1 * 2 / 3 <= got2 + 1,
        "pro rata: {got1} {got2}"
    );
    assert!(e.token_amount(&series_usdc(&series)) <= 1);
    assert_eq!(e.mint_supply(&shares), 0);
}

/// The AMM's opening fee: 5 bp of notional, rounded down.
fn open_fee(notional: u64) -> u64 {
    notional * 5 / 10_000
}

#[test]
fn below_minimum_cancels_and_deposits_come_back_one_to_one() {
    let args = series_args(2, 0, 1_000_000 * USDC, 50_000 * USDC, 1_000);
    let mut e = vault_env(&args);
    let series = series_pda(&e.pool, 2);
    let shares = series_shares(&series);
    let u = e.actor_with(100_000 * USDC, &[shares]);
    let ix = e.series_deposit_ix(2, &u.pubkey(), 20_000 * USDC);
    e.must(&[ix], &[&u]);
    e.warp(WINDOW as u64 / 400 * 1_000, WINDOW);
    e.publish(684).unwrap();
    let payer = e.payer.pubkey();
    let ix = e.hedge_ix(2, SEED, &payer, 0);
    e.must(&[ix], &[]);
    let s: Series = e.acct("Series", &series);
    assert_eq!(s.status, SeriesStatus::Cancelled);
    assert_eq!(s.swap, solana_pubkey::Pubkey::default());
    let ix = e.series_withdraw_ix(2, &u.pubkey(), 20_000 * USDC);
    e.must(&[ix], &[&u]);
    assert_eq!(
        e.token_amount(&ata(&u.pubkey(), &USDC_DEVNET)),
        100_000 * USDC
    );
    // A cancelled series cannot be hedged or settled.
    let ix = e.hedge_ix(2, SEED, &payer, 0);
    e.must_fail(&[ix], &[], "Status");
}

#[test]
fn an_unhedged_series_cancels_after_the_grace_or_on_the_authority() {
    let args = series_args(3, 1, 1_000_000 * USDC, 10_000 * USDC, 500);
    let mut e = vault_env(&args);
    let series = series_pda(&e.pool, 3);
    let shares = series_shares(&series);
    let u = e.actor_with(100_000 * USDC, &[shares]);
    let ix = e.series_deposit_ix(3, &u.pubkey(), 50_000 * USDC);
    e.must(&[ix], &[&u]);
    e.warp(WINDOW as u64 / 400 * 1_000, WINDOW);
    let stranger = e.new_actor(0, false);
    let ix = e.series_cancel_ix(3, &stranger.pubkey());
    e.must_fail(&[ix], &[&stranger], "NotYet");
    e.warp(3 * 216_000, 3 * DAY);
    let ix = e.series_cancel_ix(3, &stranger.pubkey());
    e.must(&[ix], &[&stranger]);
    let s: Series = e.acct("Series", &series);
    assert_eq!(s.status, SeriesStatus::Cancelled);
    let ix = e.series_withdraw_ix(3, &u.pubkey(), 50_000 * USDC);
    e.must(&[ix], &[&u]);
    assert_eq!(
        e.token_amount(&ata(&u.pubkey(), &USDC_DEVNET)),
        100_000 * USDC
    );
    // The authority may cancel a subscribing series at once (and only a subscribing one).
    let a = e.authority.insecure_clone();
    let args4 = series_args(4, 1, 1_000_000 * USDC, 10_000 * USDC, 500);
    let ix = e.create_series_ix(&a.pubkey(), &args4);
    e.must(&[ix], &[&a]);
    let ix = e.series_cancel_ix(4, &a.pubkey());
    e.must(&[ix], &[&a]);
    let s: Series = e.acct("Series", &series_pda(&e.pool, 4));
    assert_eq!(s.status, SeriesStatus::Cancelled);
    let ix = e.series_cancel_ix(3, &a.pubkey());
    e.must_fail(&[ix], &[&a], "Status");
}

/// External scan 2, finding 15. Invariant: the fixed rate the hedge commits a series to is never below the
/// floor set at creation, whatever limit the permissionless caller supplies: a floor the quote cannot meet
/// refuses the hedge (the AMM's own limit check fires on the raised limit) and leaves the series subscribing,
/// where the authority can cancel it and deposits come back one to one.
#[test]
fn the_series_floor_binds_the_permissionless_hedge() {
    let mut args = series_args(3, 0, 1_000_000 * USDC, 10_000 * USDC, 1_000);
    args.min_fixed_bp = 5_000;
    let mut e = vault_env(&args);
    let series = series_pda(&e.pool, 3);
    let shares = series_shares(&series);
    let s: Series = e.acct("Series", &series);
    assert_eq!(s.min_fixed_bp, 5_000);
    let u = e.actor_with(100_000 * USDC, &[shares]);
    let ix = e.series_deposit_ix(3, &u.pubkey(), 50_000 * USDC);
    e.must(&[ix], &[&u]);
    e.warp(WINDOW as u64 / 400 * 1_000, WINDOW);
    e.publish(684).unwrap();
    let payer = e.payer.pubkey();
    // A limit of zero from a stranger is raised to the series floor; the quote near 684 bp cannot meet it.
    let ix = e.hedge_ix(3, SEED, &payer, 0);
    e.must_fail(&[ix], &[], "LimitRate");
    let s: Series = e.acct("Series", &series);
    assert_eq!(s.status, SeriesStatus::Subscribing);
    assert_eq!(s.fixed_bp, 0);
}
