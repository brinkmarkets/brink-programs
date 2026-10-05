//! Adversarial LiteSVM scenarios (round 02, METHOD classes 1 to 9 and 13): re-entrancy through a hook program,
//! account substitution, duplicate mutable accounts, stale benchmark on every path, revived closed accounts,
//! arithmetic edges, PDA seed collisions, token owner and delegate checks, Token-2022 rejection. Every test here
//! is expected green against the applied tree; where a behaviour is a characterisation rather than a guarantee
//! the assertion says so. Run with `cargo test --test adversarial`.
use brink_svm_tests::harness::*;
use brink_svm_tests::*;
use solana_account::Account;
use solana_instruction::{AccountMeta, Instruction};
use solana_keypair::Keypair;
use solana_program_pack::Pack;
use solana_pubkey::Pubkey;
use solana_signer::Signer;

const TOKEN_2022: Pubkey = Pubkey::from_str_const("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");

/// Rebuilds an instruction produced by an interface crate as this crate's `Instruction` type, field by field.
macro_rules! conv {
    ($ix:expr) => {{
        let ix = $ix;
        Instruction {
            program_id: ix.program_id,
            accounts: ix
                .accounts
                .into_iter()
                .map(|m| AccountMeta {
                    pubkey: m.pubkey,
                    is_signer: m.is_signer,
                    is_writable: m.is_writable,
                })
                .collect(),
            data: ix.data,
        }
    }};
}

/// Writes a 6-decimal mint owned by `program` at a fresh address.
fn raw_mint(e: &mut Env, program: Pubkey, authority: Pubkey) -> Pubkey {
    let k = Pubkey::new_unique();
    let mut data = vec![0u8; spl_token_interface::state::Mint::LEN];
    spl_token_interface::state::Mint {
        mint_authority: Some(authority).into(),
        supply: 0,
        decimals: 6,
        is_initialized: true,
        freeze_authority: None.into(),
    }
    .pack_into_slice(&mut data);
    e.svm
        .set_account(
            k,
            Account {
                lamports: 10_000_000_000,
                data,
                owner: program,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();
    k
}

fn pool_snapshot(e: &Env) -> (u64, u64, u32, u64, u64) {
    let p: Pool = e.acct("Pool", &e.pool);
    (
        p.tvl,
        p.collateral_held,
        p.open_swaps,
        p.open_pay_notional,
        e.token_amount(&e.vault),
    )
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

// ---------------------------------------------------------------- 1. re-entrancy through a hook program

/// Class 1 (re-entrancy). The probe hook re-enters `sync_vault` at `before_open` with the pool passed
/// read-only (the only privilege a hook has), writes the read-only pool at `after_open`, and observes at the
/// deposit points. The open must fail and leave the pool untouched; a benign hook passes; the wrong hook
/// program and a missing hook account are `HookMismatch`.
#[test]
fn hook_reentrancy_and_readonly_writes_are_refused() {
    let mut e = seeded();
    if !e.load_hook_program() {
        eprintln!("hook probe binary absent; test skipped");
        return;
    }
    let lp = e.lp.insecure_clone();
    let tr = e.trader.insecure_clone();
    let default_pool = e.pool_keys();
    // Pool A: before_open re-enters sync_vault.
    let bench_a = e.create_benchmark(*b"hook-a\0\0\0\0\0\0\0\0\0\0", 684);
    let a = e
        .create_pool(
            bench_a,
            Some(HOOK_PROGRAM),
            HookFlags {
                before_open: true,
                ..HookFlags::default()
            },
        )
        .unwrap();
    e.use_pool(&a);
    let m = e.share_mint;
    e.create_ata(&lp.pubkey(), &m);
    let ix = e.deposit_ix(&lp.pubkey(), 1_000_000 * USDC, 0);
    e.must(&[ix], &[&lp]);
    let before = pool_snapshot(&e);
    let ix = e.open_ix_hook(
        &tr.pubkey(),
        &open_args(LegKind::PayFixed, 0, 10_000 * USDC, 9_999, 1),
        HOOK_PROGRAM,
    );
    let err = e
        .send(&[ix], &[&tr])
        .expect_err("re-entrant hook must fail the open");
    assert_no_panic(&err);
    assert_eq!(
        pool_snapshot(&e),
        before,
        "a failed open leaves the pool unchanged"
    );
    assert!(
        e.svm.get_account(&e.swap_pda(&tr.pubkey(), 1)).is_none(),
        "no swap account survives"
    );
    // Wrong hook program and missing hook account.
    let ix = e.open_ix_hook(
        &tr.pubkey(),
        &open_args(LegKind::PayFixed, 0, 10_000 * USDC, 9_999, 1),
        INDEX,
    );
    e.must_fail(&[ix], &[&tr], "HookMismatch");
    let ix = e.open_ix(
        &tr.pubkey(),
        &open_args(LegKind::PayFixed, 0, 10_000 * USDC, 9_999, 1),
    );
    e.must_fail(&[ix], &[&tr], "HookMismatch");
    // Pool B: after_open writes into the read-only pool account.
    let bench_b = e.create_benchmark(*b"hook-b\0\0\0\0\0\0\0\0\0\0", 684);
    let b = e
        .create_pool(
            bench_b,
            Some(HOOK_PROGRAM),
            HookFlags {
                after_open: true,
                ..HookFlags::default()
            },
        )
        .unwrap();
    e.use_pool(&b);
    let m = e.share_mint;
    e.create_ata(&lp.pubkey(), &m);
    let ix = e.deposit_ix(&lp.pubkey(), 1_000_000 * USDC, 0);
    e.must(&[ix], &[&lp]);
    let before = pool_snapshot(&e);
    let ix = e.open_ix_hook(
        &tr.pubkey(),
        &open_args(LegKind::PayFixed, 0, 10_000 * USDC, 9_999, 1),
        HOOK_PROGRAM,
    );
    let err = e
        .send(&[ix], &[&tr])
        .expect_err("a hook writing a read-only account must fail");
    assert!(
        err.contains("ReadonlyDataModified")
            || err.contains("read-only")
            || err.contains("readonly"),
        "runtime must report the read-only write:\n{err}"
    );
    assert_eq!(pool_snapshot(&e), before);
    // Pool C: benign deposit hooks (points 4 and 5 observe and return Ok).
    let bench_c = e.create_benchmark(*b"hook-c\0\0\0\0\0\0\0\0\0\0", 684);
    let c = e
        .create_pool(
            bench_c,
            Some(HOOK_PROGRAM),
            HookFlags {
                before_deposit: true,
                after_deposit: true,
                ..HookFlags::default()
            },
        )
        .unwrap();
    e.use_pool(&c);
    let m = e.share_mint;
    e.create_ata(&lp.pubkey(), &m);
    let ix = e.deposit_ix_hook(&lp.pubkey(), 1_000 * USDC, 0, HOOK_PROGRAM);
    let logs = e.must(&[ix], &[&lp]);
    assert!(
        logs.iter().any(|l| l.contains("hook: point 4")),
        "before_deposit ran"
    );
    assert!(
        logs.iter().any(|l| l.contains("hook: point 5")),
        "after_deposit ran"
    );
    // A hook that is not executable is refused.
    let ix = e.deposit_ix_hook(&lp.pubkey(), 1_000 * USDC, 0, e.registry);
    e.must_fail(&[ix], &[&lp], "HookMismatch");
    e.use_pool(&default_pool);
}

// ---------------------------------------------------------------- 2. account substitution

/// Class 2 (account substitution). With a second benchmark and pool live, every cross-pool substitution on
/// open, deposit, withdraw and close is refused and the target pool is unchanged: wrong pool, wrong benchmark,
/// wrong vault, wrong share mint, wrong settlement mint, another trader's token account.
#[test]
fn account_substitution_is_refused_on_every_path() {
    let mut e = seeded();
    let lp = e.lp.insecure_clone();
    let tr = e.trader.insecure_clone();
    open_default(&mut e, 1);
    let one = e.pool_keys();
    let bench2 = e.create_benchmark(*b"second-usdc\0\0\0\0\0", 500);
    let two = e.create_pool(bench2, None, HookFlags::default()).unwrap();
    e.use_pool(&two);
    let m2 = e.share_mint;
    e.create_ata(&lp.pubkey(), &m2);
    let ix = e.deposit_ix(&lp.pubkey(), 1_000_000 * USDC, 0);
    e.must(&[ix], &[&lp]);
    e.use_pool(&one);
    let snap1 = pool_snapshot(&e);
    let other_mint = {
        let ma = e.mint_authority.pubkey();
        raw_mint(&mut e, TOKEN_PROGRAM, ma)
    };
    let stranger = e.new_actor(1_000_000 * USDC, true);

    let args = open_args(LegKind::PayFixed, 0, 10_000 * USDC, 9_999, 7);
    let open = e.open_ix(&tr.pubkey(), &args);
    let cases: Vec<(&str, Instruction)> = vec![
        (
            "open: wrong benchmark",
            Env::substitute(&open, one.benchmark, two.benchmark),
        ),
        (
            "open: wrong vault",
            Env::substitute(&open, one.vault, two.vault),
        ),
        (
            "open: wrong pool",
            Env::substitute(&open, one.pool, two.pool),
        ),
        (
            "open: wrong settlement mint",
            Env::substitute(&open, USDC_DEVNET, other_mint),
        ),
        (
            "open: another trader's token account",
            Env::substitute(
                &open,
                ata(&tr.pubkey(), &USDC_DEVNET),
                ata(&stranger.pubkey(), &USDC_DEVNET),
            ),
        ),
    ];
    for (name, ix) in cases {
        let err = e
            .send(&[ix], &[&tr])
            .err()
            .unwrap_or_else(|| panic!("{name} must fail"));
        assert_no_panic(&err);
        assert!(
            err.contains("ConstraintHasOne")
                || err.contains("ConstraintSeeds")
                || err.contains("TokenOwner")
                || err.contains("SettlementMint")
                || err.contains("AccountNotInitialized")
                || err.contains("ConstraintTokenOwner"),
            "{name}: unexpected error\n{err}"
        );
        assert_eq!(pool_snapshot(&e), snap1, "{name}: pool one unchanged");
    }
    let deposit = e.deposit_ix(&lp.pubkey(), 1_000 * USDC, 0);
    let cases: Vec<(&str, Instruction)> = vec![
        (
            "deposit: wrong share mint",
            Env::substitute(&deposit, one.share_mint, two.share_mint),
        ),
        (
            "deposit: wrong vault",
            Env::substitute(&deposit, one.vault, two.vault),
        ),
        (
            "deposit: share account of pool two",
            Env::substitute(
                &deposit,
                ata(&lp.pubkey(), &one.share_mint),
                ata(&lp.pubkey(), &two.share_mint),
            ),
        ),
        (
            "deposit: wrong settlement mint",
            Env::substitute(&deposit, USDC_DEVNET, other_mint),
        ),
    ];
    for (name, ix) in cases {
        let err = e
            .send(&[ix], &[&lp])
            .err()
            .unwrap_or_else(|| panic!("{name} must fail"));
        assert_no_panic(&err);
        assert_eq!(pool_snapshot(&e), snap1, "{name}: pool one unchanged");
    }
    let withdraw = e.withdraw_ix(&lp.pubkey(), SHARE, 0);
    let cases: Vec<(&str, Instruction)> = vec![
        (
            "withdraw: wrong share mint",
            Env::substitute(&withdraw, one.share_mint, two.share_mint),
        ),
        (
            "withdraw: wrong vault",
            Env::substitute(&withdraw, one.vault, two.vault),
        ),
        (
            "withdraw: wrong pool",
            Env::substitute(&withdraw, one.pool, two.pool),
        ),
    ];
    for (name, ix) in cases {
        let err = e
            .send(&[ix], &[&lp])
            .err()
            .unwrap_or_else(|| panic!("{name} must fail"));
        assert_no_panic(&err);
        assert_eq!(pool_snapshot(&e), snap1, "{name}: pool one unchanged");
    }
    let close = e.close_ix("trader_cancel_swap", &tr.pubkey(), 1, &tr.pubkey(), &0u64);
    let cases: Vec<(&str, Instruction)> = vec![
        (
            "cancel: wrong pool",
            Env::substitute(&close, one.pool, two.pool),
        ),
        (
            "cancel: wrong benchmark",
            Env::substitute(&close, one.benchmark, two.benchmark),
        ),
        (
            "cancel: wrong vault",
            Env::substitute(&close, one.vault, two.vault),
        ),
        (
            "cancel: payout to a stranger's account",
            Env::substitute(
                &close,
                ata(&tr.pubkey(), &USDC_DEVNET),
                ata(&stranger.pubkey(), &USDC_DEVNET),
            ),
        ),
    ];
    for (name, ix) in cases {
        let err = e
            .send(&[ix], &[&tr])
            .err()
            .unwrap_or_else(|| panic!("{name} must fail"));
        assert_no_panic(&err);
        assert_eq!(pool_snapshot(&e), snap1, "{name}: pool one unchanged");
    }
    // A stranger cannot cancel the trader's swap by signing as themselves.
    let mut ix = close.clone();
    for m in ix.accounts.iter_mut() {
        if m.pubkey == tr.pubkey() && m.is_signer {
            m.pubkey = stranger.pubkey();
        }
    }
    let err = e
        .send(&[ix], &[&stranger])
        .expect_err("a stranger must not cancel");
    assert_no_panic(&err);
    // Benchmark substitution on sync_vault: vault of pool two against pool one.
    let sync = e.sync_ix();
    let ix = Env::substitute(&sync, one.vault, two.vault);
    e.must_fail(&[ix], &[], "ConstraintHasOne");
    assert_eq!(pool_snapshot(&e), snap1);
}

// ---------------------------------------------------------------- 3. duplicate mutable accounts

/// Class 3 (duplicate mutable accounts). Passing the same token account twice where two distinct ones are
/// expected (LP USDC as the share account, the vault as the payout account, the trader's account as the
/// vault) is refused and nothing moves.
#[test]
fn duplicate_mutable_accounts_are_refused() {
    let mut e = seeded();
    let lp = e.lp.insecure_clone();
    let tr = e.trader.insecure_clone();
    open_default(&mut e, 1);
    let snap = pool_snapshot(&e);
    let lp_usdc = ata(&lp.pubkey(), &USDC_DEVNET);
    let lp_shares = ata(&lp.pubkey(), &e.share_mint);
    let tr_usdc = ata(&tr.pubkey(), &USDC_DEVNET);
    let deposit = e.deposit_ix(&lp.pubkey(), 1_000 * USDC, 0);
    let withdraw = e.withdraw_ix(&lp.pubkey(), SHARE, 0);
    let cancel = e.close_ix("trader_cancel_swap", &tr.pubkey(), 1, &tr.pubkey(), &0u64);
    let open = e.open_ix(
        &tr.pubkey(),
        &open_args(LegKind::PayFixed, 0, 10_000 * USDC, 9_999, 2),
    );
    let cases: Vec<(&str, Instruction, &Keypair)> = vec![
        (
            "deposit: shares account = USDC account",
            Env::substitute(&deposit, lp_shares, lp_usdc),
            &lp,
        ),
        (
            "deposit: USDC account = vault",
            Env::substitute(&deposit, lp_usdc, e.vault),
            &lp,
        ),
        (
            "withdraw: shares account = USDC account",
            Env::substitute(&withdraw, lp_shares, lp_usdc),
            &lp,
        ),
        (
            "withdraw: USDC account = vault",
            Env::substitute(&withdraw, lp_usdc, e.vault),
            &lp,
        ),
        (
            "cancel: payout account = vault",
            Env::substitute(&cancel, tr_usdc, e.vault),
            &tr,
        ),
        (
            "cancel: vault = payout account",
            Env::substitute(&cancel, e.vault, tr_usdc),
            &tr,
        ),
        (
            "open: vault = trader account",
            Env::substitute(&open, e.vault, tr_usdc),
            &tr,
        ),
    ];
    for (name, ix, signer) in cases {
        let err = e
            .send(&[ix], &[signer])
            .err()
            .unwrap_or_else(|| panic!("{name} must fail"));
        assert_no_panic(&err);
        assert_eq!(pool_snapshot(&e), snap, "{name}: pool unchanged");
    }
    assert_eq!(e.token_amount(&lp_usdc), 10_000_000 * USDC);
}

// ---------------------------------------------------------------- 4. stale benchmark on every path

/// Class 4 (oracle staleness). Past `max_staleness_slots` without a publish: open, cancel and liquidate are
/// `BenchmarkStale`; settlement of a matured swap, deposit, withdraw, sync and sweep do not read the spot and
/// must keep working, so exits are never gated on a live publisher.
#[test]
fn stale_benchmark_blocks_marks_but_not_exits() {
    let mut e = seeded();
    let lp = e.lp.insecure_clone();
    let tr = e.trader.insecure_clone();
    open_default(&mut e, 1);
    let ix = e.open_ix(
        &tr.pubkey(),
        &open_args(LegKind::ReceiveFixed, 0, 100_000 * USDC, 0, 2),
    );
    e.must(&[ix], &[&tr]);
    // Degraded (past max_staleness, within twice it): opens are refused, exits still price (external scan 1,
    // M-14). The liquidation runs its mark and finds the position healthy rather than refusing to look.
    e.warp(2_001, 1_000);
    let ix = e.open_ix(
        &tr.pubkey(),
        &open_args(LegKind::PayFixed, 0, 10_000 * USDC, 9_999, 3),
    );
    e.must_fail(&[ix], &[&tr], "BenchmarkStale");
    let ix = e.close_ix(
        "crank_liquidate_swap",
        &tr.pubkey(),
        1,
        &e.payer.pubkey(),
        &(),
    );
    e.must_fail(&[ix], &[], "NotLiquidatable");
    // Stale (past twice max_staleness): every spot-reading path is refused, exits included.
    e.warp(2_001, 1_000);
    let ix = e.close_ix("trader_cancel_swap", &tr.pubkey(), 1, &tr.pubkey(), &0u64);
    e.must_fail(&[ix], &[&tr], "BenchmarkStale");
    let ix = e.close_ix(
        "crank_liquidate_swap",
        &tr.pubkey(),
        1,
        &e.payer.pubkey(),
        &(),
    );
    e.must_fail(&[ix], &[], "BenchmarkStale");
    let ix = e.close_ix("crank_settle_swap", &tr.pubkey(), 1, &e.payer.pubkey(), &());
    e.must_fail(&[ix], &[], "NotMatured");
    // Mature both, publish once after maturity, then go stale again: settlement does not read the spot. While
    // the matured swaps await settlement, LP pricing is suspended (external scan 1, M-5); it resumes once the
    // permissionless crank has settled them.
    e.warp(28 * 216_000, 28 * DAY);
    e.publish(684).unwrap();
    e.warp(2_001, 1_000);
    let ix = e.deposit_ix(&lp.pubkey(), 1_000 * USDC, 0);
    e.must_fail(&[ix], &[&lp], "MaturedUnsettled");
    let ix = e.withdraw_ix(&lp.pubkey(), 1_000 * USDC * SHARE, 0);
    e.must_fail(&[ix], &[&lp], "MaturedUnsettled");
    let v = e.vault;
    e.mint_usdc(&v, USDC);
    e.must(&[e.sync_ix()], &[]);
    let ix = e.sweep_ix();
    e.must(&[ix], &[]);
    let ix = e.close_ix("crank_settle_swap", &tr.pubkey(), 1, &e.payer.pubkey(), &());
    e.must(&[ix], &[]);
    let ix = e.deposit_ix(&lp.pubkey(), 1_000 * USDC, 0);
    e.must_fail(&[ix], &[&lp], "MaturedUnsettled");
    let ix = e.close_ix("crank_settle_swap", &tr.pubkey(), 2, &e.payer.pubkey(), &());
    e.must(&[ix], &[]);
    let p: Pool = e.acct("Pool", &e.pool);
    assert_eq!(p.open_swaps, 0);
    assert_eq!(p.collateral_held, 0);
    let ix = e.deposit_ix(&lp.pubkey(), 1_000 * USDC, 0);
    e.must(&[ix], &[&lp]);
    let ix = e.withdraw_ix(&lp.pubkey(), 1_000 * USDC * SHARE, 0);
    e.must(&[ix], &[&lp]);
}

/// Class 4, continued. The index refuses a publish whose clock went backwards, a value above the rate ceiling,
/// and a publish out of band.
#[test]
fn index_guards_hold_at_the_edges() {
    let mut e = setup();
    e.must_fail(
        &[Instruction {
            program_id: INDEX,
            accounts: [
                vec![ro(e.registry), sig(e.publisher.pubkey()), rw(e.benchmark)],
                evt(INDEX).to_vec(),
            ]
            .concat(),
            data: data("publish", &30_001u16),
        }],
        &[&e.publisher.insecure_clone()],
        "RateCeiling",
    );
    e.warp(10, 10);
    // Out of band: the value is clamped to the band edge and flagged, not refused (E2 0003, F-34). The band
    // widens with the slots elapsed since the last publish, by band x (hl + dt) / hl.
    let r = e.publish(684 + 900);
    assert!(
        r.is_ok(),
        "out-of-band publish is clamped, not refused: {r:?}"
    );
    let b: Benchmark = e.acct("Benchmark", &e.benchmark);
    assert!(b.clamped, "clamped flag set");
    assert_eq!(b.value_bp, 684 + 300, "value sits on the band edge");
    // A flagged value is never quoted on: opening a swap is refused while it stands.
    let tr = e.trader.insecure_clone();
    let lp = e.lp.insecure_clone();
    e.deposit(&lp, 1_000_000 * USDC);
    let ix = e.open_ix(
        &tr.pubkey(),
        &open_args(LegKind::PayFixed, 0, 10_000 * USDC, 9_999, 1),
    );
    e.must_fail(&[ix], &[&tr], "BenchmarkOutOfBand");
    // Clock backwards: move the timestamp back and publish.
    let mut c = e.clock();
    c.unix_timestamp -= 100;
    e.svm.set_sysvar(&c);
    let r = e.publish(684);
    assert!(r.is_err());
    assert!(r.unwrap_err().contains("ClockWentBackwards"));
}

// ---------------------------------------------------------------- 5. closed account revived

/// Class 5 (closed account revived). After a swap is cancelled its PDA is closed and the rent returned. Sending
/// lamports back to the address does not revive it: settle and cancel on the address fail, and the trader can
/// reuse the seed for a fresh swap whose state starts clean.
#[test]
fn closed_swap_cannot_be_revived_by_funding_its_address() {
    let mut e = seeded();
    let tr = e.trader.insecure_clone();
    open_default(&mut e, 1);
    let swap = e.swap_pda(&tr.pubkey(), 1);
    let ix = e.close_ix("trader_cancel_swap", &tr.pubkey(), 1, &tr.pubkey(), &0u64);
    e.must(&[ix], &[&tr]);
    assert!(e
        .svm
        .get_account(&swap)
        .is_none_or(|a| a.lamports == 0 && a.data.is_empty()));
    // Fund the dead address from the payer.
    let transfer = conv!(solana_system_interface::instruction::transfer(
        &e.payer.pubkey(),
        &swap,
        10_000_000
    ));
    e.must(&[transfer], &[]);
    assert_eq!(e.svm.get_account(&swap).unwrap().lamports, 10_000_000);
    let snap = pool_snapshot(&e);
    let ix = e.close_ix("crank_settle_swap", &tr.pubkey(), 1, &e.payer.pubkey(), &());
    let err = e
        .send(&[ix], &[])
        .expect_err("a funded dead address is not a swap");
    assert_no_panic(&err);
    let ix = e.close_ix("trader_cancel_swap", &tr.pubkey(), 1, &tr.pubkey(), &0u64);
    let err = e
        .send(&[ix], &[&tr])
        .expect_err("a funded dead address is not a swap");
    assert_no_panic(&err);
    assert_eq!(pool_snapshot(&e), snap);
    // The seed can be reused; the new swap starts from a clean state.
    open_default(&mut e, 1);
    let s: Swap = e.acct("Swap", &swap);
    assert_eq!(s.state, SwapState::Open);
    assert_eq!(s.opened_ts, e.clock().unix_timestamp);
    let p: Pool = e.acct("Pool", &e.pool);
    assert_eq!(p.open_swaps, 1);
}

/// Class 5, timelock. A cancelled operation's PDA is closed; funding the address does not let it execute, and
/// the proposer cannot re-queue under the same nonce (the nonce has advanced).
#[test]
fn cancelled_timelock_operation_cannot_be_revived() {
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
    let op = pda(&[b"op", &0u64.to_le_bytes()], &TIMELOCK);
    let kind = OperationKind::SetDelay {
        delay_slots: 500_000,
    };
    let queue = Instruction {
        program_id: TIMELOCK,
        accounts: [
            vec![rw(timelock), sigw(proposer.pubkey()), rw(op), ro(SYSTEM)],
            evt(TIMELOCK).to_vec(),
        ]
        .concat(),
        data: data("queue", &kind),
    };
    e.must(std::slice::from_ref(&queue), &[&proposer]);
    let cancel = Instruction {
        program_id: TIMELOCK,
        accounts: [
            vec![
                ro(timelock),
                sig(proposer.pubkey()),
                rw(proposer.pubkey()),
                rw(op),
            ],
            evt(TIMELOCK).to_vec(),
        ]
        .concat(),
        data: data("cancel", &()),
    };
    e.must(&[cancel], &[&proposer]);
    assert!(e.svm.get_account(&op).is_none_or(|a| a.data.is_empty()));
    let transfer = conv!(solana_system_interface::instruction::transfer(
        &payer, &op, 10_000_000
    ));
    e.must(&[transfer], &[]);
    e.warp(432_000, 432_000 / 2);
    let exec = Instruction {
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
    };
    let err = e
        .send(&[exec], &[&executor])
        .expect_err("a cancelled operation cannot execute");
    assert_no_panic(&err);
    let t: Timelock = e.acct("Timelock", &timelock);
    assert_eq!(t.delay_slots, 432_000, "delay unchanged");
    assert_eq!(t.nonce, 1, "nonce advanced past the cancelled operation");
    // Re-queueing lands on nonce 1, not on the dead address.
    let err = e
        .send(&[queue], &[&proposer])
        .expect_err("nonce 0 is spent");
    assert_no_panic(&err);
}

// ---------------------------------------------------------------- 6. arithmetic edges

/// Class 6 (arithmetic). Extreme arguments fail with program errors, never panics: notional at `u64::MAX`,
/// notional one above the leg cap on a maximal pool, shares at `u64::MAX`, `min_payout` at `u64::MAX`, a
/// deposit of `u64::MAX`, and a clock far in the future.
#[test]
fn arithmetic_edges_fail_cleanly() {
    let mut e = seeded();
    let lp = e.lp.insecure_clone();
    let tr = e.trader.insecure_clone();
    let snap = pool_snapshot(&e);
    let ix = e.open_ix(
        &tr.pubkey(),
        &open_args(LegKind::PayFixed, 0, u64::MAX, 9_999, 1),
    );
    e.must_fail(&[ix], &[&tr], "NotionalTooLarge");
    let ix = e.open_ix(
        &tr.pubkey(),
        &open_args(LegKind::PayFixed, 0, 50_000_000 * USDC, 9_999, 1),
    );
    e.must_fail(&[ix], &[&tr], "LegCap");
    // Exactly at the 48 percent cap passes; one unit more is LegCap.
    let cap = 4_800_000 * USDC;
    let ix = e.open_ix(
        &tr.pubkey(),
        &open_args(LegKind::PayFixed, 0, cap + 1, 9_999, 1),
    );
    let err = e.send(&[ix], &[&tr]).expect_err("one unit over the cap");
    assert!(err.contains("LegCap"), "{err}");
    let ix = e.open_ix(
        &tr.pubkey(),
        &open_args(LegKind::PayFixed, 0, cap, 9_999, 1),
    );
    let r = e.send(&[ix], &[&tr]);
    assert!(r.is_ok(), "exactly the cap must pass: {r:?}");
    let ix = e.withdraw_ix(&lp.pubkey(), u64::MAX, 0);
    let err = e.send(&[ix], &[&lp]).expect_err("u64::MAX shares");
    assert_no_panic(&err);
    let ix = e.deposit_ix(&lp.pubkey(), u64::MAX, 0);
    let err = e.send(&[ix], &[&lp]).expect_err("u64::MAX deposit");
    assert_no_panic(&err);
    let ix = e.deposit_ix(&lp.pubkey(), 1_000 * USDC, u64::MAX);
    e.must_fail(&[ix], &[&lp], "Slippage");
    // A floor above i64::MAX is reported as Overflow rather than Slippage (round-02 observation T-4); any
    // representable floor above the payout is Slippage.
    let ix = e.close_ix(
        "trader_cancel_swap",
        &tr.pubkey(),
        1,
        &tr.pubkey(),
        &u64::MAX,
    );
    let err = e.send(&[ix], &[&tr]).expect_err("u64::MAX floor");
    assert!(
        err.contains("Overflow") || err.contains("Slippage"),
        "{err}"
    );
    let ix = e.close_ix(
        "trader_cancel_swap",
        &tr.pubkey(),
        1,
        &tr.pubkey(),
        &(i64::MAX as u64),
    );
    e.must_fail(&[ix], &[&tr], "Slippage");
    let ix = e.withdraw_ix(&lp.pubkey(), SHARE, u64::MAX);
    e.must_fail(&[ix], &[&lp], "Slippage");
    let p: Pool = e.acct("Pool", &e.pool);
    assert_eq!(p.open_swaps, 1);
    assert_eq!(p.tvl, snap.0);
    // Clock far in the future: the index accrual and the AMM marks must fail with Overflow or succeed, never
    // panic.
    let mut c = e.clock();
    c.unix_timestamp = i64::MAX / 4;
    c.slot += 1_000;
    e.svm.set_sysvar(&c);
    let r = e.publish(684);
    if let Err(err) = &r {
        assert_no_panic(err);
    }
    let ix = e.close_ix("trader_cancel_swap", &tr.pubkey(), 1, &tr.pubkey(), &0u64);
    if let Err(err) = e.send(&[ix], &[&tr]) {
        assert_no_panic(&err);
    }
    let ix = e.close_ix("crank_settle_swap", &tr.pubkey(), 1, &e.payer.pubkey(), &());
    if let Err(err) = e.send(&[ix], &[]) {
        assert_no_panic(&err);
    }
}

/// Class 6, i128 side. A receive-fixed position whose floating leg runs to the 30,000 bp ceiling for a
/// 180-day tenor produces the largest negative mark the pricer can see; liquidation and settlement clamp to
/// collateral and never overflow.
#[test]
fn extreme_rates_clamp_rather_than_overflow() {
    let mut e = seeded();
    let tr = e.trader.insecure_clone();
    let ix = e.open_ix(
        &tr.pubkey(),
        &open_args(LegKind::ReceiveFixed, 3, 1_000_000 * USDC, 0, 1),
    );
    e.must(&[ix], &[&tr]);
    let s: Swap = e.acct("Swap", &e.swap_pda(&tr.pubkey(), 1));
    // Walk the index up by one band a day until the ceiling or exhaustion.
    for _ in 0..120 {
        let b: Benchmark = e.acct("Benchmark", &e.benchmark);
        if b.value_bp >= 30_000 {
            break;
        }
        e.warp(216_000, DAY);
        e.publish((b.ema_bp + 300).min(30_000)).unwrap();
    }
    let tvl0 = e.acct::<Pool>("Pool", &e.pool).tvl;
    let ix = e.close_ix(
        "crank_liquidate_swap",
        &tr.pubkey(),
        1,
        &e.payer.pubkey(),
        &(),
    );
    let r = e.send(&[ix], &[]);
    match r {
        Ok(_) => {
            let p: Pool = e.acct("Pool", &e.pool);
            assert_eq!(p.open_swaps, 0);
            assert!(
                p.tvl <= tvl0 + s.collateral,
                "LP gain is at most the collateral"
            );
            assert!(p.tvl >= tvl0, "the pool cannot lose on a liquidation");
        }
        Err(err) => {
            assert_no_panic(&err);
            assert!(
                err.contains("NotLiquidatable") || err.contains("BenchmarkStale"),
                "{err}"
            );
        }
    }
}

// ---------------------------------------------------------------- 7. PDA seed collisions

/// Class 7 (PDA seeds). A second pool on the same benchmark collides on `["pool", benchmark]` and is refused;
/// a duplicate benchmark id is refused; a swap seed is unique per (pool, trader, seed) so two traders may use
/// the same seed and one trader cannot reuse an open seed.
#[test]
fn pda_seed_collisions_are_refused() {
    let mut e = seeded();
    let tr = e.trader.insecure_clone();
    let r = e.create_pool(e.benchmark, None, HookFlags::default());
    assert!(r.is_err(), "second pool on the same benchmark");
    let a = e.authority.insecure_clone();
    let dup = Instruction {
        program_id: INDEX,
        accounts: [
            vec![
                rw(e.registry),
                sig(a.pubkey()),
                sigw(e.payer.pubkey()),
                rw(e.benchmark),
                ro(SYSTEM),
            ],
            evt(INDEX).to_vec(),
        ]
        .concat(),
        data: data(
            "create_benchmark",
            &CreateBenchmarkArgs {
                id: BENCH_ID,
                source: Pubkey::new_unique(),
                guards: DEFAULT_GUARDS,
            },
        ),
    };
    let err = e.send(&[dup], &[&a]).expect_err("duplicate benchmark id");
    assert_no_panic(&err);
    let b: Benchmark = e.acct("Benchmark", &e.benchmark);
    assert_eq!(b.value_bp, 684, "existing benchmark untouched");
    open_default(&mut e, 1);
    let other = e.new_actor(1_000_000 * USDC, false);
    let ix = e.open_ix(
        &other.pubkey(),
        &open_args(LegKind::PayFixed, 0, 100_000 * USDC, 9_999, 1),
    );
    e.must(&[ix], &[&other]);
    let ix = e.open_ix(
        &tr.pubkey(),
        &open_args(LegKind::PayFixed, 0, 100_000 * USDC, 9_999, 1),
    );
    let err = e.send(&[ix], &[&tr]).expect_err("open seed reuse");
    assert_no_panic(&err);
    let p: Pool = e.acct("Pool", &e.pool);
    assert_eq!(p.open_swaps, 2);
}

/// Review F-25 characterisation. `admin_create_pool` takes `benchmark` unchecked; a pool created on an
/// account that is not a `Benchmark` (the index registry here) must either be refused at creation or be
/// unusable: no open can ever succeed on it. The test records which of the two holds.
#[test]
fn f25_pool_on_a_non_benchmark_account_is_unusable() {
    let mut e = seeded();
    let lp = e.lp.insecure_clone();
    let tr = e.trader.insecure_clone();
    let registry = e.registry;
    match e.create_pool(registry, None, HookFlags::default()) {
        Err(err) => {
            assert_no_panic(&err);
            eprintln!("F-25: pool creation on a non-benchmark account refused (fixed)");
        }
        Ok(k) => {
            eprintln!("F-25: pool creation on a non-benchmark account accepted (characterisation); opens must fail");
            e.use_pool(&k);
            let m = e.share_mint;
            e.create_ata(&lp.pubkey(), &m);
            let ix = e.deposit_ix(&lp.pubkey(), 1_000_000 * USDC, 0);
            let r = e.send(&[ix], &[&lp]);
            let ix = e.open_ix(
                &tr.pubkey(),
                &open_args(LegKind::PayFixed, 0, 10_000 * USDC, 9_999, 1),
            );
            let err = e
                .send(&[ix], &[&tr])
                .expect_err("no open on a pool without a benchmark");
            assert_no_panic(&err);
            if r.is_ok() {
                let shares = e.token_amount(&ata(&lp.pubkey(), &e.share_mint));
                let ix = e.withdraw_ix(&lp.pubkey(), shares, 0);
                e.must(&[ix], &[&lp]);
            }
        }
    }
}

// ---------------------------------------------------------------- 8. token account owner and delegate

/// Class 8 (token ownership). A delegate approved on another owner's token account cannot use it as `lp_usdc`
/// or `trader_usdc`; the program checks `owner == signer`, not the SPL delegate. A share account owned by a
/// third party is likewise refused as the destination.
#[test]
fn token_owner_checks_ignore_spl_delegates() {
    let mut e = seeded();
    let tr = e.trader.insecure_clone();
    let victim = e.new_actor(1_000_000 * USDC, true);
    let v_usdc = ata(&victim.pubkey(), &USDC_DEVNET);
    let v_shares = ata(&victim.pubkey(), &e.share_mint);
    // The victim approves the trader as delegate over both accounts.
    let approve = conv!(spl_token_interface::instruction::approve(
        &TOKEN_PROGRAM,
        &v_usdc,
        &tr.pubkey(),
        &victim.pubkey(),
        &[],
        u64::MAX,
    )
    .unwrap());
    e.must(&[approve], &[&victim]);
    let approve = conv!(spl_token_interface::instruction::approve(
        &TOKEN_PROGRAM,
        &v_shares,
        &tr.pubkey(),
        &victim.pubkey(),
        &[],
        u64::MAX,
    )
    .unwrap());
    e.must(&[approve], &[&victim]);
    let snap = pool_snapshot(&e);
    let m = e.share_mint;
    e.create_ata(&tr.pubkey(), &m);
    let deposit = e.deposit_ix(&tr.pubkey(), 1_000 * USDC, 0);
    let ix = Env::substitute(&deposit, ata(&tr.pubkey(), &USDC_DEVNET), v_usdc);
    e.must_fail(&[ix], &[&tr], "TokenOwner");
    let ix = Env::substitute(&deposit, ata(&tr.pubkey(), &e.share_mint), v_shares);
    e.must_fail(&[ix], &[&tr], "TokenOwner");
    let open = e.open_ix(
        &tr.pubkey(),
        &open_args(LegKind::PayFixed, 0, 10_000 * USDC, 9_999, 1),
    );
    let ix = Env::substitute(&open, ata(&tr.pubkey(), &USDC_DEVNET), v_usdc);
    e.must_fail(&[ix], &[&tr], "TokenOwner");
    assert_eq!(pool_snapshot(&e), snap);
    assert_eq!(
        e.token_amount(&v_usdc),
        1_000_000 * USDC,
        "the victim's balance is untouched"
    );
    // The victim can still use their own accounts.
    let ix = e.deposit_ix(&victim.pubkey(), 1_000 * USDC, 0);
    e.must(&[ix], &[&victim]);
}

// ---------------------------------------------------------------- 9. Token-2022 rejection

/// Class 9 (token program). The settlement mint must be a classic SPL Token mint: `admin_initialise_global`
/// refuses a Token-2022 mint whether the token program passed is Token-2022 or Token; and on a live protocol a
/// Token-2022 program id or a foreign mint in any instruction is refused.
#[test]
fn token_2022_mint_and_program_are_refused() {
    let mut e = boot();
    let a = e.authority.insecure_clone();
    let mint22 = {
        let ma = e.mint_authority.pubkey();
        raw_mint(&mut e, TOKEN_2022, ma)
    };
    let t = e.treasury_owner.pubkey();
    let treasury = e.create_ata(&t, &USDC_DEVNET);
    let escrow = e.create_ata(&buyback_owner(&t), &USDC_DEVNET);
    let args = InitialiseGlobalArgs {
        guardian: e.guardian.pubkey(),
        param_delay_slots: 432_000,
        limited_mode_cap: 100_000 * USDC,
    };
    for token_program in [TOKEN_2022, TOKEN_PROGRAM] {
        let ix = Instruction {
            program_id: SWAP_AMM,
            accounts: vec![
                rw(e.global),
                sig(a.pubkey()),
                sigw(e.payer.pubkey()),
                ro(mint22),
                rw(e.fee_vault),
                ro(treasury),
                ro(escrow),
                ro(token_program),
                ro(SYSTEM),
            ],
            data: data("admin_initialise_global", &args),
        };
        let err = e.send(&[ix], &[&a]).expect_err("Token-2022 mint refused");
        assert_no_panic(&err);
        assert!(
            e.svm
                .get_account(&e.global)
                .is_none_or(|g| g.data.is_empty()),
            "Global not created"
        );
    }
    // Live protocol: Token-2022 as token_program on each path.
    let mut e = seeded();
    let lp = e.lp.insecure_clone();
    let tr = e.trader.insecure_clone();
    let snap = pool_snapshot(&e);
    let deposit = e.deposit_ix(&lp.pubkey(), 1_000 * USDC, 0);
    let ix = Env::substitute(&deposit, TOKEN_PROGRAM, TOKEN_2022);
    let err = e
        .send(&[ix], &[&lp])
        .expect_err("Token-2022 program on deposit");
    assert!(
        err.contains("TokenProgram")
            || err.contains("ConstraintHasOne")
            || err.contains("InvalidProgramId"),
        "{err}"
    );
    let open = e.open_ix(
        &tr.pubkey(),
        &open_args(LegKind::PayFixed, 0, 10_000 * USDC, 9_999, 1),
    );
    let ix = Env::substitute(&open, TOKEN_PROGRAM, TOKEN_2022);
    let err = e
        .send(&[ix], &[&tr])
        .expect_err("Token-2022 program on open");
    assert_no_panic(&err);
    let withdraw = e.withdraw_ix(&lp.pubkey(), SHARE, 0);
    let ix = Env::substitute(&withdraw, TOKEN_PROGRAM, TOKEN_2022);
    let err = e
        .send(&[ix], &[&lp])
        .expect_err("Token-2022 program on withdraw");
    assert_no_panic(&err);
    let mint22 = {
        let ma = e.mint_authority.pubkey();
        raw_mint(&mut e, TOKEN_2022, ma)
    };
    let ix = Env::substitute(&open, USDC_DEVNET, mint22);
    let err = e.send(&[ix], &[&tr]).expect_err("Token-2022 mint on open");
    assert_no_panic(&err);
    // A pool cannot be created with a foreign mint either: the pool takes the mint from Global.
    let bench2 = e.create_benchmark(*b"t22-usdc\0\0\0\0\0\0\0\0", 684);
    let k = e.create_pool(bench2, None, HookFlags::default()).unwrap();
    let p: Pool = e.acct("Pool", &k.pool);
    let mint_acct = e.svm.get_account(&p.share_mint).unwrap();
    assert_eq!(
        mint_acct.owner, TOKEN_PROGRAM,
        "share mint lives under classic SPL Token"
    );
    assert_eq!(pool_snapshot(&e), snap);
}

// ---------------------------------------------------------------- 13. authority and mode boundaries

/// Class 13 (authorisation). The guardian can only move the mode towards Halted; a stranger can do nothing;
/// the authority cannot transfer authority without the incoming key's signature; pool parameter changes wait
/// the configured delay.
#[test]
fn authority_boundaries_hold() {
    let mut e = seeded();
    let a = e.authority.insecure_clone();
    let g = e.guardian.insecure_clone();
    let stranger = Keypair::new();
    e.svm.airdrop(&stranger.pubkey(), 1_000_000_000).unwrap();
    assert!(e.set_mode(&stranger, OperatingMode::Halted).is_err());
    e.set_mode(&g, OperatingMode::WithdrawOnly).unwrap();
    let r = e.set_mode(&g, OperatingMode::Normal);
    assert!(r.is_err(), "guardian cannot loosen the mode");
    assert!(r.unwrap_err().contains("GuardianScope"));
    e.set_mode(&a, OperatingMode::Normal).unwrap();
    // admin_set_authority without the incoming signature.
    let incoming = Keypair::new();
    let ix = Instruction {
        program_id: SWAP_AMM,
        accounts: [
            vec![rw(e.global), sig(a.pubkey()), ro(incoming.pubkey())],
            evt(SWAP_AMM).to_vec(),
        ]
        .concat(),
        data: data("admin_set_authority", &incoming.pubkey()),
    };
    let err = e
        .send(&[ix], &[&a])
        .expect_err("incoming authority must co-sign");
    assert_no_panic(&err);
    let gl: Global = e.acct("Global", &e.global);
    assert_eq!(gl.authority, a.pubkey());
    // Index set_authority (F-19 fix): the new registry authority must co-sign.
    let ix = Instruction {
        program_id: INDEX,
        accounts: vec![rw(e.registry), sig(a.pubkey()), ro(incoming.pubkey())],
        data: data("set_authority", &()),
    };
    let err = e
        .send(&[ix], &[&a])
        .expect_err("registry transfer needs the incoming signature");
    assert_no_panic(&err);
}
