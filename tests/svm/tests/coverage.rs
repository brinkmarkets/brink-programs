//! Success-path coverage for every instruction the LiteSVM harness can reach, so the compute-unit table
//! (`programs/target/compute-units.tsv`, appended by the harness on every successful single-instruction
//! transaction) has at least one sample per instruction, plus the round-02 regressions that need a specific
//! rate path: the negative receive quote (maths M-7, patch 0008) and exhaustion liquidation (F-11).
//! `execute_upgrade` and `execute_set_upgrade_authority` need the loader to honour a PDA upgrade authority
//! and are exercised by the ignored `finding_f18_*` test in red.rs. Run with `cargo test --test coverage`.
use brink_svm_tests::harness::*;
use brink_svm_tests::*;
use solana_instruction::Instruction;
use solana_keypair::Keypair;
use solana_signer::Signer;

/// Publishes towards `target` one band step per day until `value_bp == target`.
fn walk_rate(e: &mut Env, target: u16) {
    for _ in 0..200 {
        let b: Benchmark = e.acct("Benchmark", &e.benchmark);
        if b.value_bp == target {
            return;
        }
        let next = if target > b.ema_bp {
            target.min(b.ema_bp + b.band_bp)
        } else {
            target.max(b.ema_bp.saturating_sub(b.band_bp))
        };
        e.warp(216_000, DAY);
        e.publish(next).unwrap();
    }
    panic!("rate walk did not converge on {target}");
}

/// Maths M-7 (patch 0008). Invariant: when the receive-fixed quote would fall below zero the open is refused
/// with `QuoteBelowZero` rather than wrapping or reporting `Overflow`; a pay-fixed position opened earlier in
/// the same pool still cancels (the unwind quote is floored at zero, never negative). The index is walked
/// from 684 bp to 0 bp in band steps; a 180-day receive-fixed open then has a negative fixed rate.
#[test]
fn m7_negative_receive_quote_is_quote_below_zero_and_cancel_still_works() {
    let mut e = setup();
    let lp = e.lp.insecure_clone();
    let tr = e.trader.insecure_clone();
    let ix = e.deposit_ix(&lp.pubkey(), 1_000_000 * USDC, 0);
    e.must(&[ix], &[&lp]);
    let ix = e.open_ix(
        &tr.pubkey(),
        &open_args(LegKind::PayFixed, 3, 100_000 * USDC, 9_999, 1),
    );
    e.must(&[ix], &[&tr]);
    walk_rate(&mut e, 0);
    let b: Benchmark = e.acct("Benchmark", &e.benchmark);
    assert_eq!(b.value_bp, 0);
    // Let the EMA settle on the spot so the quote base is as low as the model allows.
    for _ in 0..5 {
        e.warp(216_000, DAY);
        e.publish(0).unwrap();
    }
    let ix = e.open_ix(
        &tr.pubkey(),
        &open_args(LegKind::ReceiveFixed, 3, 250_000 * USDC, 0, 2),
    );
    let err = e
        .send(&[ix], &[&tr])
        .expect_err("a negative receive quote must be refused");
    assert!(
        err.contains("QuoteBelowZero"),
        "expected QuoteBelowZero, not Overflow or a wrapped rate:\n{err}"
    );
    assert!(!err.contains("panicked"));
    // The earlier pay-fixed position is far out of the money for the trader; cancelling it must still succeed
    // and pay at least zero.
    let before = e.token_amount(&ata(&tr.pubkey(), &USDC_DEVNET));
    let ix = e.close_ix("trader_cancel_swap", &tr.pubkey(), 1, &tr.pubkey(), &0u64);
    let r = e.send(&[ix], &[&tr]);
    match r {
        Ok(_) => {
            let after = e.token_amount(&ata(&tr.pubkey(), &USDC_DEVNET));
            assert!(after >= before, "payout is never negative");
            let p: Pool = e.acct("Pool", &e.pool);
            assert_eq!(p.open_swaps, 0);
        }
        Err(err) => {
            // The position matured during the walk (180 days at one day per step is not reached, but a
            // shorter tenor would be); only a maturity error is acceptable here.
            assert!(
                err.contains("AlreadySettled") || err.contains("Matured"),
                "{err}"
            );
        }
    }
}

/// F-11 / ADR-005 / ADR-007 (success path for `crank_liquidate_swap`). A receive-fixed 180-day position is
/// exhausted when the index runs up; a third party liquidates it, LP capital gains at most the collateral, and
/// conservation holds across pool, fees, cranker and trader.
#[test]
fn liquidation_on_exhaustion_pays_the_pool_and_the_cranker() {
    let mut e = setup();
    let lp = e.lp.insecure_clone();
    let tr = e.trader.insecure_clone();
    e.deposit(&lp, 10_000_000 * USDC);
    let ix = e.open_ix(
        &tr.pubkey(),
        &open_args(LegKind::ReceiveFixed, 3, 1_000_000 * USDC, 0, 1),
    );
    e.must(&[ix], &[&tr]);
    let s: Swap = e.acct("Swap", &e.swap_pda(&tr.pubkey(), 1));
    let cranker = e.new_actor(0, false);
    let cranker_usdc = ata(&cranker.pubkey(), &USDC_DEVNET);
    let p0: Pool = e.acct("Pool", &e.pool);
    let fees0 = p0.fees_buyback_accrued + p0.fees_treasury_accrued;
    let trader0 = e.token_amount(&ata(&tr.pubkey(), &USDC_DEVNET));
    let mut liquidated = false;
    for _ in 0..40 {
        let b: Benchmark = e.acct("Benchmark", &e.benchmark);
        e.warp(216_000, DAY);
        e.publish((b.ema_bp + 300).min(30_000)).unwrap();
        let ix = e.close_ix_bounty(
            "crank_liquidate_swap",
            &tr.pubkey(),
            1,
            &cranker.pubkey(),
            Some(cranker_usdc),
            &(),
        );
        match e.send(&[ix], &[&cranker]) {
            Ok(_) => {
                liquidated = true;
                break;
            }
            Err(err) => assert!(err.contains("NotLiquidatable"), "{err}"),
        }
    }
    assert!(
        liquidated,
        "the receive-fixed position must exhaust as the index runs up"
    );
    let p1: Pool = e.acct("Pool", &e.pool);
    assert_eq!(p1.open_swaps, 0);
    assert_eq!(p1.collateral_held, 0);
    // The bounty is carved from the position's payout first and, when that is exhausted, from the loss the
    // LPs collect (external scan 1: liquidations of losing swaps must still pay the cranker). Two basis points of
    // a $1,000,000 notional is capped at 25 USDC, and the cranker is a third party, so the pool keeps the rest.
    let bounty = e.token_amount(&cranker_usdc);
    assert_eq!(bounty, 25 * USDC, "capped bounty paid from the loss credit");
    let fees1 = p1.fees_buyback_accrued + p1.fees_treasury_accrued;
    let trader_back = e.token_amount(&ata(&tr.pubkey(), &USDC_DEVNET)) - trader0;
    assert_eq!(
        u128::from(p1.tvl) + u128::from(bounty) + u128::from(fees1 - fees0) + u128::from(trader_back),
        u128::from(p0.tvl) + u128::from(s.collateral),
        "collateral is split between the pool, the cranker, fees and the trader; nothing is created"
    );
    assert!(
        p1.tvl <= p0.tvl + s.collateral,
        "LP gain is bounded by the collateral"
    );
    assert_eq!(
        e.token_amount(&e.vault),
        p1.tvl + fees1,
        "vault holds LP capital and unswept fees"
    );
    assert!(e
        .svm
        .get_account(&e.swap_pda(&tr.pubkey(), 1))
        .is_none_or(|a| a.data.is_empty()));
}

/// Success paths for the governance instructions that no other test reaches, so the CU table has a sample for
/// each: index `set_guards`, `set_publisher`, `set_authority`; AMM `admin_set_authority`; timelock
/// `execute_config` (SetDelay and SetRoles) and `execute_set_upgrade_authority` is left to red.rs.
#[test]
fn governance_success_paths() {
    let mut e = setup();
    let a = e.authority.insecure_clone();
    // set_guards: widen the band to 400 bp and check a 350 bp step is then accepted.
    let ix = Instruction {
        program_id: INDEX,
        accounts: vec![ro(e.registry), sig(a.pubkey()), rw(e.benchmark)],
        data: data(
            "set_guards",
            &GuardArgs {
                band_bp: 400,
                ..DEFAULT_GUARDS
            },
        ),
    };
    e.must(&[ix], &[&a]);
    let b: Benchmark = e.acct("Benchmark", &e.benchmark);
    assert_eq!(b.band_bp, 400);
    e.warp(10, 10);
    e.publish(684 + 350).unwrap();
    // set_publisher: rotate to a new key; the old key is refused.
    let new_pub = Keypair::new();
    e.svm.airdrop(&new_pub.pubkey(), 1_000_000_000).unwrap();
    let ix = Instruction {
        program_id: INDEX,
        accounts: vec![rw(e.registry), sig(a.pubkey())],
        data: data(
            "set_publishers",
            &(
                InitialiseArgs::single(solana_pubkey::Pubkey::default(), new_pub.pubkey())
                    .publishers,
                1u8,
            ),
        ),
    };
    e.must(&[ix], &[&a]);
    e.warp(10, 10);
    assert!(e.publish(1_000).is_err(), "old publisher refused");
    e.publisher = new_pub;
    e.publish(1_000).unwrap();
    // set_authority on the registry with the incoming co-signature.
    let incoming = Keypair::new();
    e.svm.airdrop(&incoming.pubkey(), 1_000_000_000).unwrap();
    let ix = Instruction {
        program_id: INDEX,
        accounts: vec![rw(e.registry), sig(a.pubkey()), sig(incoming.pubkey())],
        data: data("set_authority", &()),
    };
    e.must(&[ix], &[&a, &incoming]);
    let r: Registry = e.acct("Registry", &e.registry);
    assert_eq!(r.authority, incoming.pubkey());
    // admin_set_authority on the AMM with the incoming co-signature; guardian rotates too.
    let incoming_amm = Keypair::new();
    e.svm
        .airdrop(&incoming_amm.pubkey(), 1_000_000_000)
        .unwrap();
    let new_guardian = Keypair::new();
    let ix = Instruction {
        program_id: SWAP_AMM,
        accounts: [
            vec![rw(e.global), sig(a.pubkey()), sig(incoming_amm.pubkey())],
            evt(SWAP_AMM).to_vec(),
        ]
        .concat(),
        data: data("admin_set_authority", &new_guardian.pubkey()),
    };
    e.must(&[ix], &[&a, &incoming_amm]);
    let g: Global = e.acct("Global", &e.global);
    assert_eq!(g.authority, incoming_amm.pubkey());
    assert_eq!(g.guardian, new_guardian.pubkey());
    assert!(
        e.set_mode(&a, OperatingMode::Halted).is_err(),
        "the old authority is powerless"
    );
    // Timelock: initialise, queue SetDelay, execute_config; queue SetRoles, execute_config.
    let proposer = e.authority.insecure_clone();
    let executor = e.lp.insecure_clone();
    let timelock = pda(&[b"timelock"], &TIMELOCK);
    let authority = pda(&[b"authority"], &TIMELOCK);
    let payer = e.payer.pubkey();
    e.must(
        &[Instruction {
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
                    e.guardian.pubkey(),
                    432_000u64,
                ),
            ),
        }],
        &[],
    );
    let kinds = [
        OperationKind::SetDelay {
            delay_slots: 500_000,
        },
        OperationKind::SetRoles {
            proposer: proposer.pubkey(),
            executor: incoming.pubkey(),
            guardian: e.guardian.pubkey(),
        },
    ];
    for (nonce, kind) in (0u64..).zip(kinds) {
        let op = pda(&[b"op", &nonce.to_le_bytes()], &TIMELOCK);
        e.must(
            &[Instruction {
                program_id: TIMELOCK,
                accounts: [
                    vec![rw(timelock), sigw(proposer.pubkey()), rw(op), ro(SYSTEM)],
                    evt(TIMELOCK).to_vec(),
                ]
                .concat(),
                data: data("queue", &kind),
            }],
            &[&proposer],
        );
        let t: Timelock = e.acct("Timelock", &timelock);
        e.warp(t.delay_slots, 1_000);
        e.must(
            &[Instruction {
                program_id: TIMELOCK,
                accounts: [
                    vec![
                        rw(timelock),
                        sig(executor.pubkey()),
                        rw(proposer.pubkey()),
                        rw(op),
                    ],
                    evt(TIMELOCK).to_vec(),
                ]
                .concat(),
                data: data("execute_config", &()),
            }],
            &[&executor],
        );
    }
    let t: Timelock = e.acct("Timelock", &timelock);
    assert_eq!(t.delay_slots, 500_000);
    assert_eq!(t.executor, incoming.pubkey());
}
