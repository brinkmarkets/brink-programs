//! Devnet bootstrap rehearsal: runs `deploy/init-devnet.mjs --plan` to obtain the exact transactions the script
//! would send, replays them against LiteSVM with the deployer as the upgrade authority of all three programs,
//! and checks the resulting roles, layouts and counts. Nothing touches devnet until this passes.
//! Run from `programs/`: `./build-sbf.sh && (cd tests/svm && cargo test --test bootstrap)`. Needs `node` with
//! `@solana/web3.js` resolvable from `programs/deploy` (the repository root `node_modules`).
use borsh::BorshDeserialize;
use brink_svm_tests::*;
use litesvm::LiteSVM;
use solana_account::Account;
use solana_clock::Clock;
use solana_instruction::{AccountMeta, Instruction};
use solana_keypair::Keypair;
use solana_message::Message;
use solana_program_pack::Pack;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use solana_transaction::Transaction;
use std::str::FromStr;

const LOADER: Pubkey = Pubkey::from_str_const("BPFLoaderUpgradeab1e11111111111111111111111");
const MAX_PUBLISHERS: usize = 5;

// Local mirrors of the version 2 layouts (ADR-010, patch 0003); the shared mirror in `src/lib.rs` follows.
// Every field is decoded so the layout is checked in full; only some are asserted on.
#[derive(BorshDeserialize, Debug)]
#[allow(dead_code)]
struct RegistryV2 {
    version: u8,
    authority: Pubkey,
    guardian: Pubkey,
    publishers: [Pubkey; MAX_PUBLISHERS],
    quorum: u8,
    single_publisher: bool,
    count: u32,
    bump: u8,
    _reserved: [u8; 64],
}
#[derive(BorshDeserialize, Debug, Clone, Copy)]
#[allow(dead_code)]
struct Observation {
    publisher: Pubkey,
    value_bp: u16,
    slot: u64,
}
#[derive(BorshDeserialize, Debug)]
#[allow(dead_code)]
struct BenchmarkV2 {
    version: u8,
    registry: Pubkey,
    id: [u8; 16],
    source: Pubkey,
    value_bp: u16,
    ema_bp: u16,
    slot: u64,
    unix_ts: i64,
    accrual_e18: u128,
    max_staleness_slots: u64,
    band_bp: u16,
    half_life_slots: u64,
    min_interval_slots: u64,
    published: bool,
    publish_count: u64,
    bump: u8,
    prev_value_bp: u16,
    prev_unix_ts: i64,
    max_drift_bp: u16,
    drift_window_slots: u64,
    drift_anchor_bp: u16,
    drift_anchor_slot: u64,
    clamped: bool,
    disputed: bool,
    support: u8,
    observations: [Observation; MAX_PUBLISHERS],
    _reserved: [u8; 32],
}

fn deploy_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/deploy")
}
fn programs_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// LiteSVM loads programs with no upgrade authority; the F-14 check needs the deployer there. The ProgramData
/// header is bincode: u32 variant (3) || u64 slot || Option<Pubkey> (u8 tag || 32 bytes).
fn set_upgrade_authority(svm: &mut LiteSVM, program: &Pubkey, authority: Option<&Pubkey>) {
    let pd = Pubkey::find_program_address(&[program.as_ref()], &LOADER).0;
    let mut acct = svm.get_account(&pd).expect("programdata account");
    assert_eq!(
        u32::from_le_bytes(acct.data[0..4].try_into().unwrap()),
        3,
        "ProgramData variant"
    );
    match authority {
        Some(a) => {
            acct.data[12] = 1;
            acct.data[13..45].copy_from_slice(a.as_ref());
        }
        None => {
            acct.data[12] = 0;
            acct.data[13..45].fill(0);
        }
    }
    svm.set_account(pd, acct).unwrap();
}

fn fresh_svm(deployer: &Pubkey) -> LiteSVM {
    let mut svm = LiteSVM::new();
    let d = deploy_dir();
    svm.add_program_from_file(SWAP_AMM, d.join("swap_amm.so"))
        .expect("swap_amm.so: run ./build-sbf.sh first");
    svm.add_program_from_file(INDEX, d.join("brink_index.so"))
        .unwrap();
    svm.add_program_from_file(TIMELOCK, d.join("brink_timelock.so"))
        .unwrap();
    for p in [SWAP_AMM, INDEX, TIMELOCK] {
        set_upgrade_authority(&mut svm, &p, Some(deployer));
    }
    svm.airdrop(deployer, 500_000_000_000).unwrap();
    // USDC at the devnet address.
    let mut mint_data = vec![0u8; spl_token_interface::state::Mint::LEN];
    spl_token_interface::state::Mint {
        mint_authority: Some(Pubkey::new_unique()).into(),
        supply: 0,
        decimals: 6,
        is_initialized: true,
        freeze_authority: None.into(),
    }
    .pack_into_slice(&mut mint_data);
    svm.set_account(
        USDC_DEVNET,
        Account {
            lamports: 10_000_000_000,
            data: mint_data,
            owner: TOKEN_PROGRAM,
            executable: false,
            rent_epoch: 0,
        },
    )
    .unwrap();
    let mut c = svm.get_sysvar::<Clock>();
    c.slot = 1_000;
    c.unix_timestamp = 1_790_000_000;
    svm.set_sysvar(&c);
    svm.warp_to_slot(1_000);
    let mut c2 = svm.get_sysvar::<Clock>();
    c2.unix_timestamp = 1_790_000_000;
    svm.set_sysvar(&c2);
    svm
}

#[derive(serde::Deserialize)]
struct Plan {
    transactions: Vec<PlanTx>,
    expectations: Vec<Expectation>,
    quorum: u8,
    benchmarks: usize,
}
#[derive(serde::Deserialize)]
struct PlanTx {
    label: String,
    creates: Option<String>,
    instructions: Vec<PlanIx>,
}
#[derive(serde::Deserialize)]
struct PlanIx {
    program: String,
    keys: Vec<PlanKey>,
    data: String,
}
#[derive(serde::Deserialize)]
struct PlanKey {
    pubkey: String,
    signer: bool,
    writable: bool,
}
#[derive(serde::Deserialize)]
struct Expectation {
    label: String,
    account: String,
    fields: Vec<ExpectedField>,
}
#[derive(serde::Deserialize)]
struct ExpectedField {
    name: String,
    offset: usize,
    key: String,
}

fn plan(deployer: &Pubkey, guardian: &Pubkey, publishers: &[Pubkey], quorum: Option<u8>) -> Plan {
    let out = std::env::temp_dir().join(format!("brink-init-plan-{}.json", deployer));
    let mut cmd = std::process::Command::new("node");
    cmd.current_dir(programs_dir())
        .arg("deploy/init-devnet.mjs")
        .arg("--plan")
        .arg(&out)
        .env("DEPLOYER_PUBKEY", deployer.to_string())
        .env("GUARDIAN_PUBKEY", guardian.to_string())
        .env("SWAP_PROGRAM_ID", SWAP_AMM.to_string())
        .env("INDEX_PROGRAM_ID", INDEX.to_string())
        .env("TIMELOCK_PROGRAM_ID", TIMELOCK.to_string());
    if !publishers.is_empty() {
        let list: Vec<String> = publishers.iter().map(ToString::to_string).collect();
        cmd.env("PUBLISHER_PUBKEYS", list.join(","));
    }
    if let Some(q) = quorum {
        cmd.env("PUBLISHER_QUORUM", q.to_string());
    }
    let status = cmd
        .status()
        .expect("node is required for the bootstrap rehearsal");
    assert!(status.success(), "init-devnet.mjs --plan failed");
    let text = std::fs::read_to_string(&out).unwrap();
    let _ = std::fs::remove_file(&out);
    serde_json::from_str(&text).unwrap()
}

fn to_ix(p: &PlanIx) -> Instruction {
    Instruction {
        program_id: Pubkey::from_str(&p.program).unwrap(),
        accounts: p
            .keys
            .iter()
            .map(|k| AccountMeta {
                pubkey: Pubkey::from_str(&k.pubkey).unwrap(),
                is_signer: k.signer,
                is_writable: k.writable,
            })
            .collect(),
        data: hex::decode(&p.data).unwrap(),
    }
}

fn send(svm: &mut LiteSVM, ixs: &[Instruction], signer: &Keypair) -> Result<Vec<String>, String> {
    let msg = Message::new(ixs, Some(&signer.pubkey()));
    let tx = Transaction::new(&[signer], msg, svm.latest_blockhash());
    match svm.send_transaction(tx) {
        Ok(meta) => Ok(meta.logs),
        Err(e) => Err(format!("{:?}\n{}", e.err, e.meta.logs.join("\n"))),
    }
}

fn replay(svm: &mut LiteSVM, plan: &Plan, deployer: &Keypair) {
    for t in &plan.transactions {
        let ixs: Vec<Instruction> = t.instructions.iter().map(to_ix).collect();
        send(svm, &ixs, deployer).unwrap_or_else(|e| panic!("{}: {e}", t.label));
        if let Some(created) = &t.creates {
            let k = Pubkey::from_str(created).unwrap();
            assert!(
                svm.get_account(&k)
                    .map(|a| !a.data.is_empty())
                    .unwrap_or(false),
                "{}: {created} was not created",
                t.label
            );
        }
        svm.expire_blockhash();
    }
    for e in &plan.expectations {
        let acct = svm
            .get_account(&Pubkey::from_str(&e.account).unwrap())
            .expect(&e.label);
        for f in &e.fields {
            let got = Pubkey::try_from(&acct.data[f.offset..f.offset + 32]).unwrap();
            assert_eq!(
                got.to_string(),
                f.key,
                "{}.{} read-back offset {}",
                e.label,
                f.name,
                f.offset
            );
        }
    }
}

/// The devnet default: deployer as the only publisher, quorum 1, flagged in the registry.
#[test]
fn devnet_bootstrap_runs_end_to_end_on_litesvm() {
    let deployer = Keypair::new();
    let guardian = Keypair::new();
    let plan = plan(&deployer.pubkey(), &guardian.pubkey(), &[], None);
    assert_eq!(plan.quorum, 1);
    let mut svm = fresh_svm(&deployer.pubkey());
    replay(&mut svm, &plan, &deployer);

    let timelock: Timelock = decode(
        "Timelock",
        &svm.get_account(&pda(&[b"timelock"], &TIMELOCK))
            .unwrap()
            .data,
    );
    assert_eq!(timelock.proposer, deployer.pubkey());
    assert_eq!(timelock.executor, deployer.pubkey());
    assert_eq!(timelock.guardian, guardian.pubkey());
    assert_eq!(timelock.delay_slots, 432_000);

    let registry: RegistryV2 = decode(
        "Registry",
        &svm.get_account(&pda(&[b"registry"], &INDEX)).unwrap().data,
    );
    assert_eq!(registry.version, 3);
    assert_eq!(registry.authority, deployer.pubkey());
    assert_eq!(registry.guardian, guardian.pubkey());
    assert_eq!(registry.publishers[0], deployer.pubkey());
    assert_eq!(registry.quorum, 1);
    assert!(
        registry.single_publisher,
        "a quorum of one is recorded in state"
    );
    assert_eq!(registry.count as usize, plan.benchmarks);

    let global: Global = decode(
        "Global",
        &svm.get_account(&pda(&[b"global"], &SWAP_AMM)).unwrap().data,
    );
    assert_eq!(global.authority, deployer.pubkey());
    assert_eq!(global.guardian, guardian.pubkey());
    assert_eq!(global.pool_count as usize, plan.benchmarks);

    // Every benchmark published once by the single publisher; every pool has a 12-decimal share mint.
    let mut seen = 0;
    for t in &plan.transactions {
        let Some(created) = &t.creates else { continue };
        if !t.label.starts_with("benchmark ") {
            continue;
        }
        let bench = Pubkey::from_str(created).unwrap();
        let b: BenchmarkV2 = decode("Benchmark", &svm.get_account(&bench).unwrap().data);
        assert_eq!(b.version, 3);
        assert!(b.published);
        assert_eq!(b.support, 1, "single publisher visible to readers");
        assert!(!b.clamped && !b.disputed);
        assert_eq!(b.max_drift_bp, 400);
        assert_eq!(b.drift_window_slots, 216_000);
        assert_eq!(b.observations[0].publisher, deployer.pubkey());
        assert_eq!(b.observations[0].value_bp, b.value_bp);
        let pool_key = pda(&[b"pool", bench.as_ref()], &SWAP_AMM);
        let pool: Pool = decode("Pool", &svm.get_account(&pool_key).unwrap().data);
        assert_eq!(pool.benchmark, bench);
        let mint = spl_token_interface::state::Mint::unpack(
            &svm.get_account(&pool.share_mint).unwrap().data,
        )
        .unwrap();
        assert_eq!(mint.decimals, 9, "share mint decimals (E1 0004)");
        assert_eq!(mint.supply, 0);
        seen += 1;
    }
    assert_eq!(seen, plan.benchmarks);
}

/// Three publishers with a quorum of two: the deployer's first observation alone moves nothing.
#[test]
fn bootstrap_with_a_quorum_leaves_benchmarks_unpublished_until_a_second_observation() {
    let deployer = Keypair::new();
    let guardian = Keypair::new();
    let p2 = Keypair::new();
    let p3 = Keypair::new();
    let plan = plan(
        &deployer.pubkey(),
        &guardian.pubkey(),
        &[deployer.pubkey(), p2.pubkey(), p3.pubkey()],
        None,
    );
    assert_eq!(plan.quorum, 2);
    let mut svm = fresh_svm(&deployer.pubkey());
    replay(&mut svm, &plan, &deployer);
    let registry: RegistryV2 = decode(
        "Registry",
        &svm.get_account(&pda(&[b"registry"], &INDEX)).unwrap().data,
    );
    assert!(!registry.single_publisher);
    assert_eq!(registry.quorum, 2);
    let first = plan
        .transactions
        .iter()
        .find(|t| t.label.starts_with("benchmark "))
        .unwrap();
    let bench = Pubkey::from_str(first.creates.as_ref().unwrap()).unwrap();
    let b: BenchmarkV2 = decode("Benchmark", &svm.get_account(&bench).unwrap().data);
    assert!(!b.published, "one observation is below the quorum");
    assert_eq!(b.observations[0].publisher, deployer.pubkey());
    // The second publisher agrees: the median is accepted with support 2.
    svm.airdrop(&p2.pubkey(), 1_000_000_000).unwrap();
    let registry_key = pda(&[b"registry"], &INDEX);
    let ix = Instruction {
        program_id: INDEX,
        accounts: vec![
            AccountMeta::new_readonly(registry_key, false),
            AccountMeta::new_readonly(p2.pubkey(), true),
            AccountMeta::new(bench, false),
            AccountMeta::new_readonly(event_authority(&INDEX), false),
            AccountMeta::new_readonly(INDEX, false),
        ],
        data: data("publish", &690u16),
    };
    send(&mut svm, &[ix], &p2).unwrap();
    let b: BenchmarkV2 = decode("Benchmark", &svm.get_account(&bench).unwrap().data);
    assert!(b.published);
    assert_eq!(b.support, 2);
    // The agreeing cluster's lower median, a value a publisher actually observed; the former even-count midpoint
    // let two observations two bands apart count as agreeing (external scan 1, M-6; internal F-42).
    assert_eq!(b.value_bp, 684, "lower median of the agreeing pair");
}

/// F-14: with the upgrade authority elsewhere, the first transaction of the plan is refused.
#[test]
fn bootstrap_is_refused_when_the_payer_is_not_the_upgrade_authority() {
    let deployer = Keypair::new();
    let guardian = Keypair::new();
    let plan = plan(&deployer.pubkey(), &guardian.pubkey(), &[], None);
    let mut svm = fresh_svm(&deployer.pubkey());
    let someone_else = Pubkey::new_unique();
    for p in [SWAP_AMM, INDEX, TIMELOCK] {
        set_upgrade_authority(&mut svm, &p, Some(&someone_else));
    }
    let first = &plan.transactions[0];
    let ixs: Vec<Instruction> = first.instructions.iter().map(to_ix).collect();
    let err = send(&mut svm, &ixs, &deployer).expect_err("timelock.initialise must fail");
    assert!(err.contains("NotUpgradeAuthority"), "{err}");
    // And with no upgrade authority at all (immutable program) nothing can be initialised either. The clock
    // moves first so the retry is a distinct transaction rather than a duplicate of the refused one.
    for p in [SWAP_AMM, INDEX, TIMELOCK] {
        set_upgrade_authority(&mut svm, &p, None);
    }
    svm.expire_blockhash();
    let err = send(&mut svm, &ixs, &deployer).expect_err("timelock.initialise must fail");
    assert!(err.contains("NotUpgradeAuthority"), "{err}");
}
