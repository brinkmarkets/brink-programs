//! Yield splitting: PT and YT over Brink pool shares and over devnet venue receipts, redeemable as a pair until
//! settlement and separately after it, with the yield settled at maturity.
use brink_svm_tests::harness::*;
use brink_svm_tests::products::*;
use brink_svm_tests::*;
use solana_pubkey::Pubkey;
use solana_signer::Signer;

const E18: u128 = 1_000_000_000_000_000_000;
const TERM: i64 = 90 * DAY;

/// LP capital in the pool and a user holding 1,000 USDC of pool shares.
fn pool_env() -> (Env, solana_keypair::Keypair, i64) {
    let mut e = setup_products();
    let lp = e.lp.insecure_clone();
    e.deposit(&lp, 1_000_000 * USDC);
    let share_mint = e.share_mint;
    let u = e.actor_with(10_000 * USDC, &[share_mint]);
    let ix = e.deposit_ix(&u.pubkey(), 1_000 * USDC, 0);
    e.must(&[ix], &[&u]);
    let maturity = e.clock().unix_timestamp + TERM;
    (e, u, maturity)
}

fn market_over(
    e: &mut Env,
    source: Pubkey,
    benchmark: Option<Pubkey>,
    sy_mint: Pubkey,
    kind: SourceKind,
    maturity: i64,
) -> Pubkey {
    let a = e.authority.insecure_clone();
    let args = CreateMarketArgs {
        source_kind: kind,
        maturity_ts: maturity,
        min_mint: USDC,
    };
    let ix = e.create_market_ix(&a.pubkey(), &source, benchmark.as_ref(), &sy_mint, &args);
    e.must(&[ix], &[&a]);
    market_pda(&sy_mint, maturity)
}

/// The first UTC midnight at or after `ts`.
fn midnight_from(ts: i64) -> i64 {
    ts.div_euclid(DAY) * DAY + if ts.rem_euclid(DAY) == 0 { 0 } else { DAY }
}

#[test]
fn creation_is_gated_and_bound_to_the_unit() {
    let (mut e, _u, maturity) = pool_env();
    let a = e.authority.insecure_clone();
    let (pool, share_mint) = (e.pool, e.share_mint);
    let args = CreateMarketArgs {
        source_kind: SourceKind::BrinkPool,
        maturity_ts: maturity,
        min_mint: USDC,
    };
    let stranger = e.new_actor(0, false);
    let ix = e.create_market_ix(&stranger.pubkey(), &pool, None, &share_mint, &args);
    e.must_fail(&[ix], &[&stranger], "NotUpgradeAuthority");
    // The SY must be the unit's own mint, and the kind must match the account.
    let ix = e.create_market_ix(&a.pubkey(), &pool, None, &USDC_DEVNET, &args);
    e.must_fail(&[ix], &[&a], "Source");
    let venue_kind = CreateMarketArgs {
        source_kind: SourceKind::BrinkVenue,
        maturity_ts: midnight_from(maturity),
        ..args
    };
    let ix = e.create_market_ix(&a.pubkey(), &pool, None, &share_mint, &venue_kind);
    e.must_fail(&[ix], &[&a], "Source");
    let short = CreateMarketArgs {
        maturity_ts: e.clock().unix_timestamp + 3_600,
        ..args
    };
    let ix = e.create_market_ix(&a.pubkey(), &pool, None, &share_mint, &short);
    e.must_fail(&[ix], &[&a], "Term");
    let ix = e.create_market_ix(&a.pubkey(), &pool, None, &share_mint, &args);
    e.must(&[ix], &[&a]);
    let m: Market = e.acct("Market", &market_pda(&share_mint, maturity));
    assert_eq!(m.source, pool);
    assert_eq!(m.sy_mint, share_mint);
    assert!(!m.settled);
    assert_eq!(m.version, 2);
    // At par the origin rate is one USDC per whole share: 1e18 / 1e3 per base unit.
    assert_eq!(m.rate_origin_e18, E18 / 1_000);
}

#[test]
fn pool_shares_split_and_settle_with_the_pool_rate() {
    let (mut e, u, maturity) = pool_env();
    let (pool, share_mint) = (e.pool, e.share_mint);
    let market = market_over(
        &mut e,
        pool,
        None,
        share_mint,
        SourceKind::BrinkPool,
        maturity,
    );
    let (pt, yt) = (market_pt(&market), market_yt(&market));
    e.create_ata(&u.pubkey(), &pt);
    e.create_ata(&u.pubkey(), &yt);
    // 1,000 USDC of shares at par: 5 bp fee in SY, PT = YT = 999.5 USDC.
    let sy = 1_000 * USDC * SHARE;
    let ix = e.mint_py_ix(&market, &share_mint, &u.pubkey(), sy, 0);
    e.must(&[ix], &[&u]);
    let got_pt = e.token_amount(&ata(&u.pubkey(), &pt));
    assert_eq!(got_pt, e.token_amount(&ata(&u.pubkey(), &yt)));
    assert!((999_499_000..=999_500_000).contains(&got_pt), "py {got_pt}");
    let m: Market = e.acct("Market", &market);
    assert_eq!(m.fees, sy * 5 / 10_000);
    assert_eq!(e.token_amount(&market_fees(&market)), m.fees);
    assert_eq!(m.sy_locked, sy - m.fees);
    // A pair redeems the SY that backs it, at the origin rate, any time before settlement.
    let ix = e.redeem_py_ix(&market, &share_mint, &u.pubkey(), got_pt / 2, 0);
    e.must(&[ix], &[&u]);
    let back = e.token_amount(&ata(&u.pubkey(), &share_mint));
    assert!(
        back >= (sy - m.fees) / 2 - SHARE && back <= (sy - m.fees) / 2,
        "sy back {back}"
    );
    // The pool earns: a trader opens and the opening fee is swept into TVL by the LP accounting, so the share
    // price rises; the yield token carries that.
    let tr = e.trader.insecure_clone();
    for seed in 1..=3u64 {
        let ix = e.open_ix(
            &tr.pubkey(),
            &open_args(LegKind::PayFixed, 0, 100_000 * USDC, 2_000, seed),
        );
        e.must(&[ix], &[&tr]);
    }
    e.warp(45 * 216_000, 45 * DAY);
    e.publish(684).unwrap();
    for seed in 1..=3u64 {
        let ix = e.close_ix(
            "crank_settle_swap",
            &tr.pubkey(),
            seed,
            &e.payer.pubkey(),
            &(),
        );
        e.must(&[ix], &[]);
    }
    let p: Pool = e.acct("Pool", &pool);
    let rate_now = (u128::from(p.tvl) + 1) * E18 / (u128::from(p.share_supply) + 1_000);
    assert!(rate_now > E18 / 1_000, "rate rose: {rate_now}");
    // Not matured: minting still runs, settlement does not.
    let payer = e.payer.pubkey();
    let ix = e.market_settle_ix(&market, &pool, None, &payer);
    e.must_fail(&[ix], &[], "NotMatured");
    let rest = maturity - e.clock().unix_timestamp;
    e.warp((rest / 400 * 1_000) as u64 + 1, rest);
    let ix = e.mint_py_ix(&market, &share_mint, &u.pubkey(), 10 * USDC * SHARE, 0);
    e.must_fail(&[ix], &[&u], "Matured");
    let ix = e.market_settle_ix(&market, &pool, None, &payer);
    e.must(&[ix], &[]);
    let m: Market = e.acct("Market", &market);
    assert!(m.settled);
    assert_eq!(
        m.sy_for_pt + m.sy_for_yt,
        e.token_amount(&market_sy_vault(&market))
    );
    assert!(
        m.sy_for_yt > 0,
        "the pool's gain over the term is the YT side"
    );
    assert_eq!(m.rate_at_maturity_e18, rate_now);
    // After settlement the pair path is closed; each side redeems its share.
    let ix = e.redeem_py_ix(&market, &share_mint, &u.pubkey(), 1, 0);
    e.must_fail(&[ix], &[&u], "Settled");
    let held = e.token_amount(&ata(&u.pubkey(), &pt));
    let sy_before = e.token_amount(&ata(&u.pubkey(), &share_mint));
    let ix = e.redeem_side_ix(true, &market, &share_mint, &u.pubkey(), held);
    e.must(&[ix], &[&u]);
    let from_pt = e.token_amount(&ata(&u.pubkey(), &share_mint)) - sy_before;
    assert_eq!(
        from_pt, m.sy_for_pt,
        "the only PT holder takes the whole principal side"
    );
    // PT is worth its USDC face in SY at the maturity rate.
    let face = u128::from(from_pt) * m.rate_at_maturity_e18 / E18;
    assert!(
        face >= u128::from(held) - 1 && face <= u128::from(held) + 1,
        "face {face} vs {held}"
    );
    let ix = e.redeem_side_ix(false, &market, &share_mint, &u.pubkey(), held);
    e.must(&[ix], &[&u]);
    let total = e.token_amount(&ata(&u.pubkey(), &share_mint)) - sy_before;
    assert_eq!(total, m.sy_for_pt + m.sy_for_yt);
    assert_eq!(e.token_amount(&market_sy_vault(&market)), 0);
    // Fees sweep half and half to SY accounts owned by the treasury and buyback owners.
    let t = e.treasury_owner.pubkey();
    let b = buyback_owner(&t);
    e.create_ata(&t, &share_mint);
    e.create_ata(&b, &share_mint);
    let ix = e.sweep_split_fees_ix(&market, &share_mint);
    e.must(&[ix], &[]);
    assert_eq!(e.token_amount(&ata(&t, &share_mint)), m.fees / 2);
    assert_eq!(e.token_amount(&ata(&b, &share_mint)), m.fees - m.fees / 2);
    assert_eq!(e.token_amount(&market_fees(&market)), 0);
}

#[test]
fn venue_receipts_split_with_the_index() {
    let (mut e, _u, maturity) = pool_env();
    let maturity = midnight_from(maturity);
    let a = e.authority.insecure_clone();
    let b = e.benchmark;
    let ix = e.create_venue_ix(&b, &a.pubkey());
    e.must(&[ix], &[&a]);
    let venue = venue_pda(&b);
    let receipt = receipt_mint_pda(&venue);
    // A venue market must mature at a UTC midnight, and needs the venue's benchmark for its origin rate.
    let off = CreateMarketArgs {
        source_kind: SourceKind::BrinkVenue,
        maturity_ts: maturity + 3_600,
        min_mint: USDC,
    };
    let ix = e.create_market_ix(&a.pubkey(), &venue, Some(&b), &receipt, &off);
    e.must_fail(&[ix], &[&a], "Midnight");
    let on = CreateMarketArgs {
        maturity_ts: maturity,
        ..off
    };
    let ix = e.create_market_ix(&a.pubkey(), &venue, None, &receipt, &on);
    e.must_fail(&[ix], &[&a], "Benchmark");
    let market = market_over(
        &mut e,
        venue,
        Some(b),
        receipt,
        SourceKind::BrinkVenue,
        maturity,
    );
    let (pt, yt) = (market_pt(&market), market_yt(&market));
    let u = e.actor_with(10_000 * USDC, &[receipt, pt, yt]);
    let ix = e.place_ix(&b, &u.pubkey(), 1_000 * USDC, 0);
    e.must(&[ix], &[&u]);
    let ix = e.mint_py_ix(&market, &receipt, &u.pubkey(), 1_000 * USDC, 0);
    e.must(&[ix], &[&u]);
    let py = e.token_amount(&ata(&u.pubkey(), &pt));
    assert_eq!(py, 1_000 * USDC - 1_000 * USDC * 5 / 10_000);
    // Run to maturity and touch; the maturity rate is the venue's index at the maturity midnight, which the
    // touched venue also carries at that instant.
    let rest = maturity - e.clock().unix_timestamp;
    e.warp((rest / 400 * 1_000) as u64 + 1, rest);
    e.publish(684).unwrap();
    e.touch_until_current(&b);
    let v: Venue = e.acct("Venue", &venue);
    assert!(v.index_e18 > E18);
    let payer = e.payer.pubkey();
    // Settlement of a venue market needs the benchmark too.
    let ix = e.market_settle_ix(&market, &venue, None, &payer);
    e.must_fail(&[ix], &[], "Benchmark");
    let ix = e.market_settle_ix(&market, &venue, Some(&b), &payer);
    e.must(&[ix], &[]);
    let m: Market = e.acct("Market", &market);
    assert_eq!(m.rate_at_maturity_e18, v.index_e18);
    // 90 days at 684 bp: about 1.69 percent of the SY is yield.
    let total = m.sy_for_pt + m.sy_for_yt;
    assert!(
        m.sy_for_yt * 1_000 / total >= 16 && m.sy_for_yt * 1_000 / total <= 17,
        "yield share {}",
        m.sy_for_yt * 1_000 / total
    );
    let ix = e.redeem_side_ix(false, &market, &receipt, &u.pubkey(), py);
    e.must(&[ix], &[&u]);
    assert_eq!(e.token_amount(&ata(&u.pubkey(), &receipt)), m.sy_for_yt);
    let ix = e.redeem_side_ix(true, &market, &receipt, &u.pubkey(), py);
    e.must(&[ix], &[&u]);
    assert_eq!(e.token_amount(&ata(&u.pubkey(), &receipt)), total);
}

/// External scan 2, findings 2 and 3. Invariant: every pair is backed by the same SY whenever it was minted, so
/// a holder who mints late, after the index has risen, contributes the yield already embedded in their SY and
/// claims exactly it back: each holder's PT plus YT redeem to the SY they locked (net of the fee), and the early
/// minter's yield is untouched by the late one. Under live-rate pricing the late minter would have received
/// more pairs than their backing and taken part of the early minter's yield side.
#[test]
fn late_minters_take_only_their_own_yield() {
    let (mut e, _u, maturity) = pool_env();
    let maturity = midnight_from(maturity);
    let a = e.authority.insecure_clone();
    let b = e.benchmark;
    let ix = e.create_venue_ix(&b, &a.pubkey());
    e.must(&[ix], &[&a]);
    let venue = venue_pda(&b);
    let receipt = receipt_mint_pda(&venue);
    let market = market_over(
        &mut e,
        venue,
        Some(b),
        receipt,
        SourceKind::BrinkVenue,
        maturity,
    );
    let (pt, yt) = (market_pt(&market), market_yt(&market));
    let m: Market = e.acct("Market", &market);
    let origin = m.rate_origin_e18;
    assert_eq!(origin, E18, "a fresh venue is at par");
    // Early: locks 1,000 receipts at origin.
    let early = e.actor_with(10_000 * USDC, &[receipt, pt, yt]);
    let ix = e.place_ix(&b, &early.pubkey(), 1_000 * USDC, 0);
    e.must(&[ix], &[&early]);
    let ix = e.mint_py_ix(&market, &receipt, &early.pubkey(), 1_000 * USDC, 0);
    e.must(&[ix], &[&early]);
    let py_early = e.token_amount(&ata(&early.pubkey(), &pt));
    let sy_early = 1_000 * USDC - 1_000 * USDC * 5 / 10_000;
    // Half the term passes; the index rises.
    e.warp(45 * 216_000, 45 * DAY);
    e.publish(684).unwrap();
    let ix = e.touch_ix(&b);
    e.must(&[ix], &[]);
    let v: Venue = e.acct("Venue", &venue);
    assert!(v.index_e18 > E18 * 1_007 / 1_000, "index {}", v.index_e18);
    // Late: places 1,000 USDC, receiving fewer receipts, and locks them all. Pairs come at the origin rate: the
    // late minter holds fewer pairs than USDC locked, by exactly the yield their receipts already carry. The
    // reserve holds the early principal only, so the venue refuses the placement until the accrual it owes is
    // funded: a newcomer's principal never funds another holder's yield.
    let late = e.actor_with(10_000 * USDC, &[receipt, pt, yt]);
    let ix = e.place_ix(&b, &late.pubkey(), 1_000 * USDC, 0);
    e.must_fail(&[ix], &[&late], "Underfunded");
    let funder = e.new_actor(100 * USDC, false);
    let ix = e.fund_ix(&b, &funder.pubkey(), 100 * USDC);
    e.must(&[ix], &[&funder]);
    let ix = e.place_ix(&b, &late.pubkey(), 1_000 * USDC, 0);
    e.must(&[ix], &[&late]);
    let receipts_late = e.token_amount(&ata(&late.pubkey(), &receipt));
    assert!(receipts_late < 1_000 * USDC);
    let ix = e.mint_py_ix(&market, &receipt, &late.pubkey(), receipts_late, 0);
    e.must(&[ix], &[&late]);
    let py_late = e.token_amount(&ata(&late.pubkey(), &pt));
    let sy_late = receipts_late - (receipts_late * 5).div_ceil(10_000);
    assert_eq!(
        py_late, sy_late,
        "at an origin rate of one, a pair per SY unit"
    );
    assert!(py_late < 1_000 * USDC - 1_000 * USDC * 5 / 10_000);
    // To maturity; settle at the maturity midnight's index.
    let rest = maturity - e.clock().unix_timestamp;
    e.warp((rest / 400 * 1_000) as u64 + 1, rest);
    e.publish(684).unwrap();
    let ix = e.touch_ix(&b);
    e.must(&[ix], &[]);
    let payer = e.payer.pubkey();
    let ix = e.market_settle_ix(&market, &venue, Some(&b), &payer);
    e.must(&[ix], &[]);
    let m: Market = e.acct("Market", &market);
    assert!(m.sy_for_yt > 0);
    // Each holder redeems both sides: what comes back is the SY they locked, to a unit of rounding.
    for (who, py, locked) in [(&early, py_early, sy_early), (&late, py_late, sy_late)] {
        let before = e.token_amount(&ata(&who.pubkey(), &receipt));
        let ix = e.redeem_side_ix(true, &market, &receipt, &who.pubkey(), py);
        e.must(&[ix], &[who]);
        let ix = e.redeem_side_ix(false, &market, &receipt, &who.pubkey(), py);
        e.must(&[ix], &[who]);
        let out = e.token_amount(&ata(&who.pubkey(), &receipt)) - before;
        assert!(
            out + 2 >= locked && out <= locked,
            "pt plus yt redeem the backing: {out} vs {locked}"
        );
    }
    assert!(
        e.token_amount(&market_sy_vault(&market)) <= 2,
        "dust at most"
    );
}
