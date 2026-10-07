//! Red tests: one ignored test per open finding of round 01 and of the maths and simulation reviews. Each test
//! states the invariant the fix must establish in its doc comment and asserts it against the compiled programs,
//! so the test is red today and turns green when the corresponding patch is applied. Run them with
//! `cargo test --test red -- --ignored`; a test that passes there must be un-ignored in the same change as the
//! fix. Identifiers: F-nn (programs review), M-nn (maths review), S-nn (simulation report), ADR-nnn.
use brink_svm_tests::harness::*;
use brink_svm_tests::*;
use solana_account::Account;
use solana_instruction::{AccountMeta, Instruction};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signer::Signer;

/// Publishes towards `target` one band step per day until the value is `target`.
#[allow(dead_code)]
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

/// F-12 / ADR-006. Invariant: the settlement value of a swap is a function of the published path up to maturity
/// only. Patch 0002 recovers the accrual at maturity when exactly one publish lands after maturity; when two or
/// more land before the crank, `accrual_at` has no history and settlement fails with
/// `BenchmarkHistoryUnavailable`, so a position can be blocked from settling by the publisher's cadence. The
/// fixings ring buffer (or checkpoint PDA) must make this settlement succeed at exactly the flat-rate loss.
#[test]
fn finding_f12_two_post_maturity_publishes_must_not_block_or_inflate_settlement() {
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
    e.warp(30 * 216_000, 30 * DAY);
    e.publish(684).unwrap();
    e.warp(216_000, DAY);
    e.publish(684).unwrap();
    let before = e.token_amount(&ata(&tr.pubkey(), &USDC_DEVNET));
    let ix = e.close_ix("crank_settle_swap", &tr.pubkey(), 1, &tr.pubkey(), &());
    let r = e.send(&[ix], &[&tr]);
    assert!(
        r.is_ok(),
        "settlement must not depend on how many publishes landed after maturity: {r:?}"
    );
    let payout = e.token_amount(&ata(&tr.pubkey(), &USDC_DEVNET)) - before;
    let fair_loss = u128::from(s.fixed_bp - 684) * u128::from(s.notional) * 28 / (10_000 * 365);
    assert_eq!(u128::from(s.collateral) - u128::from(payout), fair_loss);
}

/// F-13 / ADR-011. Invariant: once `Global.authority` is the timelock's `["authority"]` PDA, protocol
/// administration (`admin_set_mode`, `admin_queue_calibration`, `admin_set_authority`) is executable through a
/// queued, delayed, cancellable timelock operation signed by that PDA. The handover itself goes through the same
/// path because `admin_set_authority` needs the incoming authority's signature.
#[test]
fn finding_f13_timelock_can_execute_protocol_administration() {
    let mut e = setup();
    let proposer = e.authority.insecure_clone();
    let executor = e.lp.insecure_clone();
    let guardian = e.guardian.insecure_clone();
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
                    guardian.pubkey(),
                    432_000u64,
                ),
            ),
        }],
        &[],
    );
    let op = |n: u64| pda(&[b"op", &n.to_le_bytes()], &TIMELOCK);
    // Step 1: hand Global.authority to the PDA through a timelocked invoke of admin_set_authority. The PDA is the
    // incoming authority (a signer in the inner instruction); the current authority co-signs the outer tx.
    let inner_data = data("admin_set_authority", &guardian.pubkey());
    // The proposer is also the rent destination of the operation account (E2 0004), so the transaction carries
    // it writable; the queued hash must use the flags the runtime will present, signer and writable.
    let metas = vec![
        (e.global, false, true),
        (proposer.pubkey(), true, true),
        (authority, true, false),
    ];
    // External scan 2, finding 18: an invoke is queued with its account list and data disclosed; the generic
    // `queue` refuses the opaque kind.
    let kind = OperationKind::Invoke {
        program: SWAP_AMM,
        accounts_hash: invoke_accounts_hash(&SWAP_AMM, &metas),
        data_hash: sha256(&inner_data),
    };
    let opaque = Instruction {
        program_id: TIMELOCK,
        accounts: [
            vec![rw(timelock), sigw(proposer.pubkey()), rw(op(0)), ro(SYSTEM)],
            evt(TIMELOCK).to_vec(),
        ]
        .concat(),
        data: data("queue", &kind),
    };
    e.must_fail(&[opaque], &[&proposer], "Undisclosed");
    let queue_ix = Instruction {
        program_id: TIMELOCK,
        accounts: [
            vec![
                rw(timelock),
                sigw(proposer.pubkey()),
                rw(op(0)),
                ro(authority),
                ro(SWAP_AMM),
                ro(SYSTEM),
            ],
            evt(TIMELOCK).to_vec(),
        ]
        .concat(),
        data: data("queue_invoke", &(invoke_metas(&metas), inner_data.clone())),
    };
    let r = e.send(&[queue_ix], &[&proposer]);
    assert!(
        r.is_ok(),
        "queueing a timelocked invoke must be accepted: {r:?}"
    );
    let o: Operation = e.acct("Operation", &op(0));
    assert_eq!(o.kind, kind, "the program hashes what was disclosed");
    e.warp(432_000, 432_000 / 2);
    let exec_ix = Instruction {
        program_id: TIMELOCK,
        accounts: [
            vec![
                ro(timelock),
                sig(executor.pubkey()),
                rw(proposer.pubkey()),
                rw(op(0)),
                ro(authority),
                ro(SWAP_AMM),
            ],
            evt(TIMELOCK).to_vec(),
            vec![rw(e.global), sigw(proposer.pubkey()), ro(authority)],
        ]
        .concat(),
        data: data("execute_invoke", &inner_data),
    };
    let r = e.send(&[exec_ix], &[&executor, &proposer]);
    assert!(
        r.is_ok(),
        "executing the handover through the timelock must succeed: {r:?}"
    );
    let g: Global = e.acct("Global", &e.global);
    assert_eq!(
        g.authority, authority,
        "the PDA now holds protocol authority"
    );
    // Step 2: set_mode(Halted) through the timelock, signed by the PDA alone.
    let inner_data = data("admin_set_mode", &OperatingMode::Halted);
    // `admin_set_mode` emits through event CPI, so its account list carries the event authority and the program.
    let metas = vec![
        (e.global, false, true),
        (authority, true, false),
        (event_authority(&SWAP_AMM), false, false),
        (SWAP_AMM, false, false),
    ];
    let queue_ix = Instruction {
        program_id: TIMELOCK,
        accounts: [
            vec![
                rw(timelock),
                sigw(proposer.pubkey()),
                rw(op(1)),
                ro(authority),
                ro(SWAP_AMM),
                ro(SYSTEM),
            ],
            evt(TIMELOCK).to_vec(),
        ]
        .concat(),
        data: data("queue_invoke", &(invoke_metas(&metas), inner_data.clone())),
    };
    e.must(&[queue_ix], &[&proposer]);
    e.warp(432_000, 432_000 / 2);
    let exec_ix = Instruction {
        program_id: TIMELOCK,
        accounts: [
            vec![
                ro(timelock),
                sig(executor.pubkey()),
                rw(proposer.pubkey()),
                rw(op(1)),
                ro(authority),
                ro(SWAP_AMM),
            ],
            evt(TIMELOCK).to_vec(),
            vec![rw(e.global), ro(authority)],
            evt(SWAP_AMM).to_vec(),
        ]
        .concat(),
        data: data("execute_invoke", &inner_data),
    };
    e.must(&[exec_ix], &[&executor]);
    let g: Global = e.acct("Global", &e.global);
    assert_eq!(
        g.mode,
        OperatingMode::Halted,
        "the timelock halted the protocol"
    );
    // The old authority key has no power left.
    assert!(e.set_mode(&proposer, OperatingMode::Normal).is_err());
}

/// F-14. Invariant: `initialise` of each program is only accepted from the program's upgrade authority, so the
/// first transaction after a deployment cannot be front-run to seize `authority`, `proposer` or `treasury`. The
/// three programs are loaded with an upgrade authority set to `e.authority`; a stranger's initialise must fail
/// and the authority's must succeed.
#[test]
fn finding_f14_initialise_is_restricted_to_the_upgrade_authority() {
    let mut e = boot();
    let deployer = e.authority.insecure_clone();
    for program in [INDEX, TIMELOCK, SWAP_AMM] {
        set_upgrade_authority(&mut e, program, Some(deployer.pubkey()));
    }
    let stranger = Keypair::new();
    e.svm.airdrop(&stranger.pubkey(), 10_000_000_000).unwrap();
    // Index registry.
    let init_registry = |who: &Keypair, e: &Env| Instruction {
        program_id: INDEX,
        accounts: vec![
            rw(e.registry),
            sig(who.pubkey()),
            sigw(who.pubkey()),
            ro(program_data(&INDEX)),
            ro(SYSTEM),
        ],
        data: data(
            "initialise",
            &InitialiseArgs::single(e.guardian.pubkey(), who.pubkey()),
        ),
    };
    let r = e.send(&[init_registry(&stranger, &e)], &[&stranger]);
    assert!(r.is_err(), "a stranger must not initialise the registry");
    let r = e.send(&[init_registry(&deployer, &e)], &[&deployer]);
    assert!(
        r.is_ok(),
        "the upgrade authority initialises the registry: {r:?}"
    );
    // Timelock.
    let timelock = pda(&[b"timelock"], &TIMELOCK);
    let authority = pda(&[b"authority"], &TIMELOCK);
    let init_timelock = |who: &Keypair| Instruction {
        program_id: TIMELOCK,
        accounts: vec![
            rw(timelock),
            ro(authority),
            sigw(who.pubkey()),
            ro(program_data(&TIMELOCK)),
            ro(SYSTEM),
        ],
        data: data(
            "initialise",
            &(who.pubkey(), who.pubkey(), Pubkey::new_unique(), 432_000u64),
        ),
    };
    let r = e.send(&[init_timelock(&stranger)], &[&stranger]);
    assert!(r.is_err(), "a stranger must not initialise the timelock");
    let r = e.send(&[init_timelock(&deployer)], &[&deployer]);
    assert!(
        r.is_ok(),
        "the upgrade authority initialises the timelock: {r:?}"
    );
}

/// Overwrites the loader's ProgramData account for `program` with the given upgrade authority (LiteSVM loads
/// programs as immutable, `upgrade_authority_address: None`).
fn set_upgrade_authority(e: &mut Env, program: Pubkey, authority: Option<Pubkey>) {
    let program_data = program_data(&program);
    let mut acct = e
        .svm
        .get_account(&program_data)
        .expect("programdata exists");
    // UpgradeableLoaderState::ProgramData { slot: u64, upgrade_authority_address: Option<Pubkey> } is bincode:
    // 4-byte enum tag (3), 8-byte slot, 1-byte option tag, 32-byte key.
    acct.data[12] = u8::from(authority.is_some());
    if let Some(a) = authority {
        acct.data[13..45].copy_from_slice(a.as_ref());
    }
    e.svm.set_account(program_data, acct).unwrap();
}

/// F-15 / ADR-008 (withdraw side). Invariant: an LP cannot exit at `tvl / supply` while the open book is a
/// liability to the pool; the withdrawal price deducts the clamped SOAP liability. Here a pay-fixed trader is
/// deep in the money (floating 900 bp against a fixed rate near 727 bp for 30 days) and the LP's withdrawal of a
/// tenth of the shares must return less than par less the exit fee.
#[test]
fn finding_f15_lp_cannot_exit_at_par_while_the_book_is_a_liability() {
    let mut e = setup();
    let lp = e.lp.insecure_clone();
    let tr = e.trader.insecure_clone();
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
    // A tenth of the shares would take the pay leg past the 42 % per-leg immediate limit (external scan 2,
    // finding 11) and is pointed at the queue; a twenty-fifth goes through at once.
    let tenth = e.token_amount(&ata(&lp.pubkey(), &e.share_mint)) / 10;
    let ix = e.withdraw_ix(&lp.pubkey(), tenth, 0);
    e.must_fail(&[ix], &[&lp], "UseWithdrawQueue");
    let shares = e.token_amount(&ata(&lp.pubkey(), &e.share_mint)) / 25;
    let supply = e.mint_supply(&e.share_mint);
    let par = u128::from(shares) * (u128::from(p.tvl) + 1) / (u128::from(supply) + 1_000_000);
    let before = e.token_amount(&ata(&lp.pubkey(), &USDC_DEVNET));
    let ix = e.withdraw_ix(&lp.pubkey(), shares, 0);
    e.must(&[ix], &[&lp]);
    let got = u128::from(e.token_amount(&ata(&lp.pubkey(), &USDC_DEVNET)) - before);
    // The exit fee scales with utilisation (M-16): the bound is par less the scaled fee.
    let util_bp = (u128::from(p.open_pay_notional + p.open_rec_notional) * 10_000)
        .div_ceil(u128::from(p.tvl));
    let fee = par * 50 * util_bp / 100_000_000;
    assert!(
        got < par - fee,
        "withdrawal {got} must be below par less fee {} while the book is a liability",
        par - fee
    );
}

/// M-12 / ADR-008 (deposit side). Invariant: a depositor arriving just before a known trader loss settles does
/// not capture that loss at the expense of earlier LPs; the deposit price credits asset-side SOAP (capped at
/// `collateral_held`). Here a receive-fixed trader is deep out of the money, a late LP deposits one tenth of the
/// pool one slot before settlement and must not be able to withdraw more than deposited plus dust after it.
#[test]
fn finding_m12_late_depositor_cannot_capture_a_determined_settlement() {
    let mut e = setup();
    let lp = e.lp.insecure_clone();
    let tr = e.trader.insecure_clone();
    let ix = e.deposit_ix(&lp.pubkey(), 1_000_000 * USDC, 0);
    e.must(&[ix], &[&lp]);
    let ix = e.open_ix(
        &tr.pubkey(),
        &open_args(LegKind::ReceiveFixed, 0, 400_000 * USDC, 0, 1),
    );
    e.must(&[ix], &[&tr]);
    e.warp(216_000, DAY);
    e.publish(984).unwrap();
    e.warp(27 * 216_000, 27 * DAY);
    e.publish(984).unwrap();
    let late = e.new_actor(100_000 * USDC, true);
    // With the swap matured and unsettled, LP pricing is suspended outright (external scan 1, M-5): the late
    // depositor cannot enter at a stale price at all. Once anyone settles, deposits resume at the marked price.
    let ix = e.deposit_ix(&late.pubkey(), 100_000 * USDC, 0);
    e.must_fail(&[ix], &[&late], "MaturedUnsettled");
    e.warp(10, 10);
    let ix = e.close_ix("crank_settle_swap", &tr.pubkey(), 1, &e.payer.pubkey(), &());
    e.must(&[ix], &[]);
    let ix = e.deposit_ix(&late.pubkey(), 100_000 * USDC, 0);
    e.must(&[ix], &[&late]);
    let shares = e.token_amount(&ata(&late.pubkey(), &e.share_mint));
    let before = e.token_amount(&ata(&late.pubkey(), &USDC_DEVNET));
    let ix = e.withdraw_ix(&late.pubkey(), shares, 0);
    e.must(&[ix], &[&late]);
    let got = e.token_amount(&ata(&late.pubkey(), &USDC_DEVNET)) - before;
    assert!(
        got <= 100_000 * USDC + 1,
        "a late depositor must not capture the trader's loss: deposited 100,000 USDC, withdrew {got}"
    );
}

// F-16 / ADR-008 item 4 (first-loss reserve) was struck by the ADR-008 amendment of round 02: trader gains
// debit LP capital at book value and the share price marks the open book instead (`adr008_*` in e2e.rs). The
// share-inflation half of F-16 is green in `f16_*` (e2e.rs). No test is kept for the retired design.

/// F-18. Invariant: when the timelock is its own upgrade authority, a queued `Upgrade` of `brink_timelock` can
/// be executed through `execute_upgrade` (the executing program's accounts are writable in the loader CPI). The
/// test gives the timelock's ProgramData the `["authority"]` PDA as upgrade authority, writes a loader buffer
/// holding the same ELF, queues the upgrade with the buffer hash and executes it after the delay.
#[test]
#[ignore = "F-18: LiteSVM returns UnsupportedProgramId when the loader upgrades the program that is executing; self-upgrade is rehearsed on devnet (E2 0005 plan)"]
fn finding_f18_timelock_can_upgrade_itself_through_execute_upgrade() {
    let mut e = setup();
    let proposer = e.authority.insecure_clone();
    let executor = e.lp.insecure_clone();
    let guardian = e.guardian.pubkey();
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
                &(proposer.pubkey(), executor.pubkey(), guardian, 432_000u64),
            ),
        }],
        &[],
    );
    set_upgrade_authority(&mut e, TIMELOCK, Some(authority));
    // Loader buffer: 37-byte header (tag 1, Option<authority>) then the ELF.
    let elf = std::fs::read(deploy_dir().join("brink_timelock.so")).unwrap();
    let mut buf = vec![0u8; 37 + elf.len()];
    buf[0] = 1;
    buf[4] = 1;
    buf[5..37].copy_from_slice(authority.as_ref());
    buf[37..].copy_from_slice(&elf);
    let buffer = Pubkey::new_unique();
    let lamports = e.svm.minimum_balance_for_rent_exemption(buf.len());
    e.svm
        .set_account(
            buffer,
            Account {
                lamports,
                data: buf,
                owner: LOADER,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();
    let op = pda(&[b"op", &0u64.to_le_bytes()], &TIMELOCK);
    e.must(
        &[Instruction {
            program_id: TIMELOCK,
            accounts: [
                vec![
                    rw(timelock),
                    sigw(proposer.pubkey()),
                    rw(op),
                    ro(authority),
                    ro(TIMELOCK),
                    ro(buffer),
                    ro(SYSTEM),
                ],
                evt(TIMELOCK).to_vec(),
            ]
            .concat(),
            data: data("queue_upgrade", &()),
        }],
        &[&proposer],
    );
    let o: Operation = e.acct("Operation", &op);
    assert_eq!(
        o.kind,
        OperationKind::Upgrade {
            program: TIMELOCK,
            buffer,
            buffer_hash: sha256(&elf),
        }
    );
    e.warp(432_000, 432_000 / 2);
    let rent = Pubkey::from_str_const("SysvarRent111111111111111111111111111111111");
    let clock = Pubkey::from_str_const("SysvarC1ock11111111111111111111111111111111");
    let exec = Instruction {
        program_id: TIMELOCK,
        accounts: [
            vec![
                ro(timelock),
                sig(executor.pubkey()),
                rw(proposer.pubkey()),
                rw(op),
                ro(authority),
                rw(program_data(&TIMELOCK)),
                rw(TIMELOCK),
                rw(buffer),
                rw(payer),
                ro(rent),
                ro(clock),
                ro(LOADER),
            ],
            evt(TIMELOCK).to_vec(),
        ]
        .concat(),
        data: data("execute_upgrade", &()),
    };
    let r = e.send(&[exec], &[&executor]);
    assert!(
        r.is_ok(),
        "the timelock must be able to upgrade itself: {r:?}"
    );
    let o: Operation = e.acct("Operation", &op);
    assert_eq!(o.state, OperationState::Executed);
}

/// External scan 2, finding 18. Invariant: an upgrade is queued with its buffer disclosed and frozen: the buffer
/// must already belong to the timelock's authority PDA, so nobody can rewrite it after review, and its hash is
/// taken by the program rather than supplied. The generic `queue` refuses an opaque `Upgrade`. A cancelled upgrade
/// closes the buffer back to the proposer that paid for it.
#[test]
fn scan2_f18_upgrades_are_queued_disclosed_and_cancel_returns_the_buffer() {
    let mut e = setup();
    let proposer = e.authority.insecure_clone();
    let executor = e.lp.insecure_clone();
    let guardian = e.guardian.insecure_clone();
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
                    guardian.pubkey(),
                    432_000u64,
                ),
            ),
        }],
        &[],
    );
    let elf = vec![7u8; 4_096];
    let mut buffer_with = |owner: &Pubkey| -> (Pubkey, u64) {
        let mut buf = vec![0u8; 37 + elf.len()];
        buf[0] = 1;
        buf[4] = 1;
        buf[5..37].copy_from_slice(owner.as_ref());
        buf[37..].copy_from_slice(&elf);
        let buffer = Pubkey::new_unique();
        let lamports = e.svm.minimum_balance_for_rent_exemption(buf.len());
        e.svm
            .set_account(
                buffer,
                Account {
                    lamports,
                    data: buf,
                    owner: LOADER,
                    executable: false,
                    rent_epoch: 0,
                },
            )
            .unwrap();
        (buffer, lamports)
    };
    let (held_by_proposer, _) = buffer_with(&proposer.pubkey());
    let (held_by_pda, buffer_lamports) = buffer_with(&authority);
    let op = |n: u64| pda(&[b"op", &n.to_le_bytes()], &TIMELOCK);
    let queue_upgrade = |op: Pubkey, buffer: Pubkey| Instruction {
        program_id: TIMELOCK,
        accounts: [
            vec![
                rw(timelock),
                sigw(proposer.pubkey()),
                rw(op),
                ro(authority),
                ro(SWAP_AMM),
                ro(buffer),
                ro(SYSTEM),
            ],
            evt(TIMELOCK).to_vec(),
        ]
        .concat(),
        data: data("queue_upgrade", &()),
    };
    // Opaque kinds are refused by the generic queue, whoever computed the hash.
    let opaque = Instruction {
        program_id: TIMELOCK,
        accounts: [
            vec![rw(timelock), sigw(proposer.pubkey()), rw(op(0)), ro(SYSTEM)],
            evt(TIMELOCK).to_vec(),
        ]
        .concat(),
        data: data(
            "queue",
            &OperationKind::Upgrade {
                program: SWAP_AMM,
                buffer: held_by_pda,
                buffer_hash: sha256(&elf),
            },
        ),
    };
    e.must_fail(&[opaque], &[&proposer], "Undisclosed");
    // A buffer the proposer could still rewrite is not accepted.
    e.must_fail(
        &[queue_upgrade(op(0), held_by_proposer)],
        &[&proposer],
        "BufferAuthority",
    );
    // A buffer owned by the PDA is: the program records the hash it computed.
    e.must(&[queue_upgrade(op(0), held_by_pda)], &[&proposer]);
    let o: Operation = e.acct("Operation", &op(0));
    assert_eq!(
        o.kind,
        OperationKind::Upgrade {
            program: SWAP_AMM,
            buffer: held_by_pda,
            buffer_hash: sha256(&elf),
        }
    );
    // The guardian cancels; the buffer's lamports return to the proposer and both accounts are gone.
    let before = e.svm.get_balance(&proposer.pubkey()).unwrap();
    let cancel = Instruction {
        program_id: TIMELOCK,
        accounts: [
            vec![
                ro(timelock),
                sig(guardian.pubkey()),
                rw(proposer.pubkey()),
                rw(op(0)),
                ro(authority),
                rw(held_by_pda),
                ro(LOADER),
            ],
            evt(TIMELOCK).to_vec(),
        ]
        .concat(),
        data: data("cancel_upgrade", &()),
    };
    e.must(&[cancel], &[&guardian]);
    let after = e.svm.get_balance(&proposer.pubkey()).unwrap();
    assert!(
        after >= before + buffer_lamports,
        "buffer rent came back: {before} -> {after} (+{buffer_lamports})"
    );
    assert!(e
        .svm
        .get_account(&held_by_pda)
        .is_none_or(|a| a.data.is_empty() && a.lamports == 0));
    assert!(e.svm.get_account(&op(0)).is_none_or(|a| a.data.is_empty()));
}

/// F-31 / ADR-009. Invariant: a withdrawal that would push utilisation above the immediate limit is not
/// rejected with `PoolInvariant` and not served first come, first served: `lp_withdraw` answers
/// `UseWithdrawQueue`, and `lp_enqueue_withdraw` accepts the shares into the pool's queue (escrowed), to be
/// filled pro rata by the epoch crank.
#[test]
fn finding_f31_cap_breaching_withdrawal_is_queued_not_rejected() {
    let mut e = setup();
    let lp = e.lp.insecure_clone();
    let tr = e.trader.insecure_clone();
    let ix = e.deposit_ix(&lp.pubkey(), 1_000_000 * USDC, 0);
    e.must(&[ix], &[&lp]);
    let ix = e.open_ix(
        &tr.pubkey(),
        &open_args(LegKind::PayFixed, 0, 480_000 * USDC, 9_999, 1),
    );
    e.must(&[ix], &[&tr]);
    let shares = e.token_amount(&ata(&lp.pubkey(), &e.share_mint));
    let ix = e.withdraw_ix(&lp.pubkey(), shares / 2, 0);
    let err = e
        .send(&[ix], &[&lp])
        .expect_err("half the pool cannot leave at once with 48 percent utilised");
    assert!(
        err.contains("UseWithdrawQueue"),
        "an over-limit withdrawal must be redirected to the queue, not fail with PoolInvariant:\n{err}"
    );
    e.must(&[e.init_queue_ix()], &[]);
    let ix = e.enqueue_ix(&lp.pubkey(), shares / 2, 1);
    let r = e.send(&[ix], &[&lp]);
    assert!(r.is_ok(), "the queue must accept the request: {r:?}");
    let q: WithdrawQueue = e.acct("WithdrawQueue", &e.queue_pda());
    assert_eq!(q.queued_shares, shares / 2);
    assert_eq!(
        e.token_amount(&e.escrow_pda()),
        shares / 2,
        "shares are escrowed, not burnt"
    );
    assert_eq!(
        e.token_amount(&ata(&lp.pubkey(), &e.share_mint)),
        shares - shares / 2
    );
}

/// F-32 / ADR-010. Invariant: with a quorum of publishers the effective value is the median of fresh
/// submissions that agree, so a single band-edge submission leaves `value_bp` unchanged until a second
/// publisher corroborates it. The fixture registry is the devnet single-publisher shape, so the test first
/// moves governance to three publishers with a quorum of two.
#[test]
fn finding_f32_single_publisher_cannot_move_the_value() {
    let mut e = setup();
    let a = e.authority.insecure_clone();
    let p2 = Keypair::new();
    let p3 = Keypair::new();
    for k in [&p2, &p3] {
        e.svm.airdrop(&k.pubkey(), 1_000_000_000).unwrap();
    }
    let mut publishers = [Pubkey::default(); MAX_PUBLISHERS];
    publishers[0] = e.publisher.pubkey();
    publishers[1] = p2.pubkey();
    publishers[2] = p3.pubkey();
    let ix = Instruction {
        program_id: INDEX,
        accounts: vec![rw(e.registry), sig(a.pubkey())],
        data: data("set_publishers", &(publishers, 2u8)),
    };
    e.must(&[ix], &[&a]);
    let b0: Benchmark = e.acct("Benchmark", &e.benchmark);
    e.warp(216_000, DAY);
    // One publisher at the band edge: recorded, not applied.
    e.publish(b0.ema_bp + b0.band_bp).unwrap();
    let b1: Benchmark = e.acct("Benchmark", &e.benchmark);
    assert_eq!(
        b1.value_bp, b0.value_bp,
        "a lone publisher's submission must not change the effective value without a quorum"
    );
    assert_eq!(b1.publish_count, b0.publish_count);
    // A second publisher far from the first: with two fresh values the median is their midpoint and support
    // is measured from it, so the pair "agree" whenever they sit within two bands of each other (round 02
    // observation F-42). Seven hundred basis points apart they do not: the benchmark is flagged disputed and
    // the value does not move.
    let p1 = std::mem::replace(&mut e.publisher, p2.insecure_clone());
    e.warp(10, 10);
    e.publish(b0.ema_bp - 400).unwrap();
    let b2: Benchmark = e.acct("Benchmark", &e.benchmark);
    assert_eq!(
        b2.value_bp, b0.value_bp,
        "disagreeing pair does not move the value"
    );
    assert!(b2.disputed, "disagreement is flagged");
    // A third publisher one basis point above the EMA: the median of three is its value, two of the three
    // support it, and the outlier below is ignored. The value moves to the supported median.
    e.publisher = p3.insecure_clone();
    e.warp(10, 10);
    e.publish(b0.ema_bp + 1).unwrap();
    let b3: Benchmark = e.acct("Benchmark", &e.benchmark);
    assert_eq!(
        b3.value_bp,
        b0.ema_bp + 1,
        "the supported median is applied"
    );
    assert!(!b3.disputed, "a supported median clears the dispute flag");
    e.publisher = p1;
}

/// S-6 / F-32 (band walk). Invariant: the band bounds each step and a separate drift bound limits how far the
/// effective value can move from its anchor inside one drift window (`max_drift_bp` over `drift_window_slots`,
/// 400 bp over one day by default), so a publisher sitting at the band edge once an hour cannot walk the index
/// six-fold in a day as the round 01 binaries allowed.
#[test]
fn finding_s6_band_bounds_the_daily_drift_not_only_each_step() {
    let mut e = setup();
    let b0: Benchmark = e.acct("Benchmark", &e.benchmark);
    assert!(b0.max_drift_bp >= b0.band_bp);
    // Twenty-three hourly publishes keep the test inside one drift window (23 x 9,000 < 216,000 slots).
    for _ in 0..23 {
        let b: Benchmark = e.acct("Benchmark", &e.benchmark);
        e.warp(9_000, 3_600);
        e.publish((b.ema_bp + b.band_bp).min(30_000)).unwrap();
    }
    let b1: Benchmark = e.acct("Benchmark", &e.benchmark);
    assert!(
        b1.value_bp <= b0.value_bp + b0.max_drift_bp,
        "value moved from {} to {} bp inside one window with a {} bp drift bound",
        b0.value_bp,
        b1.value_bp,
        b0.max_drift_bp
    );
    assert!(
        b1.value_bp > b0.value_bp + b0.band_bp,
        "the walk does make progress up to the drift bound, so the bound is what held it"
    );
    assert!(
        b1.clamped,
        "the last band-edge publish was clamped by the drift bound and flagged"
    );
}

/// M-3. Invariant: the demand spread cannot be avoided by splitting a trade. (a) A trade that increases the
/// imbalance pays demand even when its notional is below `tvl / 10^4` (ceil, not floor); (b) a trade split into
/// twenty pieces pays in total at least what the single trade pays, to within one basis point of notional
/// (trapezoid rule on the extending part of the fill).
#[test]
fn finding_m3_demand_spread_cannot_be_avoided_by_splitting() {
    // (a) sub-unit notional on a large imbalance.
    let mut e = setup();
    let lp = e.lp.insecure_clone();
    let tr = e.trader.insecure_clone();
    // 16,000,000 USDC: the largest pool the 12-decimal share supply admits (test finding T-1).
    e.deposit(&lp, 16_000_000 * USDC);
    let big = e.new_actor(2_000_000 * USDC, false);
    let ix = e.open_ix(
        &big.pubkey(),
        &open_args(LegKind::PayFixed, 0, 6_400_000 * USDC, 9_999, 1),
    );
    e.must(&[ix], &[&big]);
    let ix = e.open_ix(
        &tr.pubkey(),
        &open_args(LegKind::PayFixed, 0, 1_000 * USDC, 9_999, 1),
    );
    e.must(&[ix], &[&tr]);
    let s: Swap = e.acct("Swap", &e.swap_pda(&tr.pubkey(), 1));
    let demand = i32::from(s.fixed_bp) - 684 - 11 - 3;
    assert!(
        demand >= 18,
        "a 1,000 USDC pay trade on a 4,000 bp pay imbalance must pay about 18 bp of demand, paid {demand}"
    );

    // (b) one 1,000,000 USDC 90-day trade against twenty 50,000 USDC trades on an identical pool.
    let mut e1 = setup();
    let lp = e1.lp.insecure_clone();
    let tr = e1.trader.insecure_clone();
    e1.deposit(&lp, 10_000_000 * USDC);
    let ix = e1.open_ix(
        &tr.pubkey(),
        &open_args(LegKind::PayFixed, 2, 1_000_000 * USDC, 9_999, 1),
    );
    e1.must(&[ix], &[&tr]);
    let s: Swap = e1.acct("Swap", &e1.swap_pda(&tr.pubkey(), 1));
    let single_cost = u128::from(s.fixed_bp - 684 - 31 - 7) * u128::from(s.notional);

    let mut e2 = setup();
    let lp = e2.lp.insecure_clone();
    let tr = e2.trader.insecure_clone();
    e2.deposit(&lp, 10_000_000 * USDC);
    let mut split_cost = 0u128;
    for i in 0..20u64 {
        let ix = e2.open_ix(
            &tr.pubkey(),
            &open_args(LegKind::PayFixed, 2, 50_000 * USDC, 9_999, i),
        );
        e2.must(&[ix], &[&tr]);
        let s: Swap = e2.acct("Swap", &e2.swap_pda(&tr.pubkey(), i));
        split_cost += u128::from(s.fixed_bp - 684 - 31 - 7) * u128::from(s.notional);
    }
    assert!(
        split_cost + u128::from(1_000_000 * USDC) >= single_cost,
        "twenty pieces paid {split_cost} bp-units of demand against {single_cost} for the single trade"
    );
}

/// M-4 / S-1. Invariant: close paths (`settle`, `cancel`, `liquidate`) are never blocked by the utilisation
/// caps or by `tvl == 0`; caps apply on entry and withdrawal only. Green against the applied tree (caps on
/// entry only), kept here as the regression for the finding. Pool 1,000,000 USDC with the pay leg at its
/// 48 percent cap; a receive-fixed trader wins their collateral at maturity, which pushes pay utilisation above
/// the cap; settlement must still succeed.
#[test]
fn finding_m4_close_paths_are_never_blocked_by_caps() {
    let mut e = setup();
    let lp = e.lp.insecure_clone();
    let tr = e.trader.insecure_clone();
    let ix = e.deposit_ix(&lp.pubkey(), 1_000_000 * USDC, 0);
    e.must(&[ix], &[&lp]);
    let ix = e.open_ix(
        &tr.pubkey(),
        &open_args(LegKind::PayFixed, 3, 480_000 * USDC, 9_999, 1),
    );
    e.must(&[ix], &[&tr]);
    let ix = e.open_ix(
        &tr.pubkey(),
        &open_args(LegKind::ReceiveFixed, 0, 100_000 * USDC, 0, 2),
    );
    e.must(&[ix], &[&tr]);
    e.warp(216_000, DAY);
    e.publish(384).unwrap();
    e.warp(27 * 216_000, 27 * DAY);
    e.publish(384).unwrap();
    let ix = e.close_ix("crank_settle_swap", &tr.pubkey(), 2, &e.payer.pubkey(), &());
    let r = e.send(&[ix], &[]);
    assert!(
        r.is_ok(),
        "settlement must not be blocked by the other leg's cap: {r:?}"
    );
    let p: Pool = e.acct("Pool", &e.pool);
    assert_eq!(p.open_swaps, 1);
    assert!(
        p.util_pay_bp > 4_800,
        "the pay leg now sits above its cap, which only blocks new entries"
    );
    let ix = e.open_ix(
        &tr.pubkey(),
        &open_args(LegKind::PayFixed, 0, 1_000 * USDC, 9_999, 3),
    );
    e.must_fail(&[ix], &[&tr], "LegCap");
}

/// M-6. Invariant: the EMA tracks a rising rate to within one basis point under the devnet guards (half-life
/// 10,000 slots, publishes every 150 slots). Today the floor discards every step below one bp, so from 600 bp
/// the EMA stalls 67 bp below a constant 684 bp value.
#[test]
fn finding_m6_ema_tracks_a_rising_rate_within_one_bp() {
    let mut e = setup();
    let bench = e.create_benchmark(*b"ema-stall\0\0\0\0\0\0\0", 600);
    let k = PoolKeys {
        benchmark: bench,
        pool: Pubkey::default(),
        share_mint: Pubkey::default(),
        vault: Pubkey::default(),
    };
    e.use_pool(&k);
    for _ in 0..600 {
        e.warp(150, 60);
        e.publish(684).unwrap();
    }
    let b: Benchmark = e.acct("Benchmark", &bench);
    assert!(
        b.ema_bp >= 683,
        "after 600 publishes at 684 bp the EMA is {} bp (stall of {} bp)",
        b.ema_bp,
        684 - b.ema_bp
    );
}

/// M-11. Invariant: the configured demand cap binds somewhere inside the utilisation caps, or the parameters
/// document the effective maximum. The largest imbalance-increasing trade from an empty pool (48 percent on one
/// leg) must pay `demand_cap_bp` (60 bp); with `k = 45` it pays 22 bp.
#[test]
#[ignore = "M-11: demand cap of 60 bp is unreachable inside the caps; effective maximum 22 bp is documented, recalibration is a governance decision"]
fn finding_m11_demand_cap_is_reachable_inside_the_utilisation_caps() {
    let mut e = setup();
    let lp = e.lp.insecure_clone();
    let tr = e.trader.insecure_clone();
    let ix = e.deposit_ix(&lp.pubkey(), 1_000_000 * USDC, 0);
    e.must(&[ix], &[&lp]);
    let ix = e.open_ix(
        &tr.pubkey(),
        &open_args(LegKind::PayFixed, 0, 480_000 * USDC, 9_999, 1),
    );
    e.must(&[ix], &[&tr]);
    let s: Swap = e.acct("Swap", &e.swap_pda(&tr.pubkey(), 1));
    let demand = u16::try_from(i32::from(s.fixed_bp) - 684 - 11 - 3).unwrap();
    let p: Pool = e.acct("Pool", &e.pool);
    assert_eq!(
        demand, p.params.demand_cap_bp,
        "the largest admissible trade must reach the demand cap"
    );
}

/// S-2. Invariant: Limited mode bounds a trader's exposure, not only each swap: with `limited_mode_cap` at
/// 100,000 USDC, three 100,000 USDC opens by the same trader in the same slot are not all accepted.
#[test]
fn finding_s2_limited_mode_cap_bounds_exposure_not_each_swap() {
    let mut e = setup();
    let lp = e.lp.insecure_clone();
    let tr = e.trader.insecure_clone();
    let a = e.authority.insecure_clone();
    let ix = e.deposit_ix(&lp.pubkey(), 2_000_000 * USDC, 0);
    e.must(&[ix], &[&lp]);
    e.set_mode(&a, OperatingMode::Limited).unwrap();
    let ix = e.open_ix(
        &tr.pubkey(),
        &open_args(LegKind::PayFixed, 0, 150_000 * USDC, 9_999, 0),
    );
    e.must_fail(&[ix], &[&tr], "LimitedModeCap");
    let mut accepted = 0;
    for i in 1..=3u64 {
        let ix = e.open_ix(
            &tr.pubkey(),
            &open_args(LegKind::PayFixed, 0, 100_000 * USDC, 9_999, i),
        );
        if e.send(&[ix], &[&tr]).is_ok() {
            accepted += 1;
        }
    }
    let p: Pool = e.acct("Pool", &e.pool);
    assert!(
        p.open_pay_notional <= 100_000 * USDC,
        "Limited mode let one trader open {} USDC of notional in one slot ({accepted} swaps)",
        p.open_pay_notional / USDC
    );
}

/// S-3 / F-12. Invariant: cancelling after maturity is either impossible or identical to settlement. Here the
/// pay-fixed swap is quoted above a flat 684 bp index; after maturity the trader cancels instead of settling:
/// the payout must equal the settlement payout (a small loss), and no bounty logic is skipped. With patch 0002
/// the single post-maturity publish is handled; this test holds the general invariant through `cancel`.
#[test]
fn finding_s3_cancel_after_maturity_equals_settlement() {
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
    // Two publishes after maturity, so the latest segment does not contain maturity.
    e.warp(30 * 216_000, 30 * DAY);
    e.publish(684).unwrap();
    e.warp(26 * 216_000, 26 * DAY);
    e.publish(684).unwrap();
    let before = e.token_amount(&ata(&tr.pubkey(), &USDC_DEVNET));
    let ix = e.close_ix("trader_cancel_swap", &tr.pubkey(), 1, &tr.pubkey(), &0u64);
    let r = e.send(&[ix], &[&tr]);
    let fair_loss =
        u64::try_from(u128::from(s.fixed_bp - 684) * u128::from(s.notional) * 28 / (10_000 * 365))
            .unwrap();
    match r {
        Err(err) => assert!(
            err.contains("AlreadySettled") || err.contains("NotMatured"),
            "cancel after maturity must be refused with a maturity error or pay the settlement value: {err}"
        ),
        Ok(_) => {
            let payout = e.token_amount(&ata(&tr.pubkey(), &USDC_DEVNET)) - before;
            assert_eq!(
                payout,
                s.collateral - fair_loss,
                "cancel after maturity must pay exactly the settlement value"
            );
        }
    }
}

/// Keeps the unused-import lints quiet when a test is filtered out.
#[allow(dead_code)]
fn _unused(_: AccountMeta) {}

/// Round-02 test finding T-1 (regression from maths patch 0005). Invariant: any deposit the pool accepts by its
/// own limits is accepted whole. With 12-decimal shares a deposit above `i64::MAX / 10^6` units (about 9.22
/// million USDC at par) mints more shares than the `LiquidityChanged.shares: i64` field can carry and the
/// instruction fails with `Overflow`; a pool can never hold more than `u64::MAX / 10^6` units (about 18.4
/// million USDC) of share supply at par. Either the event fields widen (u128 or u64), the share scale drops, or
/// the limit is documented and enforced with a named error.
#[test]
fn finding_t1_large_deposit_is_accepted_whole() {
    let mut e = setup();
    let lp = e.lp.insecure_clone();
    let ix = e.deposit_ix(&lp.pubkey(), 10_000_000 * USDC, 0);
    let r = e.send(&[ix], &[&lp]);
    assert!(
        r.is_ok(),
        "a 10,000,000 USDC deposit into an empty pool must be accepted: {r:?}"
    );
    let ix = e.deposit_ix(&lp.pubkey(), 10_000_000 * USDC, 0);
    let r = e.send(&[ix], &[&lp]);
    assert!(
        r.is_ok(),
        "a pool must be able to hold 20,000,000 USDC of LP capital: {r:?}"
    );
}

fn seeded() -> Env {
    let mut e = setup();
    let lp = e.lp.insecure_clone();
    e.deposit(&lp, 10_000_000 * USDC);
    e
}

fn open_default(e: &mut Env, seed: u64) {
    let tr = e.trader.insecure_clone();
    let ix = e.open_ix(
        &tr.pubkey(),
        &open_args(LegKind::PayFixed, 0, 100_000 * USDC, 9_999, seed),
    );
    e.must(&[ix], &[&tr]);
}

fn assert_no_panic(err: &str) {
    assert!(
        !err.contains("panicked") && !err.contains("SBF program panicked"),
        "program must fail with an error, not a panic:\n{err}"
    );
}

/// External scan 1, M-18. Invariant: once a queued withdrawal epoch is eligible, a new swap cannot take the
/// capacity it needs; the crank processes the epoch first and the book reopens to new exposure afterwards.
#[test]
fn scan1_m18_eligible_withdrawal_epoch_has_priority_over_new_exposure() {
    let mut e = setup();
    let lp = e.lp.insecure_clone();
    let tr = e.trader.insecure_clone();
    let ix = e.deposit_ix(&lp.pubkey(), 1_000_000 * USDC, 0);
    e.must(&[ix], &[&lp]);
    let ix = e.open_ix(
        &tr.pubkey(),
        &open_args(LegKind::PayFixed, 0, 400_000 * USDC, 9_999, 1),
    );
    e.must(&[ix], &[&tr]);
    e.must(&[e.init_queue_ix()], &[]);
    let shares = e.token_amount(&ata(&lp.pubkey(), &e.share_mint));
    let ix = e.enqueue_ix(&lp.pubkey(), shares / 2, 1);
    e.must(&[ix], &[&lp]);
    // Before the epoch is eligible, new exposure is still accepted.
    let ix = e.open_ix(
        &tr.pubkey(),
        &open_args(LegKind::PayFixed, 0, 10_000 * USDC, 9_999, 2),
    );
    e.must(&[ix], &[&tr]);
    // Eligible epoch: half the pool is owed and the caps' floor leaves far less; the swap is refused.
    e.warp(216_000, DAY);
    e.publish(684).unwrap();
    let ix = e.open_ix(
        &tr.pubkey(),
        &open_args(LegKind::PayFixed, 0, 10_000 * USDC, 9_999, 3),
    );
    e.must_fail(&[ix], &[&tr], "QueueHasPriority");
    // The permissionless crank serves the epoch; what it could not serve stays queued for the next one, and the
    // pool's queued-share mirror agrees with the queue account (M-18 mirror check).
    e.must(&[e.process_ix()], &[]);
    let q: WithdrawQueue = e.acct("WithdrawQueue", &e.queue_pda());
    let p: Pool = e.acct("Pool", &e.pool);
    assert_eq!(p.queued_shares, q.queued_shares, "pool mirrors the queue");
    assert_eq!(
        p.share_supply,
        e.mint_supply(&e.share_mint),
        "pool mirrors the mint"
    );
}

/// External scan 1, M-8. Invariant: a trader's cancel carries a minimum payout that the program enforces on the
/// final amount, so a mark that moves between signing and execution cannot silently pay less.
#[test]
fn scan1_m8_cancel_enforces_min_payout() {
    let mut e = seeded();
    let tr = e.trader.insecure_clone();
    open_default(&mut e, 1);
    let ix = e.close_ix(
        "trader_cancel_swap",
        &tr.pubkey(),
        1,
        &tr.pubkey(),
        &u64::MAX,
    );
    e.must_fail(&[ix], &[&tr], "Slippage");
    let ix = e.close_ix("trader_cancel_swap", &tr.pubkey(), 1, &tr.pubkey(), &0u64);
    e.must(&[ix], &[&tr]);
}

/// External scan 1, H-1 (claims no longer require the associated token account) keeps its ownership check: a
/// claim paid to a token account the requesting LP does not own is refused.
#[test]
fn scan1_h1_claim_pays_only_accounts_the_lp_owns() {
    let mut e = setup();
    let lp = e.lp.insecure_clone();
    let tr = e.trader.insecure_clone();
    let ix = e.deposit_ix(&lp.pubkey(), 1_000_000 * USDC, 0);
    e.must(&[ix], &[&lp]);
    let ix = e.open_ix(
        &tr.pubkey(),
        &open_args(LegKind::PayFixed, 0, 400_000 * USDC, 9_999, 1),
    );
    e.must(&[ix], &[&tr]);
    e.must(&[e.init_queue_ix()], &[]);
    let shares = e.token_amount(&ata(&lp.pubkey(), &e.share_mint));
    let ix = e.enqueue_ix(&lp.pubkey(), shares / 2, 1);
    e.must(&[ix], &[&lp]);
    e.warp(216_000, DAY);
    e.publish(684).unwrap();
    e.must(&[e.process_ix()], &[]);
    let stranger = e.new_actor(USDC, false);
    let ix = e.claim_ix(&lp.pubkey(), 1, &lp.pubkey());
    let ix = Env::substitute(
        &ix,
        ata(&lp.pubkey(), &USDC_DEVNET),
        ata(&stranger.pubkey(), &USDC_DEVNET),
    );
    let err = e
        .send(&[ix], &[&lp])
        .expect_err("a claim must not pay a token account the LP does not own");
    assert_no_panic(&err);
    let ix = e.claim_ix(&lp.pubkey(), 1, &lp.pubkey());
    e.must(&[ix], &[&lp]);
}

/// External scan 1, L-25. Invariant: governance cannot hand the pool to a configuration with no guardian.
#[test]
fn scan1_l25_authority_change_refuses_a_zero_guardian() {
    let mut e = seeded();
    let a = e.authority.insecure_clone();
    let incoming = Keypair::new();
    e.svm.airdrop(&incoming.pubkey(), 1_000_000_000).unwrap();
    let ix = Instruction {
        program_id: SWAP_AMM,
        accounts: [
            vec![rw(e.global), sig(a.pubkey()), sig(incoming.pubkey())],
            evt(SWAP_AMM).to_vec(),
        ]
        .concat(),
        data: data("admin_set_authority", &Pubkey::default()),
    };
    e.must_fail(&[ix], &[&a, &incoming], "GuardianScope");
}
