//! Stacked Treasuries reserve: idle LP capital placed at the venue, a working balance kept for payouts, yield
//! harvested into LP capital with a bounty from yield only, recalls never delayed, conservation throughout.
use brink_svm_tests::harness::*;
use brink_svm_tests::products::*;
use brink_svm_tests::*;
use solana_signer::Signer;

fn params() -> ReserveParams {
    ReserveParams {
        working_bps: 2_000,
        band_bps: 200,
        max_place_bps: 7_000,
        min_interval_slots: 0,
        bounty_cap: USDC,
    }
}

/// Pool with 100,000 USDC of LP capital, a venue on its benchmark and the reserve enabled.
fn reserve_env() -> (Env, solana_keypair::Keypair) {
    let mut e = setup_products();
    let a = e.authority.insecure_clone();
    let b = e.benchmark;
    let ix = e.create_venue_ix(&b, &a.pubkey());
    e.must(&[ix], &[&a]);
    let lp = e.new_actor(1_000_000 * USDC, true);
    e.deposit(&lp, 100_000 * USDC);
    let ix = e.enable_reserve_ix(&b, &a.pubkey(), &params());
    e.must(&[ix], &[&a]);
    (e, lp)
}

fn conserved(e: &Env) {
    let p: Pool = e.acct("Pool", &e.pool);
    let vault = e.token_amount(&e.vault);
    let accounted = p.tvl
        + p.collateral_held
        + p.fees_buyback_accrued
        + p.fees_treasury_accrued
        + p.withdraw_reserved;
    assert!(
        vault + p.reserve_placed >= accounted,
        "vault {vault} + placed {} < accounted {accounted}",
        p.reserve_placed
    );
}

#[test]
fn only_the_authority_enables_a_reserve_and_the_parameters_are_bounded() {
    let mut e = setup_products();
    let a = e.authority.insecure_clone();
    let b = e.benchmark;
    let ix = e.create_venue_ix(&b, &a.pubkey());
    e.must(&[ix], &[&a]);
    let stranger = e.new_actor(0, false);
    let ix = e.enable_reserve_ix(&b, &stranger.pubkey(), &params());
    e.must_fail(&[ix], &[&stranger], "ConstraintHasOne");
    // Working balance under five percent, a band wider than the target, a ceiling that overlaps the working
    // balance and a bounty cap above ten USDC are all refused.
    for bad in [
        ReserveParams {
            working_bps: 400,
            ..params()
        },
        ReserveParams {
            band_bps: 2_500,
            ..params()
        },
        ReserveParams {
            max_place_bps: 8_500,
            ..params()
        },
        ReserveParams {
            bounty_cap: 11 * USDC,
            ..params()
        },
    ] {
        let ix = e.enable_reserve_ix(&b, &a.pubkey(), &bad);
        e.must_fail(&[ix], &[&a], "ReserveParams");
    }
    let ix = e.enable_reserve_ix(&b, &a.pubkey(), &params());
    e.must(&[ix], &[&a]);
    let r: PoolReserve = e.acct("PoolReserve", &pool_reserve_pda(&e.pool));
    assert_eq!(r.pool, e.pool);
    assert_eq!(r.venue, venue_pda(&b));
    assert_eq!(r.receipts, pool_receipts_pda(&e.pool));
    assert_eq!(r.params, params());
    assert!(!r.paused);
    let p: Pool = e.acct("Pool", &e.pool);
    assert_eq!((p.reserve_active, p.reserve_placed), (1, 0));
    // Enabling twice is refused: the PDA exists.
    let ix = e.enable_reserve_ix(&b, &a.pubkey(), &params());
    e.must_fail(&[ix], &[&a], "already in use");
}

#[test]
fn the_crank_places_the_excess_up_to_the_ceiling_and_then_rests() {
    let (mut e, _lp) = reserve_env();
    let b = e.benchmark;
    let cranker = e.new_actor(0, false);
    // Working target 20,000 of 100,000; excess 80,000; ceiling 70,000: 70,000 goes to the venue at par.
    let ix = e.rebalance_reserve_ix(&b, &cranker.pubkey());
    e.must(&[ix], &[&cranker]);
    let p: Pool = e.acct("Pool", &e.pool);
    assert_eq!(p.reserve_placed, 70_000 * USDC);
    assert_eq!(p.tvl, 100_000 * USDC);
    assert_eq!(e.token_amount(&e.vault), 30_000 * USDC);
    assert_eq!(e.token_amount(&pool_receipts_pda(&e.pool)), 70_000 * USDC);
    let v: Venue = e.acct("Venue", &venue_pda(&b));
    assert_eq!(v.principal, 70_000 * USDC);
    let r: PoolReserve = e.acct("PoolReserve", &pool_reserve_pda(&e.pool));
    assert_eq!(
        (r.placed_lifetime, r.recalled_lifetime, r.rebalances),
        (70_000 * USDC, 0, 1)
    );
    conserved(&e);
    // Within the band and nothing accrued: the crank has nothing to do and says so.
    let ix = e.rebalance_reserve_ix(&b, &cranker.pubkey());
    e.must_fail(&[ix], &[&cranker], "ReserveBalanced");
    // No bounty was paid for moving principal.
    assert_eq!(e.token_amount(&ata(&cranker.pubkey(), &USDC_DEVNET)), 0);
}

#[test]
fn a_payout_beyond_the_working_balance_is_named_and_a_recall_restores_the_balance() {
    let (mut e, lp) = reserve_env();
    let b = e.benchmark;
    let cranker = e.new_actor(0, false);
    let ix = e.rebalance_reserve_ix(&b, &cranker.pubkey());
    e.must(&[ix], &[&cranker]);
    // A pool with an active reserve is priced through the venue (external scan 2, finding 6): the bare
    // instruction is refused by name rather than failing in the token program or pricing the exit short.
    let ix = e.withdraw_ix(&lp.pubkey(), 50_000 * USDC * SHARE, 0);
    e.must_fail(&[ix], &[&lp], "ReserveAccountsRequired");
    // A set of reserve accounts that is not the pool's own is refused.
    let mut forged = e.withdraw_ix(&lp.pubkey(), 50_000 * USDC * SHARE, 0);
    let mut extra = e.reserve_remaining(&b);
    extra[1] = rw(ata(&lp.pubkey(), &receipt_mint_pda(&venue_pda(&b))));
    forged.accounts.extend(extra);
    e.must_fail(&[forged], &[&lp], "ReserveVenue");
    // 25,000 fits the 30,000 working balance without a recall: it drops to 5,000 against a 15,000 target on
    // 75,000 of capital.
    let ix = e.withdraw_with_reserve_ix(&b, &lp.pubkey(), 25_000 * USDC * SHARE, 0);
    e.must(&[ix], &[&lp]);
    let p: Pool = e.acct("Pool", &e.pool);
    assert_eq!(p.tvl, 75_000 * USDC);
    assert_eq!(e.token_amount(&e.vault), 5_000 * USDC);
    conserved(&e);
    // The crank recalls the 10,000 shortfall from the venue.
    let ix = e.rebalance_reserve_ix(&b, &cranker.pubkey());
    e.must(&[ix], &[&cranker]);
    let p: Pool = e.acct("Pool", &e.pool);
    // Receipts are rounded up on a recall, so a unit or two of yield can come back with the principal; it is
    // LP capital, never a reduction of the principal recorded as placed.
    let vault = e.token_amount(&e.vault);
    assert!(
        (15_000 * USDC..=15_000 * USDC + 2).contains(&vault),
        "vault {vault}"
    );
    assert!(
        p.reserve_placed.abs_diff(60_000 * USDC) <= 2,
        "placed {}",
        p.reserve_placed
    );
    assert_eq!(p.tvl, 75_000 * USDC);
    let r: PoolReserve = e.acct("PoolReserve", &pool_reserve_pda(&e.pool));
    assert!(r.recalled_lifetime.abs_diff(10_000 * USDC) <= 2);
    conserved(&e);
}

#[test]
fn accrued_yield_is_harvested_into_lp_capital_and_the_cranker_is_paid_from_it() {
    let (mut e, _lp) = reserve_env();
    let b = e.benchmark;
    let cranker = e.new_actor(0, false);
    let ix = e.rebalance_reserve_ix(&b, &cranker.pubkey());
    e.must(&[ix], &[&cranker]);
    // Half a year at 684 bp, published along the way.
    for _ in 0..5 {
        e.warp(36_500 * 216, 36_500 * 86_400 / 1_000);
        e.publish(684).unwrap();
    }
    // Half a year untouched is more than the venue's walk bound: the crank's own touch advances the anchor by
    // one bound and the crank refuses to value the position against an index that is not at now; touching the
    // venue until current takes one call per bound.
    let ix = e.rebalance_reserve_ix(&b, &cranker.pubkey());
    e.must_fail(&[ix], &[&cranker], "VenueNeedsTouch");
    assert_eq!(e.touch_until_current(&b), 182 / MAX_WALK_DAYS + 1);
    // The venue's reserve holds principal only: the yield is not funded yet, so nothing is harvested and the
    // working balance is in band; the crank reports nothing to do rather than paying yield out of principal.
    let ix = e.rebalance_reserve_ix(&b, &cranker.pubkey());
    e.must_fail(&[ix], &[&cranker], "ReserveBalanced");
    // Fund the venue; the next crank harvests the daily-compounded half year on 70,000 at 684 bp, about 2,435
    // USDC (simple interest would give 2,394), less the cranker's bounty.
    let funder = e.new_actor(10_000 * USDC, false);
    let ix = e.fund_ix(&b, &funder.pubkey(), 5_000 * USDC);
    e.must(&[ix], &[&funder]);
    let tvl_before = e.acct::<Pool>("Pool", &e.pool).tvl;
    let ix = e.rebalance_reserve_ix(&b, &cranker.pubkey());
    e.must(&[ix], &[&cranker]);
    let p: Pool = e.acct("Pool", &e.pool);
    let r: PoolReserve = e.acct("PoolReserve", &pool_reserve_pda(&e.pool));
    let bounty = e.token_amount(&ata(&cranker.pubkey(), &USDC_DEVNET));
    assert_eq!(
        bounty, USDC,
        "bounty is one percent of the harvest, capped at the parameter"
    );
    let gained = p.tvl - tvl_before;
    assert!(
        gained > 2_425 * USDC && gained < 2_445 * USDC,
        "gained {gained}"
    );
    // The same crank's placement charges the unit of receipt rounding to capital (external scan 2, finding 22),
    // so capital gains the realised yield less at most one unit.
    assert!(
        r.yield_realised >= gained && r.yield_realised - gained <= 1,
        "realised {} gained {gained}",
        r.yield_realised
    );
    assert_eq!(r.bounties_paid, USDC);
    // The harvest lifts LP capital, so the same crank places the new excess up to the ceiling on the larger
    // capital base, each placement's rounding unit charged to capital as it is booked; the receipts left are
    // worth the principal placed within a cent.
    let ceiling = p.tvl * 7_000 / 10_000;
    assert!(
        p.reserve_placed <= ceiling && ceiling - p.reserve_placed <= 2,
        "placed {} ceiling {ceiling}",
        p.reserve_placed
    );
    assert_eq!(r.placed_lifetime, p.reserve_placed);
    let v: Venue = e.acct("Venue", &venue_pda(&b));
    let held = e.token_amount(&pool_receipts_pda(&e.pool));
    let worth = (u128::from(held) * v.index_e18 / 1_000_000_000_000_000_000) as u64;
    assert!(
        worth.abs_diff(p.reserve_placed) < 10_000,
        "worth {worth} placed {}",
        p.reserve_placed
    );
    conserved(&e);
}

#[test]
fn a_paused_reserve_places_nothing_but_still_recalls_a_shortfall() {
    let (mut e, lp) = reserve_env();
    let b = e.benchmark;
    let a = e.authority.insecure_clone();
    let cranker = e.new_actor(0, false);
    let ix = e.rebalance_reserve_ix(&b, &cranker.pubkey());
    e.must(&[ix], &[&cranker]);
    // Pause: a stranger may not, the authority may.
    let stranger = e.new_actor(0, false);
    let ix = e.set_reserve_ix(&stranger.pubkey(), &params(), true);
    e.must_fail(&[ix], &[&stranger], "ConstraintHasOne");
    let ix = e.set_reserve_ix(&a.pubkey(), &params(), true);
    e.must(&[ix], &[&a]);
    // More capital arrives; the excess is not placed while paused.
    let ix = e.deposit_with_reserve_ix(&b, &lp.pubkey(), 50_000 * USDC, 0);
    e.must(&[ix], &[&lp]);
    let ix = e.rebalance_reserve_ix(&b, &cranker.pubkey());
    e.must_fail(&[ix], &[&cranker], "ReserveBalanced");
    let p: Pool = e.acct("Pool", &e.pool);
    assert_eq!(p.reserve_placed, 70_000 * USDC);
    // A shortfall is recalled regardless of the pause: withdraw most of the working balance first.
    let ix = e.withdraw_with_reserve_ix(&b, &lp.pubkey(), 70_000 * USDC * SHARE, 0);
    e.must(&[ix], &[&lp]);
    let ix = e.rebalance_reserve_ix(&b, &cranker.pubkey());
    e.must(&[ix], &[&cranker]);
    let p: Pool = e.acct("Pool", &e.pool);
    assert!(p.reserve_placed < 70_000 * USDC);
    let target = p.tvl * 2_000 / 10_000;
    let vault = e.token_amount(&e.vault);
    assert!(
        vault >= target && vault <= target + 2,
        "vault {vault} target {target}"
    );
    conserved(&e);
    // Unpause with a tighter ceiling: the crank now recalls down to it.
    let tighter = ReserveParams {
        max_place_bps: 1_000,
        ..params()
    };
    let ix = e.set_reserve_ix(&a.pubkey(), &tighter, false);
    e.must(&[ix], &[&a]);
    let r: PoolReserve = e.acct("PoolReserve", &pool_reserve_pda(&e.pool));
    assert_eq!(r.params, tighter);
    assert!(!r.paused);
}

#[test]
fn swaps_settle_normally_with_a_reserve_in_place() {
    let (mut e, _lp) = reserve_env();
    let b = e.benchmark;
    let cranker = e.new_actor(0, false);
    let ix = e.rebalance_reserve_ix(&b, &cranker.pubkey());
    e.must(&[ix], &[&cranker]);
    // A trader pays fixed on 20,000 for 28 days; collateral lands in the vault and is excluded from the excess.
    let trader = e.new_actor(100_000 * USDC, false);
    let args = open_args(LegKind::PayFixed, 2, 20_000 * USDC, 800, 7);
    let ix = e.open_ix(&trader.pubkey(), &args);
    e.must(&[ix], &[&trader]);
    conserved(&e);
    let before = e.acct::<Pool>("Pool", &e.pool);
    // Crank once more: the collateral and fees are kept liquid, so nothing beyond the band moves.
    let ix = e.rebalance_reserve_ix(&b, &cranker.pubkey());
    let _ = e.send(&[ix], &[&cranker]);
    let after = e.acct::<Pool>("Pool", &e.pool);
    let vault = e.token_amount(&e.vault);
    assert!(
        vault >= after.collateral_held + after.fees_buyback_accrued + after.fees_treasury_accrued,
        "working balance covers collateral and fees"
    );
    assert_eq!(after.collateral_held, before.collateral_held);
    // Matured after ninety days: settlement pays from the working balance.
    e.warp(45 * 216_000, 45 * DAY);
    e.publish(900).unwrap();
    e.warp(45 * 216_000, 45 * DAY + 1);
    e.publish(900).unwrap();
    let ix = e.close_ix(
        "crank_settle_swap",
        &trader.pubkey(),
        7,
        &e.payer.pubkey(),
        &(),
    );
    e.must(&[ix], &[]);
    let p: Pool = e.acct("Pool", &e.pool);
    assert_eq!(p.open_swaps, 0);
    conserved(&e);
}

#[test]
fn an_exit_beyond_the_working_balance_recalls_inline_when_the_reserve_accounts_are_supplied() {
    let (mut e, lp) = reserve_env();
    let b = e.benchmark;
    let cranker = e.new_actor(0, false);
    let ix = e.rebalance_reserve_ix(&b, &cranker.pubkey());
    e.must(&[ix], &[&cranker]);
    let lp_before = e.token_amount(&ata(&lp.pubkey(), &USDC_DEVNET));
    // 50,000 against a 30,000 working balance: the pool recalls the 20,000 it lacks from the venue in the same
    // instruction, so the LP leaves in one transaction.
    let ix = e.withdraw_with_reserve_ix(&b, &lp.pubkey(), 50_000 * USDC * SHARE, 0);
    e.must(&[ix], &[&lp]);
    let p: Pool = e.acct("Pool", &e.pool);
    let got = e.token_amount(&ata(&lp.pubkey(), &USDC_DEVNET)) - lp_before;
    assert_eq!(got, 50_000 * USDC);
    assert_eq!(p.tvl, 50_000 * USDC);
    assert!(
        p.reserve_placed.abs_diff(50_000 * USDC) <= 2,
        "20,000 of principal came back: {}",
        p.reserve_placed
    );
    let vault = e.token_amount(&e.vault);
    assert!(
        vault <= 2,
        "the working balance is spent to the unit; the next crank restores it: {vault}"
    );
    let r: PoolReserve = e.acct("PoolReserve", &pool_reserve_pda(&e.pool));
    assert!(r.recalled_lifetime.abs_diff(20_000 * USDC) <= 2);
    conserved(&e);
    // The next crank recalls the working target back from the venue.
    let ix = e.rebalance_reserve_ix(&b, &cranker.pubkey());
    e.must(&[ix], &[&cranker]);
    let p: Pool = e.acct("Pool", &e.pool);
    let target = p.tvl * 2_000 / 10_000;
    let vault = e.token_amount(&e.vault);
    assert!(
        vault >= target && vault <= target + 2,
        "vault {vault} target {target}"
    );
    conserved(&e);
    // The last LP leaves the same way: the whole principal comes back and the pool is left with no reserve placed.
    let ix = e.withdraw_with_reserve_ix(&b, &lp.pubkey(), 50_000 * USDC * SHARE, 0);
    e.must(&[ix], &[&lp]);
    let p: Pool = e.acct("Pool", &e.pool);
    assert!(p.tvl < 10_000, "tvl {}", p.tvl);
    assert_eq!(p.reserve_placed, 0);
    conserved(&e);
}

#[test]
fn the_sole_lp_leaves_with_its_pending_yield_in_one_transaction() {
    // Review of external scan 2, A-2: an exit priced above `tvl` on yield the reserve has accrued but not yet
    // booked realises that yield inline, without a bounty, and pays the whole position.
    let (mut e, lp) = reserve_env();
    let b = e.benchmark;
    let cranker = e.new_actor(0, false);
    let ix = e.rebalance_reserve_ix(&b, &cranker.pubkey());
    e.must(&[ix], &[&cranker]);
    let p: Pool = e.acct("Pool", &e.pool);
    assert!(
        p.reserve_placed > 69_000 * USDC,
        "placed {}",
        p.reserve_placed
    );
    // A month at 684 bp, published and funded at the venue.
    e.warp(30 * 216_000, 30 * DAY);
    e.publish(684).unwrap();
    let funder = e.new_actor(10_000 * USDC, false);
    let ix = e.fund_ix(&b, &funder.pubkey(), 2_000 * USDC);
    e.must(&[ix], &[&funder]);
    // The LP holds every share, so the exit is priced at capital plus the pending yield, above `tvl`: the
    // withdrawal harvests the yield into capital, recalls the principal and pays in one transaction. A month
    // on 70,000 at 684 bp compounded daily is about 395 USDC.
    let lp_before = e.token_amount(&ata(&lp.pubkey(), &USDC_DEVNET));
    // Two venue touches, a redemption for the yield and a recall of the principal sit inside the one exit, which
    // is more than the default budget: the app sizes the request from a simulation, as here.
    let ix = e.withdraw_with_reserve_ix(&b, &lp.pubkey(), 100_000 * USDC * SHARE, 0);
    e.must(&[budget_ix(600_000), ix], &[&lp]);
    let got = e.token_amount(&ata(&lp.pubkey(), &USDC_DEVNET)) - lp_before;
    assert!(
        got > 100_380 * USDC && got < 100_410 * USDC,
        "the whole position with its yield: {got}"
    );
    let p: Pool = e.acct("Pool", &e.pool);
    assert!(p.tvl < 10_000, "dust only: tvl {}", p.tvl);
    // The recall burns receipts for the principal it wants plus one; the venue's floored payout can leave a unit
    // of principal on the books, still backed by the receipt units the pool holds.
    let held = e.token_amount(&pool_receipts_pda(&e.pool));
    let v: Venue = e.acct("Venue", &venue_pda(&b));
    assert!(p.reserve_placed <= 2, "placed {}", p.reserve_placed);
    let backing = (u128::from(held) * v.index_e18 / 1_000_000_000_000_000_000) as u64;
    assert!(
        backing + 1 >= p.reserve_placed,
        "what is left placed is backed by receipts: held {held} placed {}",
        p.reserve_placed
    );
    let r: PoolReserve = e.acct("PoolReserve", &pool_reserve_pda(&e.pool));
    assert!(
        r.yield_realised > 380 * USDC,
        "realised {}",
        r.yield_realised
    );
    assert_eq!(r.bounties_paid, 0, "an inline harvest pays no bounty");
    conserved(&e);
}

#[test]
fn the_crank_rests_rather_than_place_into_a_short_venue_and_its_touch_is_an_event() {
    // Review of external scan 2, B-1 and A-6: the venue refuses placements while its reserve is short of the
    // claims it owes, the AMM's crank applies the same gate before placing, and a touch made through a CPI is
    // recorded through the venue's event authority.
    let (mut e, _lp) = reserve_env();
    let b = e.benchmark;
    let venue = venue_pda(&b);
    let cranker = e.new_actor(0, false);
    let ix = e.rebalance_reserve_ix(&b, &cranker.pubkey());
    e.must(&[ix], &[&cranker]);
    // A week later the receipts are worth more than the principal in the venue's reserve.
    e.warp(7 * 216_000, 7 * DAY);
    e.publish(684).unwrap();
    let receipt = receipt_mint_pda(&venue);
    let u = e.actor_with(10_000 * USDC, &[receipt]);
    let ix = e.place_ix(&b, &u.pubkey(), 1_000 * USDC, 0);
    e.must_fail(&[ix], &[&u], "Underfunded");
    // Redemptions continue at the funded fraction; nothing is refused on the way out.
    let ix = e.venue_redeem_ix(&b, &u.pubkey(), 0, 0);
    e.must_fail(&[ix], &[&u], "Zero");
    // A second LP lifts capital to 150,000, so the ceiling has room and the working balance is above its band:
    // the crank harvests nothing (the yield is not funded) and would place the excess, but with the venue short
    // it rests instead of placing LP principal into it.
    let lp2 = e.new_actor(1_000_000 * USDC, true);
    let ix = e.deposit_with_reserve_ix(&b, &lp2.pubkey(), 50_000 * USDC, 0);
    e.must(&[ix], &[&lp2]);
    let placed_before = e.acct::<Pool>("Pool", &e.pool).reserve_placed;
    let ix = e.rebalance_reserve_ix(&b, &cranker.pubkey());
    e.must_fail(&[ix], &[&cranker], "ReserveBalanced");
    assert_eq!(
        e.acct::<Pool>("Pool", &e.pool).reserve_placed,
        placed_before
    );
    // Funding the venue reopens placements for everyone, the crank included.
    let funder = e.new_actor(1_000 * USDC, false);
    let ix = e.fund_ix(&b, &funder.pubkey(), 500 * USDC);
    e.must(&[ix], &[&funder]);
    let ix = e.place_ix(&b, &u.pubkey(), 1_000 * USDC, 0);
    e.must(&[ix], &[&u]);
    // The crank's own touch of the venue is a CPI; its `Touched` event travels by self-CPI through the venue's
    // event authority, so the log shows the venue invoked at depth two by the crank and again at depth three by
    // itself for the event, where the first build logged nothing a reader could index.
    let ix = e.rebalance_reserve_ix(&b, &cranker.pubkey());
    let logs = e.must(&[ix], &[&cranker]);
    assert!(
        e.acct::<Pool>("Pool", &e.pool).reserve_placed > placed_before,
        "the funded venue takes the excess"
    );
    let touched = logs
        .iter()
        .position(|l| l.contains("Instruction: Touch"))
        .expect("the crank touches the venue");
    assert!(
        logs[touched..]
            .iter()
            .take_while(|l| !l.contains(&format!("Program {VENUE} success")))
            .any(|l| l.contains(&format!("Program {VENUE} invoke [3]"))),
        "the touch emits its event by self-CPI:\n{}",
        logs.join("\n")
    );
    conserved(&e);
}
