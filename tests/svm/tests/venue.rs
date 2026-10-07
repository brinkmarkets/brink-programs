//! Devnet venue: receipts at the index, the index compounding daily on the benchmark's midnight fixings, and
//! every redemption paid the fraction of all claims the reserve covers, never refused.
use brink_svm_tests::harness::*;
use brink_svm_tests::products::*;
use brink_svm_tests::*;
use solana_signer::Signer;

const E18: u128 = 1_000_000_000_000_000_000;

/// The venue index as a product of daily factors at a flat `bp`: simple accrual inside each UTC day, compounded
/// at every midnight between `from` and `to` (external scan 2, finding 1).
fn compounded(bp: u16, from: i64, to: i64) -> f64 {
    let per_day = f64::from(bp) / 10_000.0 / 365.0;
    let mut x = 1.0;
    let mut cur = from;
    loop {
        let midnight = (cur.div_euclid(DAY) + 1) * DAY;
        if midnight >= to {
            break;
        }
        x *= 1.0 + per_day * ((midnight - cur) as f64) / (DAY as f64);
        cur = midnight;
    }
    x * (1.0 + per_day * ((to - cur) as f64) / (DAY as f64))
}

fn venue_env() -> Env {
    let mut e = setup_products();
    let a = e.authority.insecure_clone();
    let b = e.benchmark;
    let ix = e.create_venue_ix(&b, &a.pubkey());
    e.must(&[ix], &[&a]);
    e
}

#[test]
fn only_the_upgrade_authority_creates_a_venue_and_it_starts_at_par() {
    let mut e = setup_products();
    let b = e.benchmark;
    let stranger = e.new_actor(0, false);
    let ix = e.create_venue_ix(&b, &stranger.pubkey());
    e.must_fail(&[ix], &[&stranger], "NotUpgradeAuthority");
    let a = e.authority.insecure_clone();
    let ix = e.create_venue_ix(&b, &a.pubkey());
    e.must(&[ix], &[&a]);
    let v: Venue = e.acct("Venue", &venue_pda(&b));
    assert_eq!(v.index_e18, E18);
    assert_eq!(v.benchmark, b);
    assert_eq!((v.principal, v.receipts, v.funded), (0, 0, 0));
    assert!(!v.paused);
}

#[test]
fn receipts_follow_the_benchmark_and_the_reserve_pays_pro_rata() {
    let mut e = venue_env();
    let b = e.benchmark;
    let venue = venue_pda(&b);
    let receipt = receipt_mint_pda(&venue);
    let u = e.actor_with(10_000 * USDC, &[receipt]);
    let t0 = e.clock().unix_timestamp;
    // Par: 1,000 USDC buys 1,000 receipts.
    let ix = e.place_ix(&b, &u.pubkey(), 1_000 * USDC, 1_000 * USDC);
    e.must(&[ix], &[&u]);
    assert_eq!(e.token_amount(&ata(&u.pubkey(), &receipt)), 1_000 * USDC);
    assert_eq!(e.token_amount(&reserve_pda(&venue)), 1_000 * USDC);
    // Half a year at 684 bp, published on the way and touched only now: the index is the product of the daily
    // factors over every midnight since creation, reconstructed from the fixings, not simple interest.
    for _ in 0..5 {
        e.warp(36_500 * 216, 36_500 * 86_400 / 1_000);
        e.publish(684).unwrap();
    }
    // 182 midnights is more than one walk bound: a placement is told to touch first, each touch advances the
    // anchor by the bound inside the default compute budget, and the walk completes in one call per bound.
    let ix = e.place_ix(&b, &u.pubkey(), 1_000 * USDC, 0);
    e.must_fail(&[ix], &[&u], "NeedsTouch");
    let ix = e.touch_ix(&b);
    e.must(&[ix], &[]);
    let v: Venue = e.acct("Venue", &venue);
    assert_eq!(
        v.anchor_day,
        u32::try_from(t0 / 86_400).unwrap() + MAX_WALK_DAYS
    );
    assert_eq!(e.touch_until_current(&b), 182 / MAX_WALK_DAYS);
    let v: Venue = e.acct("Venue", &venue);
    let t1 = e.clock().unix_timestamp;
    assert_eq!(v.last_ts, t1);
    let expected = (compounded(684, t0, t1) * 1e18) as u128;
    let simple = E18 + E18 * 684 * 182_500 / (10_000 * 365_000);
    assert!(
        v.index_e18.abs_diff(expected) * 1_000_000 < E18,
        "index {} vs compounded {expected}",
        v.index_e18
    );
    assert!(
        v.index_e18 > simple + E18 / 10_000,
        "compounding exceeds simple interest by more than a basis point: {} vs {simple}",
        v.index_e18
    );
    assert_eq!(v.version, 2);
    assert!(v.anchor_day > 0, "anchored at a midnight");
    // The reserve holds the principal only. Every redemption is paid the fraction of all claims it covers, so a
    // holder who asks for the accrual too is told by their own minimum, not refused by the venue.
    let claims = u128::from(1_000 * USDC) * v.index_e18 / E18;
    let ix = e.venue_redeem_ix(&b, &u.pubkey(), 1_000 * USDC, 1_030 * USDC);
    e.must_fail(&[ix], &[&u], "BelowMinimum");
    let ix = e.venue_redeem_ix(&b, &u.pubkey(), 900 * USDC, 0);
    e.must(&[ix], &[&u]);
    let got = e.token_amount(&ata(&u.pubkey(), &USDC_DEVNET)) - 9_000 * USDC;
    let funded_fraction = f64::from(1_000) / (claims as f64 / 1e6);
    let value = 900.0 * (v.index_e18 as f64 / 1e18);
    let paid = (value * funded_fraction * 1e6) as u64;
    assert!(
        got + 2 >= paid && got <= paid,
        "900 receipts worth {value:.6} paid at the funded fraction {funded_fraction:.6}: {got} vs {paid}"
    );
    assert!(
        got <= 900 * USDC && got + 2 >= 900 * USDC,
        "which is their principal here: {got}"
    );
    let v: Venue = e.acct("Venue", &venue);
    assert_eq!(v.receipts, 100 * USDC);
    assert_eq!(v.principal, 100 * USDC);
    // Anyone funds the reserve; once it covers every claim the rest redeems with its accrual in full.
    let funder = e.new_actor(1_000 * USDC, false);
    let ix = e.fund_ix(&b, &funder.pubkey(), 100 * USDC);
    e.must(&[ix], &[&funder]);
    let ix = e.venue_redeem_ix(&b, &u.pubkey(), 100 * USDC, 103 * USDC);
    e.must(&[ix], &[&u]);
    let v: Venue = e.acct("Venue", &venue);
    assert_eq!(v.receipts, 0);
    assert_eq!(v.principal, 0);
    assert_eq!(v.funded, 100 * USDC);
}

#[test]
fn placing_a_receipt_later_costs_the_index_and_pausing_blocks_new_placements() {
    let mut e = venue_env();
    let b = e.benchmark;
    let venue = venue_pda(&b);
    let receipt = receipt_mint_pda(&venue);
    let u = e.actor_with(10_000 * USDC, &[receipt]);
    e.warp(365 * 216_000, 365 * DAY);
    e.publish(684).unwrap();
    // A year untouched: three touches bring the anchor within the walk bound of today, the placement's own
    // accrual covers the rest. A year at 684 bp compounded daily: 1,070.8 USDC per 1,000 receipts (simple
    // interest would give 1,068.4), so 1,000 USDC buys about 934 receipts.
    let ix = e.place_ix(&b, &u.pubkey(), 1_000 * USDC, 0);
    e.must_fail(&[ix], &[&u], "NeedsTouch");
    assert_eq!(e.touch_until_current(&b), 365 / MAX_WALK_DAYS + 1);
    let ix = e.place_ix(&b, &u.pubkey(), 1_000 * USDC, 0);
    e.must(&[ix], &[&u]);
    let r = e.token_amount(&ata(&u.pubkey(), &receipt));
    assert!(r > 933_500_000 && r < 934_500_000, "receipts {r}");
    // A minimum above the index refuses.
    let ix = e.place_ix(&b, &u.pubkey(), 1_000 * USDC, 1_000 * USDC);
    e.must_fail(&[ix], &[&u], "BelowMinimum");
    // Paused: no placements, redemptions still run.
    let a = e.authority.insecure_clone();
    let stranger = e.new_actor(0, false);
    let ix = e.set_paused_ix(&b, &stranger.pubkey(), true);
    e.must_fail(&[ix], &[&stranger], "NotAuthority");
    let ix = e.set_paused_ix(&b, &a.pubkey(), true);
    e.must(&[ix], &[&a]);
    let ix = e.place_ix(&b, &u.pubkey(), 100 * USDC, 0);
    e.must_fail(&[ix], &[&u], "Paused");
    let ix = e.venue_redeem_ix(&b, &u.pubkey(), r, 0);
    e.must(&[ix], &[&u]);
    assert_eq!(e.token_amount(&ata(&u.pubkey(), &receipt)), 0);
}
