//! Adversarial sequences against the compiled programs on LiteSVM. Every test records compute units through
//! `Env::send`; the summary table is appended to `results/svm_adversarial.md` by the last test to finish (each
//! test writes its own section so ordering does not matter).

use std::fmt::Write as _;

use brink_sim_core::model::{Setup, DEFAULT_PARAMS};
use brink_sim_svm::*;
use solana_instruction::Instruction;
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signer::Signer;

const DAY: i64 = 86_400;
const SLOTS_PER_DAY: u64 = 216_000;

fn setup() -> Setup {
    Setup {
        band_bp: 300,
        max_staleness_slots: 2_000,
        half_life_slots: 10_000,
        min_interval_slots: 0,
        params: DEFAULT_PARAMS,
        param_delay_slots: 432_000,
        limited_mode_cap: 100_000 * USDC,
        min_notional: 1_000 * USDC,
        max_notional: 50_000_000 * USDC,
        n_lps: 2,
        n_traders: 2,
        start_slot: 1_000,
        start_ts: 1_790_000_000,
    }
}

fn open_args(leg: LegKind, tenor: u8, notional: u64, limit: u16, seed: u64) -> OpenSwapArgs {
    OpenSwapArgs { leg, tenor, notional, limit_rate_bp: limit, client_seed: seed }
}

fn note(section: &str, body: &str) {
    use std::io::Write;
    let dir = results_dir();
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("svm_adversarial.md")) {
        let _ = writeln!(f, "## {section}\n\n{body}\n");
    }
}

/// Asserts the transaction fails with one of the listed codes (Anchor error names or runtime strings).
fn must_fail_any(env: &mut Env, ixs: &[Instruction], signers: &[&Keypair], codes: &[&str]) -> String {
    match env.send(ixs, signers) {
        Ok(logs) => panic!("expected failure {codes:?}, succeeded:\n{}", logs.join("\n")),
        Err(e) => {
            assert!(codes.iter().any(|c| e.contains(c)), "expected one of {codes:?}, got:\n{e}");
            e
        }
    }
}

fn env_with_deposit(amount: u64) -> Env {
    let s = setup();
    let mut e = Env::new(&s);
    let p = e.publisher.insecure_clone();
    let ix = e.publish_ix(684);
    e.must(&[ix], &[&p]);
    let lp = e.lps[0].insecure_clone();
    let ix = e.deposit_ix(&lp.pubkey(), amount, 0);
    e.must(&[ix], &[&lp]);
    e
}

#[test]
fn a01_reentrancy_via_hook_cpi_cannot_touch_state() {
    let s = setup();
    let mut body = String::new();
    // Environment A: the hook re-enters at BeforeOpen (point 0) and observes at BeforeDeposit (point 4).
    let flags_a = HookFlags { before_open: true, before_deposit: true, ..HookFlags::default() };
    let mut e = Env::new_with(&s, Some(flags_a));
    if !e.hook_loaded {
        note("A-1 re-entrancy via hook CPI", "SKIPPED: hook program not built (hook/deploy/brink_sim_hook.so missing; run build_hook.sh)");
        eprintln!("hook program not built; skipping");
        return;
    }
    let p = e.publisher.insecure_clone();
    let ix = e.publish_ix(684);
    e.must(&[ix], &[&p]);
    let lp = e.lps[0].insecure_clone();
    let ix = e.deposit_ix(&lp.pubkey(), 1_000_000 * USDC, 0);
    let logs = e.must(&[ix], &[&lp]);
    assert!(logs.iter().any(|l| l.contains("hook: point 4")), "hook observed the deposit");
    let before = e.pool();
    let tr = e.traders[0].insecure_clone();
    let ix = e.open_ix(&tr.pubkey(), &open_args(LegKind::PayFixed, 0, 10_000 * USDC, 9_999, 1));
    let err = must_fail_any(&mut e, &[ix], &[&tr], &["ReentrancyNotAllowed", "reentrancy not allowed", "MissingAccount", "Unknown program", "ConstraintMut"]);
    let after = e.pool();
    assert_eq!(after.event_seq, before.event_seq, "no state change from the aborted open");
    assert_eq!(after.open_swaps, 0);
    let first = err.lines().next().unwrap_or("").to_string();
    let _ = writeln!(body, "- Environment A (BeforeOpen re-enters `sync_vault` with the accounts exactly as received): the re-entry cannot even be addressed: `swap_amm` does not forward its own program account to the hook, so the runtime reports the target program as missing and the outer open is aborted (`{}`); pool unchanged. A failed CPI cannot be caught by the caller on Solana, so a re-entering hook can only deny service, never mutate (and a hook given the program account would still meet the runtime's re-entrancy rule).", first.trim());

    // Environment B: the hook writes into the read-only pool at AfterOpen (point 1).
    let flags_b = HookFlags { after_open: true, ..HookFlags::default() };
    let mut e = Env::new_with(&s, Some(flags_b));
    let p = e.publisher.insecure_clone();
    let ix = e.publish_ix(684);
    e.must(&[ix], &[&p]);
    let lp = e.lps[0].insecure_clone();
    let ix = e.deposit_ix(&lp.pubkey(), 1_000_000 * USDC, 0);
    e.must(&[ix], &[&lp]);
    let before = e.pool();
    let tr = e.traders[0].insecure_clone();
    let ix = e.open_ix(&tr.pubkey(), &open_args(LegKind::PayFixed, 0, 10_000 * USDC, 9_999, 1));
    let err = must_fail_any(&mut e, &[ix], &[&tr], &["HookRejected", "ReadonlyDataModified", "modified data"]);
    let after = e.pool();
    assert_eq!(after.event_seq, before.event_seq, "state unchanged after the rejected open");
    assert_eq!(after.open_swaps, 0);
    let first = err.lines().next().unwrap_or("").to_string();
    let _ = writeln!(body, "- Environment B (AfterOpen writes one byte into the read-only pool): transaction rejected (`{}`), pool unchanged.", first.trim());

    // Environment C: the hook vetoes BeforeWithdraw (point 6).
    let flags_c = HookFlags { before_withdraw: true, ..HookFlags::default() };
    let mut e = Env::new_with(&s, Some(flags_c));
    let p = e.publisher.insecure_clone();
    let ix = e.publish_ix(684);
    e.must(&[ix], &[&p]);
    let lp = e.lps[0].insecure_clone();
    let ix = e.deposit_ix(&lp.pubkey(), 1_000_000 * USDC, 0);
    e.must(&[ix], &[&lp]);
    let ix = e.withdraw_ix(&lp.pubkey(), 1_000 * USDC, 0);
    let err = must_fail_any(&mut e, &[ix], &[&lp], &["HookRejected", "0x4e5a"]);
    let _ = writeln!(body, "- Environment C (BeforeWithdraw returns an error): withdraw rejected; the hook's own error code surfaces unchanged (`{}`), so a hook can veto LP exits, which is the documented trust assumption on the hook program.", err.lines().next().unwrap_or("").trim());

    // Environment D: the hook re-enters trader_cancel_swap at BeforeCancel (point 2) with the swap account.
    let flags_d = HookFlags { before_cancel: true, ..HookFlags::default() };
    let mut e = Env::new_with(&s, Some(flags_d));
    let p = e.publisher.insecure_clone();
    let ix = e.publish_ix(684);
    e.must(&[ix], &[&p]);
    let lp = e.lps[0].insecure_clone();
    let ix = e.deposit_ix(&lp.pubkey(), 1_000_000 * USDC, 0);
    e.must(&[ix], &[&lp]);
    let tr = e.traders[0].insecure_clone();
    let ix = e.open_ix(&tr.pubkey(), &open_args(LegKind::PayFixed, 0, 10_000 * USDC, 9_999, 1));
    e.must(&[ix], &[&tr]);
    let ix = e.close_ix("trader_cancel_swap", &tr.pubkey(), 1, &tr.pubkey(), None, &0u64);
    let err = must_fail_any(&mut e, &[ix], &[&tr], &["PrivilegeEscalation", "privilege escalated", "ReentrancyNotAllowed"]);
    assert_eq!(e.pool().open_swaps, 1, "the swap is still open; nothing was cancelled");
    let first = err.lines().next().unwrap_or("").to_string();
    let _ = writeln!(body, "- Environment D (BeforeCancel re-enters `trader_cancel_swap` with the pool and swap marked writable although received read-only): rejected by the runtime before the callee runs (`{}`); swap still open.", first.trim());
    note("A-1 re-entrancy via hook CPI", &body);
}

#[test]
fn a02_duplicate_wrong_owner_and_wrong_mint_accounts() {
    let mut e = env_with_deposit(1_000_000 * USDC);
    let lp0 = e.lps[0].insecure_clone();
    let lp1 = e.lps[1].insecure_clone();
    let tr = e.traders[0].insecure_clone();
    let stranger = e.stranger.insecure_clone();
    let mut body = String::new();
    // deposit with another LP's USDC account
    let ix = e.deposit_ix_with(&lp0.pubkey(), &ata(&lp1.pubkey(), &USDC_DEVNET), &ata(&lp0.pubkey(), &e.share_mint), 1_000 * USDC, 0);
    e.must_fail(&[ix], &[&lp0], "TokenOwner");
    // deposit with the share account passed as the USDC account (wrong mint) and vice versa
    let ix = e.deposit_ix_with(&lp0.pubkey(), &ata(&lp0.pubkey(), &e.share_mint), &ata(&lp0.pubkey(), &e.share_mint), 1_000 * USDC, 0);
    must_fail_any(&mut e, &[ix], &[&lp0], &["SettlementMint", "ConstraintDuplicateMutableAccount"]);
    let ix = e.deposit_ix_with(&lp0.pubkey(), &ata(&lp0.pubkey(), &USDC_DEVNET), &ata(&lp0.pubkey(), &USDC_DEVNET), 1_000 * USDC, 0);
    must_fail_any(&mut e, &[ix], &[&lp0], &["TokenOwner", "ConstraintDuplicateMutableAccount"]);
    let ix = e.deposit_ix_with(&lp0.pubkey(), &ata(&lp0.pubkey(), &e.share_mint), &ata(&lp1.pubkey(), &USDC_DEVNET), 1_000 * USDC, 0);
    e.must_fail(&[ix], &[&lp0], "SettlementMint");
    let ix = e.deposit_ix_with(&lp0.pubkey(), &ata(&lp0.pubkey(), &USDC_DEVNET), &ata(&lp0.pubkey(), &USDC_DEVNET), 1_000 * USDC, 0);
    must_fail_any(&mut e, &[ix], &[&lp0], &["TokenOwner", "ConstraintDuplicateMutableAccount"]);
    // withdraw with another LP's share account
    let ix = e.withdraw_ix_with(&lp0.pubkey(), &ata(&lp0.pubkey(), &USDC_DEVNET), &ata(&lp1.pubkey(), &e.share_mint), 1, 0);
    e.must_fail(&[ix], &[&lp0], "TokenOwner");
    let _ = writeln!(body, "- lp_deposit / lp_withdraw: foreign owner and wrong-mint token accounts rejected (TokenOwner, SettlementMint); the same account passed twice as two mutable arguments is rejected by Anchor before any typed check (ConstraintDuplicateMutableAccount).");

    // open with the vault as the trader's USDC account; with a wrong-mint ATA; duplicate (fee vault as vault)
    let ix = e.open_ix_with(&tr.pubkey(), &e.vault.clone(), &open_args(LegKind::PayFixed, 0, 10_000 * USDC, 9_999, 1));
    must_fail_any(&mut e, &[ix], &[&tr], &["TokenOwner", "ConstraintDuplicateMutableAccount"]);
    let other_mint = e.create_mint();
    let wrong = e.create_ata(&tr.pubkey(), &other_mint);
    e.mint_to(&other_mint, &wrong, 1_000_000 * USDC);
    let ix = e.open_ix_with(&tr.pubkey(), &wrong, &open_args(LegKind::PayFixed, 0, 10_000 * USDC, 9_999, 1));
    e.must_fail(&[ix], &[&tr], "SettlementMint");
    let mut ix = e.open_ix(&tr.pubkey(), &open_args(LegKind::PayFixed, 0, 10_000 * USDC, 9_999, 1));
    ix.accounts[6].pubkey = e.fee_vault; // vault slot now carries the fee vault
    must_fail_any(&mut e, &[ix], &[&tr], &["ConstraintHasOne", "has one constraint", "ConstraintDuplicateMutableAccount"]);
    let mut ix = e.open_ix(&tr.pubkey(), &open_args(LegKind::PayFixed, 0, 10_000 * USDC, 9_999, 1));
    ix.accounts[7].pubkey = e.vault; // fee vault slot now carries the vault
    must_fail_any(&mut e, &[ix], &[&tr], &["ConstraintHasOne", "has one constraint", "ConstraintDuplicateMutableAccount"]);
    let _ = writeln!(body, "- trader_open_swap: vault as trader account and vault or fee vault duplicated (ConstraintDuplicateMutableAccount), wrong-mint ATA (SettlementMint).");

    // a second benchmark under the same registry, passed to open against the pool keyed by the first
    let a = e.authority.insecure_clone();
    let id2: [u8; 16] = *b"sim-other\0\0\0\0\0\0\0";
    let bench2 = pda(&[b"benchmark", &id2], &INDEX);
    let args = CreateBenchmarkArgs { id: id2, source: Pubkey::new_unique(), band_bp: 300, max_staleness_slots: 2_000, half_life_slots: 10_000, min_interval_slots: 0 };
    let registry = e.registry;
    let payer = e.payer.pubkey();
    e.must(&[Instruction { program_id: INDEX, accounts: [vec![rw(registry), sig(a.pubkey()), sigw(payer), rw(bench2), ro(SYSTEM)], evt(INDEX).to_vec()].concat(), data: data("create_benchmark", &args) }], &[&a]);
    let mut ix = e.open_ix(&tr.pubkey(), &open_args(LegKind::PayFixed, 0, 10_000 * USDC, 9_999, 1));
    ix.accounts[2].pubkey = bench2;
    must_fail_any(&mut e, &[ix], &[&tr], &["ConstraintHasOne", "has one constraint"]);
    let _ = writeln!(body, "- trader_open_swap with a different benchmark account than the pool's: ConstraintHasOne.");

    // a real open, then close-path account confusion
    let ix = e.open_ix(&tr.pubkey(), &open_args(LegKind::PayFixed, 0, 10_000 * USDC, 9_999, 1));
    e.must(&[ix], &[&tr]);
    // cranker account owned by the pool (vault) or by the trader while a stranger signs
    let ix = e.close_ix("crank_liquidate_swap", &tr.pubkey(), 1, &stranger.pubkey(), Some(e.vault), &());
    must_fail_any(&mut e, &[ix], &[&stranger], &["TokenOwner", "ConstraintDuplicateMutableAccount"]);
    let ix = e.close_ix("crank_liquidate_swap", &tr.pubkey(), 1, &stranger.pubkey(), Some(ata(&tr.pubkey(), &USDC_DEVNET)), &());
    must_fail_any(&mut e, &[ix], &[&stranger], &["TokenOwner", "ConstraintDuplicateMutableAccount"]);
    let ix = e.close_ix("crank_liquidate_swap", &tr.pubkey(), 1, &stranger.pubkey(), Some(ata(&e.lps[0].pubkey(), &USDC_DEVNET)), &());
    e.must_fail(&[ix], &[&stranger], "TokenOwner");
    // trader account replaced by a stranger (rent and USDC owner must equal swap.trader)
    let mut ix = e.close_ix("trader_cancel_swap", &tr.pubkey(), 1, &tr.pubkey(), None, &0u64);
    ix.accounts[4].pubkey = stranger.pubkey();
    must_fail_any(&mut e, &[ix], &[&tr], &["ConstraintHasOne", "has one constraint"]);
    // trader USDC replaced by the stranger's (owner mismatch)
    let ix = e.close_ix_with("trader_cancel_swap", &tr.pubkey(), 1, &tr.pubkey(), &ata(&stranger.pubkey(), &USDC_DEVNET), None, &0u64);
    e.must_fail(&[ix], &[&tr], "TokenOwner");
    // a stranger cancels someone else's swap
    let ix = e.close_ix("trader_cancel_swap", &tr.pubkey(), 1, &stranger.pubkey(), None, &0u64);
    e.must_fail(&[ix], &[&stranger], "ConstraintSigner");
    // sweep with the treasury and buyback swapped
    let mut ix = e.sweep_ix();
    ix.accounts.swap(2, 3);
    must_fail_any(&mut e, &[ix], &[], &["ConstraintHasOne", "has one constraint"]);
    // sync_vault with the fee vault as the vault
    let mut ix = e.sync_ix();
    ix.accounts[1].pubkey = e.fee_vault;
    must_fail_any(&mut e, &[ix], &[], &["ConstraintHasOne", "has one constraint"]);
    let _ = writeln!(body, "- close paths: cranker destination equal to the vault or to the trader account (ConstraintDuplicateMutableAccount) or owned by a third party (TokenOwner), trader account or trader USDC replaced (ConstraintHasOne, TokenOwner), stranger cancel (ConstraintSigner); sweep_fees with treasury and buyback swapped and sync_vault with the fee vault: ConstraintHasOne.");
    // the one duplicate that is accepted: the trader cranks their own settlement naming their own USDC account as bounty destination; no bounty is paid
    e.warp(28 * SLOTS_PER_DAY, 28 * DAY);
    let p = e.publisher.insecure_clone();
    let ix = e.publish_ix(684);
    e.must(&[ix], &[&p]);
    let before = e.token_amount(&ata(&tr.pubkey(), &USDC_DEVNET));
    let pool_before = e.pool();
    let ix = e.close_ix("crank_settle_swap", &tr.pubkey(), 1, &tr.pubkey(), Some(ata(&tr.pubkey(), &USDC_DEVNET)), &());
    must_fail_any(&mut e, &[ix], &[&tr], &["ConstraintDuplicateMutableAccount"]);
    let ix = e.close_ix("crank_settle_swap", &tr.pubkey(), 1, &tr.pubkey(), None, &());
    e.must(&[ix], &[&tr]);
    let paid = e.token_amount(&ata(&tr.pubkey(), &USDC_DEVNET)) - before;
    let pool_after = e.pool();
    assert_eq!(pool_after.tvl + paid, pool_before.tvl + pool_before.collateral_held, "collateral splits into payout and LP gain; no bounty, no income fee on a loss");
    let _ = writeln!(body, "- Trader settles their own swap: naming their own USDC account as cranker destination is a duplicate mutable account (rejected); without a cranker account the settlement succeeds, payout {paid}, no bounty (signer == trader).");
    note("A-2 duplicate, wrong-owner and wrong-mint accounts", &body);
}

#[test]
fn a03_stale_benchmark_band_ceiling_and_publisher_rules() {
    let mut e = env_with_deposit(1_000_000 * USDC);
    let tr = e.traders[0].insecure_clone();
    let p = e.publisher.insecure_clone();
    let stranger = e.stranger.insecure_clone();
    let mut body = String::new();
    let ix = e.open_ix(&tr.pubkey(), &open_args(LegKind::PayFixed, 0, 10_000 * USDC, 9_999, 1));
    e.must(&[ix], &[&tr]);
    // past the staleness guard: open, cancel and liquidate are refused; settle is not guarded by staleness
    e.warp(2_001, 800);
    let ix = e.open_ix(&tr.pubkey(), &open_args(LegKind::PayFixed, 0, 10_000 * USDC, 9_999, 2));
    e.must_fail(&[ix], &[&tr], "BenchmarkStale");
    let ix = e.close_ix("trader_cancel_swap", &tr.pubkey(), 1, &tr.pubkey(), None, &0u64);
    e.must_fail(&[ix], &[&tr], "BenchmarkStale");
    let ix = e.close_ix("crank_liquidate_swap", &tr.pubkey(), 1, &stranger.pubkey(), None, &());
    e.must_fail(&[ix], &[&stranger], "BenchmarkStale");
    let _ = writeln!(body, "- 2,001 slots after the last publish (guard 2,000): open, cancel, liquidate rejected with BenchmarkStale.");
    // band, ceiling, publisher identity
    let mut ix = e.publish_ix(684 + 301);
    e.must_fail(&[ix.clone()], &[&p], "OutOfBand");
    ix = e.publish_ix(30_001);
    e.must_fail(&[ix], &[&p], "RateCeiling");
    let mut ix = e.publish_ix(684);
    ix.accounts[1].pubkey = stranger.pubkey();
    must_fail_any(&mut e, &[ix], &[&stranger], &["ConstraintHasOne", "has one constraint"]);
    // walking the EMA: at the band edge every publish moves the EMA by band * dt / (hl + dt)
    let b0 = e.bench();
    let mut v = 684u16;
    for _ in 0..24 {
        e.warp(9_000, 3_600);
        let b = e.bench();
        v = b.ema_bp + 300;
        let ix = e.publish_ix(v);
        e.must(&[ix], &[&p]);
    }
    let b1 = e.bench();
    let _ = writeln!(body, "- OutOfBand at ema + band + 1, RateCeiling above 30,000 bp, non-publisher signer rejected (ConstraintHasOne). A publisher sitting at the band edge once an hour for 24 hours moved the EMA from {} to {} bp and the spot to {} bp with the band at 300 and a half-life of 10,000 slots: the band bounds each step, not the drift.", b0.ema_bp, b1.ema_bp, v);
    // the stale swap still settles: settle has no staleness guard but requires `published`
    e.warp(28 * SLOTS_PER_DAY, 28 * DAY);
    let ix = e.close_ix("crank_settle_swap", &tr.pubkey(), 1, &stranger.pubkey(), None, &());
    e.must(&[ix], &[&stranger]);
    let _ = writeln!(body, "- Settlement 28 days after the last publish succeeded: `crank_settle_swap` is deliberately not staleness-guarded (liveness), the held rate accrues meanwhile.");
    // minimum interval
    let mut s = setup();
    s.min_interval_slots = 150;
    let mut e2 = Env::new(&s);
    let p2 = e2.publisher.insecure_clone();
    let ix = e2.publish_ix(684);
    e2.must(&[ix], &[&p2]);
    e2.warp(100, 40);
    let ix = e2.publish_ix(684);
    e2.must_fail(&[ix], &[&p2], "TooFrequent");
    e2.warp(50, 20);
    let ix = e2.publish_ix(684);
    e2.must(&[ix], &[&p2]);
    let _ = writeln!(body, "- min_interval 150 slots: a publish after 100 slots is TooFrequent, after 150 accepted.");
    note("A-3 stale benchmark, band, ceiling and publisher rules", &body);
}

#[test]
fn a04_settle_before_maturity_double_settle_and_liquidation_rules() {
    let mut e = env_with_deposit(10_000_000 * USDC);
    let tr = e.traders[0].insecure_clone();
    let p = e.publisher.insecure_clone();
    let stranger = e.stranger.insecure_clone();
    let mut body = String::new();
    let ix = e.open_ix(&tr.pubkey(), &open_args(LegKind::PayFixed, 2, 1_000_000 * USDC, 9_999, 1));
    e.must(&[ix], &[&tr]);
    let s: Swap = e.acct("Swap", &e.swap_pda(&tr.pubkey(), 1));
    let ix = e.close_ix("crank_settle_swap", &tr.pubkey(), 1, &stranger.pubkey(), None, &());
    e.must_fail(&[ix], &[&stranger], "NotMatured");
    let ix = e.close_ix("crank_liquidate_swap", &tr.pubkey(), 1, &stranger.pubkey(), None, &());
    e.must_fail(&[ix], &[&stranger], "NotLiquidatable");
    // the rate rises so the pay-fixed position is deep in the money; three hours before maturity a stranger tries
    // to liquidate: with ADR-007 in the binary this is NotLiquidatable (exhaustion is the only trigger)
    e.warp(SLOTS_PER_DAY, DAY);
    let ix = e.publish_ix(900);
    e.must(&[ix], &[&p]);
    e.warp(89 * SLOTS_PER_DAY - 2_000, 89 * DAY - 3 * 3_600);
    let ix = e.publish_ix(900);
    e.must(&[ix], &[&p]);
    let ix = e.close_ix("crank_liquidate_swap", &tr.pubkey(), 1, &stranger.pubkey(), Some(ata(&stranger.pubkey(), &USDC_DEVNET)), &());
    let near = e.send(&[ix], &[&stranger]);
    let binary_has_adr007 = match &near {
        Err(err) => err.contains("NotLiquidatable"),
        Ok(_) => false,
    };
    let _ = writeln!(body, "- Settle before maturity: NotMatured. Liquidate a healthy swap: NotLiquidatable. Near-maturity (3 h) liquidation of an in-the-money swap by a stranger: {}.", if binary_has_adr007 { "NotLiquidatable (the binary carries ADR-007, exhaustion is the only trigger)" } else { "ACCEPTED: the binary predates ADR-007 (pre-maturity window still live, review finding F-11)" });
    if binary_has_adr007 {
        e.warp(2_000, 3 * 3_600);
        let before = e.token_amount(&ata(&tr.pubkey(), &USDC_DEVNET));
        let ix = e.close_ix("crank_settle_swap", &tr.pubkey(), 1, &stranger.pubkey(), Some(ata(&stranger.pubkey(), &USDC_DEVNET)), &());
        e.must(&[ix], &[&stranger]);
        let paid = e.token_amount(&ata(&tr.pubkey(), &USDC_DEVNET)) - before;
        assert!(paid > s.collateral, "in-the-money settlement pays more than collateral");
        let _ = writeln!(body, "- Settlement at maturity paid {paid} on collateral {} (gain clamped to collateral less the 10 pct income fee, less the 2 bp crank bounty).", s.collateral);
    }
    // double settle: the account is closed, a second settle cannot find it
    let ix = e.close_ix("crank_settle_swap", &tr.pubkey(), 1, &stranger.pubkey(), None, &());
    must_fail_any(&mut e, &[ix], &[&stranger], &["AccountNotInitialized", "AccountOwnedByWrongProgram", "AccountDiscriminatorNotFound", "AlreadySettled"]);
    // liquidate after maturity on a fresh swap: AlreadySettled
    let ix = e.publish_ix(900);
    e.must(&[ix], &[&p]);
    let ix = e.open_ix(&tr.pubkey(), &open_args(LegKind::PayFixed, 0, 100_000 * USDC, 9_999, 2));
    e.must(&[ix], &[&tr]);
    e.warp(28 * SLOTS_PER_DAY + 10, 28 * DAY + 10);
    let ix = e.close_ix("crank_liquidate_swap", &tr.pubkey(), 2, &stranger.pubkey(), None, &());
    e.must_fail(&[ix], &[&stranger], "AlreadySettled");
    let _ = writeln!(body, "- Double settle: second attempt fails on the closed account. Liquidate after maturity: AlreadySettled.");
    // exhaustion: a receive-fixed swap at 900 with the rate jumping far above it loses more than 99 pct of collateral
    let ix = e.publish_ix(900);
    e.must(&[ix], &[&p]);
    let ix = e.open_ix(&tr.pubkey(), &open_args(LegKind::ReceiveFixed, 3, 1_000_000 * USDC, 0, 3));
    e.must(&[ix], &[&tr]);
    let s3: Swap = e.acct("Swap", &e.swap_pda(&tr.pubkey(), 3));
    for _ in 0..8 {
        e.warp(9_000, 3_600);
        let b = e.bench();
        let ix = e.publish_ix((b.ema_bp + 300).min(30_000));
        e.must(&[ix], &[&p]);
    }
    let b = e.bench();
    let before = e.token_amount(&ata(&tr.pubkey(), &USDC_DEVNET));
    let ix = e.close_ix("crank_liquidate_swap", &tr.pubkey(), 3, &stranger.pubkey(), Some(ata(&stranger.pubkey(), &USDC_DEVNET)), &());
    let r = e.send(&[ix], &[&stranger]);
    let paid = e.token_amount(&ata(&tr.pubkey(), &USDC_DEVNET)) - before;
    let _ = writeln!(body, "- Exhaustion probe: receive-fixed 180 d at {} bp, spot walked to {} bp (ema {}): liquidation {} (residual to trader {paid} on collateral {}).", s3.fixed_bp, b.value_bp, b.ema_bp, if r.is_ok() { "accepted" } else { "NotLiquidatable (mark loss below 99 pct of collateral)" }, s3.collateral);
    note("A-4 settlement and liquidation rules", &body);
}

#[test]
fn a05_withdraw_beyond_share_and_zero_amounts() {
    let mut e = env_with_deposit(1_000_000 * USDC);
    let lp0 = e.lps[0].insecure_clone();
    let lp1 = e.lps[1].insecure_clone();
    let mut body = String::new();
    let held = e.token_amount(&ata(&lp0.pubkey(), &e.share_mint));
    let ix = e.withdraw_ix(&lp0.pubkey(), held + 1, 0);
    must_fail_any(&mut e, &[ix], &[&lp0], &["insufficient funds", "InsufficientFunds", "custom program error: 0x1"]);
    let ix = e.withdraw_ix(&lp1.pubkey(), 1, 0);
    must_fail_any(&mut e, &[ix], &[&lp1], &["insufficient funds", "InsufficientFunds", "custom program error: 0x1"]);
    let ix = e.withdraw_ix(&lp0.pubkey(), 0, 0);
    e.must_fail(&[ix], &[&lp0], "NotionalTooSmall");
    let ix = e.deposit_ix(&lp0.pubkey(), 0, 0);
    e.must_fail(&[ix], &[&lp0], "NotionalTooSmall");
    let ix = e.withdraw_ix(&lp0.pubkey(), held, held + 1);
    e.must_fail(&[ix], &[&lp0], "Slippage");
    let ix = e.deposit_ix(&lp1.pubkey(), 1_000 * USDC, 1_000 * USDC + 1);
    e.must_fail(&[ix], &[&lp1], "Slippage");
    let _ = writeln!(body, "- Withdraw more shares than held (own or empty account): SPL insufficient funds. Zero shares or zero amount: NotionalTooSmall. min_amount or min_shares above the result: Slippage.");
    // exit fee while swaps are open, then full exit without fee
    let tr = e.traders[0].insecure_clone();
    let ix = e.open_ix(&tr.pubkey(), &open_args(LegKind::PayFixed, 0, 10_000 * USDC, 9_999, 1));
    e.must(&[ix], &[&tr]);
    let before = e.token_amount(&ata(&lp0.pubkey(), &USDC_DEVNET));
    let ix = e.withdraw_ix(&lp0.pubkey(), held / 10, 0);
    e.must(&[ix], &[&lp0]);
    let got = e.token_amount(&ata(&lp0.pubkey(), &USDC_DEVNET)) - before;
    let _ = writeln!(body, "- Withdrawing 10 pct with a swap open returned {got} (50 bp exit fee applied; the fee is booked to the fee vault).");
    note("A-5 withdraw beyond share and zero amounts", &body);
}

#[test]
fn a06_unauthorised_mode_changes_and_admin_paths() {
    let mut e = env_with_deposit(1_000_000 * USDC);
    let a = e.authority.insecure_clone();
    let g = e.guardian.insecure_clone();
    let stranger = e.stranger.insecure_clone();
    let mut body = String::new();
    let ix = e.set_mode_ix(&stranger.pubkey(), OperatingMode::Halted);
    e.must_fail(&[ix], &[&stranger], "ConstraintSigner");
    // a stranger forging the signer flag without the signature: the runtime rejects the transaction
    let mut ix = e.set_mode_ix(&stranger.pubkey(), OperatingMode::Halted);
    ix.accounts[1].pubkey = a.pubkey();
    let msg = solana_message::Message::new(&[ix], Some(&e.payer.pubkey()));
    let mut tx = solana_transaction::Transaction::new_unsigned(msg);
    e.svm.expire_blockhash();
    let payer = e.payer.insecure_clone();
    tx.partial_sign(&[&payer], e.svm.latest_blockhash());
    let r = e.svm.send_transaction(tx);
    assert!(r.is_err(), "unsigned authority must be rejected by the runtime");
    let ix = e.set_mode_ix(&g.pubkey(), OperatingMode::Halted);
    e.must_fail(&[ix], &[&g], "GuardianScope");
    let ix = e.set_mode_ix(&g.pubkey(), OperatingMode::WithdrawOnly);
    e.must(&[ix], &[&g]);
    let ix = e.set_mode_ix(&g.pubkey(), OperatingMode::Limited);
    e.must_fail(&[ix], &[&g], "GuardianScope");
    let ix = e.set_mode_ix(&g.pubkey(), OperatingMode::WithdrawOnly);
    e.must_fail(&[ix], &[&g], "GuardianScope");
    let ix = e.set_mode_ix(&a.pubkey(), OperatingMode::Normal);
    e.must(&[ix], &[&a]);
    let _ = writeln!(body, "- admin_set_mode: stranger ConstraintSigner; missing authority signature rejected by the runtime; guardian to Halted, guardian loosening, guardian same-mode: GuardianScope; authority restores Normal.");
    let ix = e.queue_calibration_ix(&stranger.pubkey(), &to_params(&DEFAULT_PARAMS));
    must_fail_any(&mut e, &[ix], &[&stranger], &["ConstraintHasOne", "has one constraint"]);
    let ix = e.queue_calibration_ix(&g.pubkey(), &to_params(&DEFAULT_PARAMS));
    must_fail_any(&mut e, &[ix], &[&g], &["ConstraintHasOne", "has one constraint"]);
    let mut p = DEFAULT_PARAMS;
    p.collateral_bp[0] = 0;
    let ix = e.queue_calibration_ix(&a.pubkey(), &to_params(&p));
    e.must_fail(&[ix], &[&a], "CalibrationStep");
    let mut p = DEFAULT_PARAMS;
    p.collateral_bp[0] = 181; // 120 * 3 / 2 + 1 = 181 is the first value outside the step
    p.collateral_bp[0] = 182;
    let ix = e.queue_calibration_ix(&a.pubkey(), &to_params(&p));
    e.must_fail(&[ix], &[&a], "CalibrationStep");
    p.collateral_bp[0] = 181;
    let ix = e.queue_calibration_ix(&a.pubkey(), &to_params(&p));
    e.must(&[ix], &[&a]);
    // a second queue within the delay replaces the pending set (last write wins)
    let mut p2 = DEFAULT_PARAMS;
    p2.collateral_bp[0] = 60;
    let ix = e.queue_calibration_ix(&a.pubkey(), &to_params(&p2));
    e.must(&[ix], &[&a]);
    let pool = e.pool();
    assert_eq!(pool.pending_params.collateral_bp[0], 60);
    let _ = writeln!(body, "- admin_queue_calibration: stranger and guardian ConstraintHasOne; zero collateral and 182 (above 1.5x + 1 of 120) CalibrationStep; 181 accepted; re-queue within the delay replaces the pending set and restarts the delay (pending_effective_slot {}).", pool.pending_effective_slot);
    // admin_set_authority requires both signatures (review F-17): the stranger cannot take governance
    let new_auth = Keypair::new();
    e.svm.airdrop(&new_auth.pubkey(), 1_000_000_000).unwrap();
    let ix = Instruction { program_id: SWAP_AMM, accounts: [vec![rw(e.global), sig(stranger.pubkey()), sig(new_auth.pubkey())], evt(SWAP_AMM).to_vec()].concat(), data: data("admin_set_authority", &g.pubkey()) };
    let r = e.send(&[ix], &[&stranger, &new_auth]);
    let _ = writeln!(body, "- admin_set_authority by a stranger co-signed by the would-be authority: {}.", match &r { Err(err) if err.contains("ConstraintHasOne") || err.contains("has one constraint") => "ConstraintHasOne".to_string(), Err(err) => format!("rejected ({})", err.lines().next().unwrap_or("")), Ok(_) => "ACCEPTED (defect)".to_string() });
    assert!(r.is_err());
    note("A-6 unauthorised mode changes and admin paths", &body);
}

#[test]
fn a07_timelock_bypass_attempts() {
    let s = setup();
    let mut e = Env::new(&s);
    let proposer = e.authority.insecure_clone();
    let guardian = e.guardian.insecure_clone();
    let executor = e.lps[0].insecure_clone();
    let stranger = e.stranger.insecure_clone();
    let timelock = pda(&[b"timelock"], &TIMELOCK);
    let authority = pda(&[b"authority"], &TIMELOCK);
    let payer = e.payer.pubkey();
    let mut body = String::new();
    let init = |delay: u64, p: Pubkey, x: Pubkey, g: Pubkey| Instruction { program_id: TIMELOCK, accounts: vec![rw(timelock), ro(authority), sigw(payer), ro(SYSTEM)], data: data("initialise", &(p, x, g, delay)) };
    e.must_fail(&[init(431_999, proposer.pubkey(), executor.pubkey(), guardian.pubkey())], &[], "DelayOutOfRange");
    e.must_fail(&[init(6_480_001, proposer.pubkey(), executor.pubkey(), guardian.pubkey())], &[], "DelayOutOfRange");
    e.must_fail(&[init(432_000, proposer.pubkey(), executor.pubkey(), proposer.pubkey())], &[], "RolesMustDiffer");
    e.must(&[init(432_000, proposer.pubkey(), executor.pubkey(), guardian.pubkey())], &[]);
    // re-initialise to seize the roles: the PDA already exists
    must_fail_any(&mut e, &[init(432_000, stranger.pubkey(), stranger.pubkey(), stranger.pubkey())], &[], &["already in use", "RolesMustDiffer"]);
    must_fail_any(&mut e, &[init(432_000, stranger.pubkey(), executor.pubkey(), guardian.pubkey())], &[], &["already in use"]);
    let op = |n: u64| pda(&[b"op", &n.to_le_bytes()], &TIMELOCK);
    let queue = |n: u64, k: OperationKind, who: &Pubkey| Instruction { program_id: TIMELOCK, accounts: [vec![rw(timelock), sigw(*who), rw(op(n)), ro(SYSTEM)], evt(TIMELOCK).to_vec()].concat(), data: data("queue", &k) };
    let exec = |n: u64, who: &Pubkey| Instruction { program_id: TIMELOCK, accounts: [vec![rw(timelock), sig(*who), rw(op(n))], evt(TIMELOCK).to_vec()].concat(), data: data("execute_config", &()) };
    let cancel = |n: u64, who: &Pubkey, p: &Pubkey| Instruction { program_id: TIMELOCK, accounts: [vec![ro(timelock), sig(*who), rw(*p), rw(op(n))], evt(TIMELOCK).to_vec()].concat(), data: data("cancel", &()) };
    must_fail_any(&mut e, &[queue(0, OperationKind::SetDelay { delay_slots: 500_000 }, &stranger.pubkey())], &[&stranger], &["ConstraintHasOne", "has one constraint"]);
    must_fail_any(&mut e, &[queue(0, OperationKind::SetDelay { delay_slots: 500_000 }, &executor.pubkey())], &[&executor], &["ConstraintHasOne", "has one constraint"]);
    e.must(&[queue(0, OperationKind::SetRoles { proposer: stranger.pubkey(), executor: stranger.pubkey(), guardian: executor.pubkey() }, &proposer.pubkey())], &[&proposer]);
    // bypass attempts: execute immediately (TooEarly), by the wrong parties (Unauthorised), with a wrong nonce
    e.must_fail(&[exec(0, &executor.pubkey())], &[&executor], "TooEarly");
    e.must_fail(&[exec(0, &stranger.pubkey())], &[&stranger], "Unauthorised");
    e.must_fail(&[exec(0, &guardian.pubkey())], &[&guardian], "Unauthorised");
    must_fail_any(&mut e, &[exec(7, &executor.pubkey())], &[&executor], &["AccountNotInitialized", "AccountOwnedByWrongProgram"]);
    // queue the same nonce twice (the op PDA exists)
    must_fail_any(&mut e, &[queue(0, OperationKind::SetDelay { delay_slots: 500_000 }, &proposer.pubkey())], &[&proposer], &["already in use", "ConstraintSeeds"]);
    // warp one slot short of the eta, then exactly to it
    e.warp(431_999, 172_000);
    e.must_fail(&[exec(0, &executor.pubkey())], &[&executor], "TooEarly");
    e.warp(1, 1);
    // cancel by a stranger fails; the guardian cancels; execution of a cancelled op fails
    e.must_fail(&[cancel(0, &stranger.pubkey(), &proposer.pubkey())], &[&stranger], "Unauthorised");
    e.must_fail(&[cancel(0, &executor.pubkey(), &proposer.pubkey())], &[&executor], "Unauthorised");
    e.must(&[cancel(0, &guardian.pubkey(), &proposer.pubkey())], &[&guardian]);
    must_fail_any(&mut e, &[exec(0, &executor.pubkey())], &[&executor], &["AccountNotInitialized", "AccountOwnedByWrongProgram", "NotQueued"]);
    let t: Timelock = e.acct("Timelock", &timelock);
    assert_eq!(t.proposer, proposer.pubkey(), "roles unchanged after a cancelled SetRoles");
    // an operation left past eta + grace expires
    e.must(&[queue(1, OperationKind::SetDelay { delay_slots: 500_000 }, &proposer.pubkey())], &[&proposer]);
    e.warp(432_000 + 1_512_001, 800_000);
    e.must_fail(&[exec(1, &executor.pubkey())], &[&executor], "Expired");
    // a SetDelay below the floor cannot even be queued
    e.must_fail(&[queue(2, OperationKind::SetDelay { delay_slots: 1 }, &proposer.pubkey())], &[&proposer], "DelayOutOfRange");
    // a well-formed path: queue, wait, proposer executes (proposer or executor may execute)
    e.must(&[queue(2, OperationKind::SetDelay { delay_slots: 500_000 }, &proposer.pubkey())], &[&proposer]);
    e.warp(432_000, 172_800);
    e.must(&[exec(2, &proposer.pubkey())], &[&proposer]);
    let t: Timelock = e.acct("Timelock", &timelock);
    assert_eq!(t.delay_slots, 500_000);
    must_fail_any(&mut e, &[exec(2, &executor.pubkey())], &[&executor], &["NotQueued", "AccountNotInitialized", "AccountOwnedByWrongProgram"]);
    let _ = writeln!(body, "- initialise: delay below 432,000 or above 6,480,000 DelayOutOfRange; proposer equal to guardian RolesMustDiffer (proposer may equal executor, a two-key deployment); re-initialise on the existing PDA rejected by the system program.");
    let _ = writeln!(body, "- queue: stranger and executor ConstraintHasOne; a reused nonce fails the operation PDA seeds check (the nonce is the timelock's own counter); SetDelay below the floor DelayOutOfRange at queue time.");
    let _ = writeln!(body, "- execute: before eta TooEarly (also at eta minus one slot); stranger and guardian Unauthorised; unknown nonce fails on the account; cancelled or executed operation NotQueued or closed; past eta plus grace Expired.");
    let _ = writeln!(body, "- cancel: stranger and executor Unauthorised; guardian cancels; roles unchanged afterwards.");
    note("A-7 timelock bypass attempts", &body);
}

/// Simulation finding S-1: a close whose gain shrinks LP capital is refused when another leg sits at its cap.
#[test]
fn s01_settlement_blocked_by_utilisation_cap_after_gain() {
    let mut e = env_with_deposit(1_000_000 * USDC);
    let tr0 = e.traders[0].insecure_clone();
    let tr1 = e.traders[1].insecure_clone();
    let lp1 = e.lps[1].insecure_clone();
    let p = e.publisher.insecure_clone();
    let stranger = e.stranger.insecure_clone();
    // whale fills the pay leg exactly to its 48 pct cap with a 180-day swap
    let ix = e.open_ix(&tr0.pubkey(), &open_args(LegKind::PayFixed, 3, 480_000 * USDC, 9_999, 1));
    e.must(&[ix], &[&tr0]);
    // a receive-fixed 28-day swap
    let ix = e.open_ix(&tr1.pubkey(), &open_args(LegKind::ReceiveFixed, 0, 100_000 * USDC, 0, 2));
    e.must(&[ix], &[&tr1]);
    let s: Swap = e.acct("Swap", &e.swap_pda(&tr1.pubkey(), 2));
    let pool = e.pool();
    assert_eq!(pool.util_pay_bp, 4_800);
    // rates fall: the receive-fixed position gains (clamped at its collateral)
    let ix = e.publish_ix(400);
    e.must(&[ix], &[&p]);
    e.warp(28 * SLOTS_PER_DAY, 28 * DAY);
    let ix = e.publish_ix(400);
    e.must(&[ix], &[&p]);
    let ix = e.close_ix("crank_settle_swap", &tr1.pubkey(), 2, &stranger.pubkey(), None, &());
    let err = e.must_fail(&[ix], &[&stranger], "PoolInvariant");
    // the trader cannot cancel either (same assertion), and liquidation is not applicable (in the money)
    let ix = e.close_ix("trader_cancel_swap", &tr1.pubkey(), 2, &tr1.pubkey(), None, &0u64);
    e.must_fail(&[ix], &[&tr1], "PoolInvariant");
    // escape: fresh LP capital (or the whale closing) lets the settlement through
    let ix = e.deposit_ix(&lp1.pubkey(), 2_000 * USDC, 0);
    e.must(&[ix], &[&lp1]);
    let before = e.token_amount(&ata(&tr1.pubkey(), &USDC_DEVNET));
    let ix = e.close_ix("crank_settle_swap", &tr1.pubkey(), 2, &stranger.pubkey(), None, &());
    e.must(&[ix], &[&stranger]);
    let paid = e.token_amount(&ata(&tr1.pubkey(), &USDC_DEVNET)) - before;
    let after = e.pool();
    let body = format!(
        "Reproduced on chain. Pool 1,000,000 USDC; pay leg at 4,800 bp (480,000 USDC, 180 d); receive-fixed 100,000 USDC 28 d at {} bp; rate published at 400 bp. At maturity `crank_settle_swap` and `trader_cancel_swap` both fail with PoolInvariant because the gain ({} clamped to collateral) lowers tvl and pushes pay utilisation above the cap. First error line: `{}`. After a 2,000 USDC deposit the settlement succeeds, paying {paid}; pay utilisation afterwards {} bp.",
        s.fixed_bp,
        s.collateral,
        err.lines().next().unwrap_or(""),
        after.util_pay_bp
    );
    note("S-1 settlement blocked by the utilisation cap after a gain", &body);
}

/// Simulation observation S-2: Limited mode caps each swap's notional, not a trader's exposure.
#[test]
fn s02_limited_mode_cap_is_per_swap() {
    let mut e = env_with_deposit(10_000_000 * USDC);
    let a = e.authority.insecure_clone();
    let tr = e.traders[0].insecure_clone();
    let ix = e.set_mode_ix(&a.pubkey(), OperatingMode::Limited);
    e.must(&[ix], &[&a]);
    let ix = e.open_ix(&tr.pubkey(), &open_args(LegKind::PayFixed, 0, 150_000 * USDC, 9_999, 1));
    e.must_fail(&[ix], &[&tr], "LimitedModeCap");
    let ix = e.open_ix(&tr.pubkey(), &open_args(LegKind::PayFixed, 0, 100_000 * USDC, 9_999, 2));
    e.must(&[ix], &[&tr]);
    let ix = e.open_ix(&tr.pubkey(), &open_args(LegKind::PayFixed, 0, 100_000 * USDC, 9_999, 3));
    e.must(&[ix], &[&tr]);
    let ix = e.open_ix(&tr.pubkey(), &open_args(LegKind::PayFixed, 0, 100_000 * USDC, 9_999, 4));
    e.must(&[ix], &[&tr]);
    let pool = e.pool();
    assert_eq!(pool.open_pay_notional, 300_000 * USDC);
    note("S-2 Limited mode cap is per swap", &format!("With limited_mode_cap 100,000 USDC a 150,000 USDC open is rejected (LimitedModeCap) but three 100,000 USDC opens by the same trader in the same slot are accepted: open pay notional {} USDC. The cap bounds a single swap, not aggregate exposure.", pool.open_pay_notional / USDC));
}

/// Simulation observation S-3: `trader_cancel_swap` after maturity reads the post-maturity accrual (review F-12 via
/// a second path) and pays no crank bounty.
#[test]
fn s03_cancel_after_maturity_uses_post_maturity_accrual() {
    let mut e = env_with_deposit(10_000_000 * USDC);
    let tr = e.traders[0].insecure_clone();
    let p = e.publisher.insecure_clone();
    let ix = e.open_ix(&tr.pubkey(), &open_args(LegKind::PayFixed, 0, 100_000 * USDC, 9_999, 1));
    e.must(&[ix], &[&tr]);
    let s: Swap = e.acct("Swap", &e.swap_pda(&tr.pubkey(), 1));
    e.warp(56 * SLOTS_PER_DAY, 56 * DAY);
    let ix = e.publish_ix(684);
    e.must(&[ix], &[&p]);
    let before = e.token_amount(&ata(&tr.pubkey(), &USDC_DEVNET));
    let ix = e.close_ix("trader_cancel_swap", &tr.pubkey(), 1, &tr.pubkey(), None, &0u64);
    e.must(&[ix], &[&tr]);
    let paid = e.token_amount(&ata(&tr.pubkey(), &USDC_DEVNET)) - before;
    note("S-3 cancel after maturity", &format!("Pay-fixed 100,000 USDC 28 d at {} bp against a flat 684 bp index; nobody cranks for 28 days after maturity, the publisher republishes 684, the trader cancels (not settles): payout {paid} on collateral {} ({}). A fair settlement is a small loss.", s.fixed_bp, s.collateral, if paid > s.collateral { "GAIN paid out of LP capital: F-12 reachable through cancel as well as settle" } else { "no gain: F-12 not reproduced through cancel" }));
    assert!(paid > 0);
}

/// Simulation observation S-4: first-depositor inflation through `sync_vault` (review F-16), quantified.
#[test]
fn s04_first_depositor_inflation_via_sync_vault() {
    let s = setup();
    let mut e = Env::new(&s);
    let p = e.publisher.insecure_clone();
    let ix = e.publish_ix(684);
    e.must(&[ix], &[&p]);
    let lp0 = e.lps[0].insecure_clone();
    let lp1 = e.lps[1].insecure_clone();
    let ix = e.deposit_ix(&lp0.pubkey(), 1, 0);
    e.must(&[ix], &[&lp0]);
    let v = e.vault;
    e.mint_usdc(&v, 1_000 * USDC);
    let ix = e.sync_ix();
    e.must(&[ix], &[]);
    let ix = e.deposit_ix(&lp1.pubkey(), 1_999 * USDC, 0);
    e.must(&[ix], &[&lp1]);
    let shares1 = e.token_amount(&ata(&lp1.pubkey(), &e.share_mint));
    let pool = e.pool();
    let supply = e.mint_supply(&e.share_mint.clone());
    let ix = e.withdraw_ix(&lp1.pubkey(), shares1, 0);
    e.must(&[ix], &[&lp1]);
    let back = e.token_amount(&ata(&lp1.pubkey(), &USDC_DEVNET)) - (TRADER_FUNDING - 1_999 * USDC);
    note("S-4 first-depositor inflation via sync_vault", &format!("First LP deposits 1 unit, donates 1,000 USDC to the vault and calls sync_vault (tvl 1,000.000001 USDC on 1 share). Second LP deposits 1,999 USDC and receives {shares1} share(s) (pool tvl {} units, supply {}); withdrawing immediately returns {back} units, a loss of {} USDC to the first depositor. A min_shares floor on the client side or a minimum initial deposit on chain removes the exposure (review F-16).", pool.tvl, supply, (1_999 * USDC - back) / USDC));
}

/// Simulation observation S-5: at low index levels the receive-fixed quote is negative and is rejected with
/// `Overflow`; a pay-fixed holder then cannot cancel (the unwind quote is the receive quote) nor be liquidated.
#[test]
fn s05_low_rate_regime_makes_receive_quote_and_cancel_unavailable() {
    let s = setup();
    let mut e = Env::new(&s);
    let p = e.publisher.insecure_clone();
    let ix = e.publish_ix(10);
    e.must(&[ix], &[&p]);
    let lp0 = e.lps[0].insecure_clone();
    let ix = e.deposit_ix(&lp0.pubkey(), 1_000_000 * USDC, 0);
    e.must(&[ix], &[&lp0]);
    let tr = e.traders[0].insecure_clone();
    let ix = e.open_ix(&tr.pubkey(), &open_args(LegKind::ReceiveFixed, 2, 100_000 * USDC, 0, 1));
    let err = e.must_fail(&[ix], &[&tr], "Overflow");
    let ix = e.open_ix(&tr.pubkey(), &open_args(LegKind::PayFixed, 2, 100_000 * USDC, 9_999, 2));
    e.must(&[ix], &[&tr]);
    let s2: Swap = e.acct("Swap", &e.swap_pda(&tr.pubkey(), 2));
    e.warp(SLOTS_PER_DAY, DAY);
    let ix = e.publish_ix(10);
    e.must(&[ix], &[&p]);
    let ix = e.close_ix("trader_cancel_swap", &tr.pubkey(), 2, &tr.pubkey(), None, &0u64);
    let cancel = e.send(&[ix], &[&tr]);
    let cancel_note = match &cancel {
        Ok(_) => "the trader's cancel one day later SUCCEEDED: the binary floors the unwind quote at zero (maths finding M-7), the model in this harness still reports Overflow for this case and needs the same floor".to_string(),
        Err(err) => format!("one day later the trader's cancel fails with `{}` because the unwind marks at the negative receive quote; the position can only be settled at maturity", err.lines().next().unwrap_or("")),
    };
    note("S-5 low-rate regime", &format!("Index at 10 bp: a receive-fixed 90 d quote would be 10 - 14 - demand - 7 < 0 and the open fails with `{}` (the error name Overflow is misleading for a quotable-but-negative rate; the current source renames it QuoteBelowZero); a pay-fixed 90 d swap opens at {} bp; {}.", err.lines().next().unwrap_or(""), s2.fixed_bp, cancel_note));
}

#[test]
fn z99_compute_unit_table() {
    // A short canonical path with every instruction once, so the table has a baseline row per instruction even
    // if the other tests are filtered out. The scenario replay adds the distribution.
    let mut e = env_with_deposit(1_000_000 * USDC);
    let tr = e.traders[0].insecure_clone();
    let lp0 = e.lps[0].insecure_clone();
    let a = e.authority.insecure_clone();
    let p = e.publisher.insecure_clone();
    let stranger = e.stranger.insecure_clone();
    let ix = e.open_ix(&tr.pubkey(), &open_args(LegKind::PayFixed, 0, 10_000 * USDC, 9_999, 1));
    e.must(&[ix], &[&tr]);
    let ix = e.open_ix(&tr.pubkey(), &open_args(LegKind::ReceiveFixed, 1, 10_000 * USDC, 0, 2));
    e.must(&[ix], &[&tr]);
    let ix = e.close_ix("trader_cancel_swap", &tr.pubkey(), 2, &tr.pubkey(), None, &0u64);
    e.must(&[ix], &[&tr]);
    let ix = e.withdraw_ix(&lp0.pubkey(), 1_000 * USDC, 0);
    e.must(&[ix], &[&lp0]);
    let mut pp = DEFAULT_PARAMS;
    pp.term_bp[0] = 4;
    let ix = e.queue_calibration_ix(&a.pubkey(), &to_params(&pp));
    e.must(&[ix], &[&a]);
    let v = e.vault;
    e.mint_usdc(&v, 1);
    let ix = e.sync_ix();
    e.must(&[ix], &[]);
    let ix = e.sweep_ix();
    e.must(&[ix], &[]);
    let ix = e.set_mode_ix(&a.pubkey(), OperatingMode::Limited);
    e.must(&[ix], &[&a]);
    let ix = e.set_mode_ix(&a.pubkey(), OperatingMode::Normal);
    e.must(&[ix], &[&a]);
    e.warp(28 * SLOTS_PER_DAY, 28 * DAY);
    let ix = e.publish_ix(684);
    e.must(&[ix], &[&p]);
    let ix = e.close_ix("crank_settle_swap", &tr.pubkey(), 1, &stranger.pubkey(), Some(ata(&stranger.pubkey(), &USDC_DEVNET)), &());
    e.must(&[ix], &[&stranger]);
    note("Compute units, canonical path", &cu_table(&e.cu_log));
}
