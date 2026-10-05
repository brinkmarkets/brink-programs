//! Slow layer support: an Anchor-compatible client (discriminators, PDAs, mirrored layouts), a LiteSVM
//! environment with several LPs and traders, instruction builders for every action of the shared scenario
//! generator, and a compute-unit recorder writing to `results/compute-units.tsv`.
//!
//! The mirrored layouts are copied from the in-tree SVM tests; field order is the on-chain contract.
#![allow(clippy::too_many_arguments)]

use borsh::{BorshDeserialize, BorshSerialize};
use litesvm::LiteSVM;
use sha2::{Digest, Sha256};
use solana_account::Account;
use solana_clock::Clock;
use solana_instruction::{AccountMeta, Instruction};
use solana_keypair::Keypair;
use solana_message::Message;
use solana_program_pack::Pack;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use solana_transaction::Transaction;

use brink_sim_core::model::{Actor, Mode, Setup};
use brink_sim_core::scenario::Action;

pub const SWAP_AMM: Pubkey = Pubkey::from_str_const("EbD862QySygYKCMd8RHM2JaHqnduLQXB8Y632U3pQtce");
pub const INDEX: Pubkey = Pubkey::from_str_const("J2udZ8xzSsETsrSnbW3DeLFBTVooKRup3P7LWKSRwuvS");
pub const TIMELOCK: Pubkey = Pubkey::from_str_const("CeQz4x7Ad7Hg715Tn4PHtvtAxYin2PHv4KGaD3KS3kM5");
pub const USDC_DEVNET: Pubkey = Pubkey::from_str_const("4zMMC9srt5Ri5X14GAgXhaHii3GnPAEERYPJgZJDncDU");
pub const TOKEN_PROGRAM: Pubkey = Pubkey::from_str_const("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
pub const ATA_PROGRAM: Pubkey = Pubkey::from_str_const("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
pub const SYSTEM: Pubkey = Pubkey::from_str_const("11111111111111111111111111111111");
/// Re-entrancy probe program (built from `../hook`); any id distinct from the three programs works.
pub const HOOK: Pubkey = Pubkey::new_from_array([7u8; 32]);

pub const USDC: u64 = 1_000_000;
pub const BENCH_ID: [u8; 16] = *b"sim-usdc\0\0\0\0\0\0\0\0";

pub fn ix_disc(name: &str) -> [u8; 8] {
    let h = Sha256::digest(format!("global:{name}").as_bytes());
    h[..8].try_into().unwrap()
}
pub fn acc_disc(name: &str) -> [u8; 8] {
    let h = Sha256::digest(format!("account:{name}").as_bytes());
    h[..8].try_into().unwrap()
}
pub fn data<T: BorshSerialize>(name: &str, args: &T) -> Vec<u8> {
    let mut d = ix_disc(name).to_vec();
    args.serialize(&mut d).unwrap();
    d
}
pub fn pda(seeds: &[&[u8]], program: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(seeds, program).0
}
pub fn event_authority(program: &Pubkey) -> Pubkey {
    pda(&[b"__event_authority"], program)
}
pub fn ata(owner: &Pubkey, mint: &Pubkey) -> Pubkey {
    pda(&[owner.as_ref(), TOKEN_PROGRAM.as_ref(), mint.as_ref()], &ATA_PROGRAM)
}
pub fn decode<T: BorshDeserialize>(name: &str, bytes: &[u8]) -> T {
    assert_eq!(&bytes[..8], &acc_disc(name), "discriminator mismatch for {name}");
    T::try_from_slice(&bytes[8..]).or_else(|_| T::deserialize(&mut &bytes[8..])).unwrap()
}

// ---- mirrored layouts ----
#[derive(BorshSerialize, BorshDeserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum OperatingMode {
    Normal,
    Limited,
    WithdrawOnly,
    Halted,
}
#[derive(BorshSerialize, BorshDeserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Params {
    pub model_pay_bp: [u16; 4],
    pub model_rec_bp: [u16; 4],
    pub term_bp: [u16; 4],
    pub demand_k_bp: u16,
    pub demand_cap_bp: u16,
    pub collateral_bp: [u16; 4],
}
#[derive(BorshSerialize, BorshDeserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HookFlags {
    pub before_open: bool,
    pub after_open: bool,
    pub before_cancel: bool,
    pub after_cancel: bool,
    pub before_deposit: bool,
    pub after_deposit: bool,
    pub before_withdraw: bool,
    pub after_settle: bool,
}
#[derive(BorshSerialize, BorshDeserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pricer {
    Vernier,
    External { program: Pubkey },
}
#[derive(BorshSerialize, BorshDeserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum LegKind {
    PayFixed,
    ReceiveFixed,
}
#[derive(BorshSerialize, BorshDeserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum SwapState {
    Open,
    Settled { pnl: i64 },
    Liquidated { pnl: i64 },
    Cancelled { pnl: i64 },
}
#[derive(BorshSerialize, BorshDeserialize, Debug)]
pub struct Global {
    pub authority: Pubkey,
    pub guardian: Pubkey,
    pub usdc_mint: Pubkey,
    pub token_program: Pubkey,
    pub treasury: Pubkey,
    pub buyback_escrow: Pubkey,
    pub fee_vault: Pubkey,
    pub buyback_accrued: u64,
    pub treasury_accrued: u64,
    pub buyback_lifetime: u64,
    pub treasury_lifetime: u64,
    pub mode: OperatingMode,
    pub param_delay_slots: u64,
    pub limited_mode_cap: u64,
    pub pool_count: u32,
    pub bump: u8,
    pub _reserved: [u8; 128],
}
#[derive(BorshSerialize, BorshDeserialize, Debug)]
pub struct Pool {
    pub benchmark: Pubkey,
    pub share_mint: Pubkey,
    pub vault: Pubkey,
    pub hook_program: Pubkey,
    pub hooks: HookFlags,
    pub pricer: Pricer,
    pub params: Params,
    pub pending_params: Params,
    pub pending_effective_slot: u64,
    pub tvl: u64,
    pub collateral_held: u64,
    pub util_pay_bp: u16,
    pub util_rec_bp: u16,
    pub open_pay_notional: u64,
    pub open_rec_notional: u64,
    pub open_swaps: u32,
    pub fees_lifetime: u64,
    pub event_seq: u64,
    pub min_notional: u64,
    pub max_notional: u64,
    pub bump: u8,
    pub _reserved: [u8; 128],
}
#[derive(BorshSerialize, BorshDeserialize, Debug)]
pub struct Swap {
    pub pool: Pubkey,
    pub trader: Pubkey,
    pub leg: LegKind,
    pub tenor: u8,
    pub notional: u64,
    pub fixed_bp: u16,
    pub collateral: u64,
    pub opened_slot: u64,
    pub opened_ts: i64,
    pub matures_ts: i64,
    pub index_accrual_start: u128,
    pub client_seed: u64,
    pub state: SwapState,
    pub bump: u8,
    pub _reserved: [u8; 64],
}
#[derive(BorshSerialize, BorshDeserialize, Debug)]
pub struct Benchmark {
    pub registry: Pubkey,
    pub id: [u8; 16],
    pub source: Pubkey,
    pub value_bp: u16,
    pub ema_bp: u16,
    pub slot: u64,
    pub unix_ts: i64,
    pub accrual_e18: u128,
    pub max_staleness_slots: u64,
    pub band_bp: u16,
    pub half_life_slots: u64,
    pub min_interval_slots: u64,
    pub published: bool,
    pub publish_count: u64,
    pub bump: u8,
    pub _reserved: [u8; 64],
}
#[derive(BorshSerialize, BorshDeserialize, Debug)]
pub struct Timelock {
    pub proposer: Pubkey,
    pub executor: Pubkey,
    pub guardian: Pubkey,
    pub delay_slots: u64,
    pub nonce: u64,
    pub bump: u8,
    pub authority_bump: u8,
    pub _reserved: [u8; 64],
}
#[derive(BorshSerialize, BorshDeserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum OperationKind {
    Upgrade { program: Pubkey, buffer: Pubkey, buffer_hash: [u8; 32] },
    SetUpgradeAuthority { program: Pubkey, new_authority: Pubkey },
    SetDelay { delay_slots: u64 },
    SetRoles { proposer: Pubkey, executor: Pubkey, guardian: Pubkey },
}
#[derive(BorshSerialize, BorshDeserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum OperationState {
    Queued,
    Executed,
    Cancelled,
}
#[derive(BorshSerialize, BorshDeserialize, Debug)]
pub struct Operation {
    pub timelock: Pubkey,
    pub nonce: u64,
    pub kind: OperationKind,
    pub queued_slot: u64,
    pub eta_slot: u64,
    pub state: OperationState,
    pub bump: u8,
}

#[derive(BorshSerialize)]
pub struct InitialiseGlobalArgs {
    pub guardian: Pubkey,
    pub param_delay_slots: u64,
    pub limited_mode_cap: u64,
}
#[derive(BorshSerialize)]
pub struct CreatePoolArgs {
    pub params: Params,
    pub hook_program: Option<Pubkey>,
    pub hooks: HookFlags,
    pub pricer: Pricer,
    pub min_notional: u64,
    pub max_notional: u64,
}
#[derive(BorshSerialize)]
pub struct OpenSwapArgs {
    pub leg: LegKind,
    pub tenor: u8,
    pub notional: u64,
    pub limit_rate_bp: u16,
    pub client_seed: u64,
}
#[derive(BorshSerialize)]
pub struct CreateBenchmarkArgs {
    pub id: [u8; 16],
    pub source: Pubkey,
    pub band_bp: u16,
    pub max_staleness_slots: u64,
    pub half_life_slots: u64,
    pub min_interval_slots: u64,
}

pub fn to_params(p: &vernier::Params) -> Params {
    Params { model_pay_bp: p.model_pay_bp, model_rec_bp: p.model_rec_bp, term_bp: p.term_bp, demand_k_bp: p.demand_k_bp, demand_cap_bp: p.demand_cap_bp, collateral_bp: p.collateral_bp }
}
pub fn to_mode(m: Mode) -> OperatingMode {
    match m {
        Mode::Normal => OperatingMode::Normal,
        Mode::Limited => OperatingMode::Limited,
        Mode::WithdrawOnly => OperatingMode::WithdrawOnly,
        Mode::Halted => OperatingMode::Halted,
    }
}
pub fn to_leg(l: vernier::Leg) -> LegKind {
    match l {
        vernier::Leg::Pay => LegKind::PayFixed,
        vernier::Leg::Receive => LegKind::ReceiveFixed,
    }
}

pub const IX_NAMES: &[&str] = &[
    "admin_initialise_global", "admin_set_mode", "admin_set_authority", "admin_queue_calibration", "admin_create_pool",
    "lp_deposit", "lp_withdraw", "trader_open_swap", "trader_cancel_swap", "crank_settle_swap", "crank_liquidate_swap",
    "sync_vault", "sweep_fees",
    "initialise", "create_benchmark", "publish", "set_guards", "set_publisher", "set_authority",
    "queue", "cancel", "execute_upgrade", "execute_set_upgrade_authority", "execute_config",
];
pub fn ix_name(program: &Pubkey, data: &[u8]) -> Option<String> {
    if data.len() < 8 {
        return None;
    }
    let prefix = if *program == SWAP_AMM {
        "swap_amm"
    } else if *program == INDEX {
        "brink_index"
    } else if *program == TIMELOCK {
        "brink_timelock"
    } else {
        return None;
    };
    IX_NAMES.iter().find(|n| ix_disc(n) == data[..8]).map(|n| format!("{prefix}::{n}"))
}

pub fn results_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../results")
}
/// Appends one compute-unit observation (`program::instruction<TAB>outcome<TAB>cu`) to `results/compute-units.tsv`.
pub fn record_cu(name: &str, ok: bool, cu: u64) {
    use std::io::Write;
    let dir = results_dir();
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("compute-units.tsv")) {
        let _ = writeln!(f, "{name}\t{}\t{cu}", if ok { "ok" } else { "err" });
    }
}

pub fn deploy_dir() -> std::path::PathBuf {
    if let Ok(d) = std::env::var("BRINK_SIM_SO_DIR") {
        return std::path::PathBuf::from(d);
    }
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../target/deploy")
}

pub fn ro(k: Pubkey) -> AccountMeta {
    AccountMeta::new_readonly(k, false)
}
pub fn rw(k: Pubkey) -> AccountMeta {
    AccountMeta::new(k, false)
}
pub fn sig(k: Pubkey) -> AccountMeta {
    AccountMeta::new_readonly(k, true)
}
pub fn sigw(k: Pubkey) -> AccountMeta {
    AccountMeta::new(k, true)
}
pub fn evt(program: Pubkey) -> [AccountMeta; 2] {
    [ro(event_authority(&program)), ro(program)]
}
pub fn buyback_owner(treasury_owner: &Pubkey) -> Pubkey {
    pda(&[b"test-buyback", treasury_owner.as_ref()], &SYSTEM)
}

/// LiteSVM environment with the three programs, the USDC mint, a benchmark, the global, one pool, `n_lps` LPs
/// and `n_traders` traders funded with effectively unlimited USDC.
pub struct Env {
    pub svm: LiteSVM,
    pub payer: Keypair,
    pub authority: Keypair,
    pub guardian: Keypair,
    pub publisher: Keypair,
    pub treasury_owner: Keypair,
    pub mint_authority: Keypair,
    pub stranger: Keypair,
    pub lps: Vec<Keypair>,
    pub traders: Vec<Keypair>,
    pub registry: Pubkey,
    pub benchmark: Pubkey,
    pub global: Pubkey,
    pub fee_vault: Pubkey,
    pub pool: Pubkey,
    pub share_mint: Pubkey,
    pub vault: Pubkey,
    pub hook_loaded: bool,
    pub cu_log: Vec<(String, bool, u64)>,
}

/// Per-trader funding. Large enough that no scenario ever runs out; small enough that the mint supply stays in u64.
pub const TRADER_FUNDING: u64 = 1_000_000_000_000_000_000; // 1e18 units = 1e12 USDC

impl Env {
    pub fn new(setup: &Setup) -> Self {
        Self::new_with(setup, None)
    }

    /// `hook`: `Some(flags)` loads the re-entrancy probe program from `../hook/deploy/brink_sim_hook.so` and
    /// creates the pool with those hook points enabled.
    pub fn new_with(setup: &Setup, hook: Option<HookFlags>) -> Self {
        let mut svm = LiteSVM::new();
        let d = deploy_dir();
        svm.add_program_from_file(SWAP_AMM, d.join("swap_amm.so")).expect("swap_amm.so missing: run programs/build-sbf.sh");
        svm.add_program_from_file(INDEX, d.join("brink_index.so")).expect("brink_index.so");
        svm.add_program_from_file(TIMELOCK, d.join("brink_timelock.so")).expect("brink_timelock.so");
        let mut hook_loaded = false;
        if hook.is_some() {
            let hook_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../hook/deploy/brink_sim_hook.so");
            if svm.add_program_from_file(HOOK, &hook_path).is_ok() {
                hook_loaded = true;
            }
        }
        let payer = Keypair::new();
        let authority = Keypair::new();
        let guardian = Keypair::new();
        let publisher = Keypair::new();
        let treasury_owner = Keypair::new();
        let mint_authority = Keypair::new();
        let stranger = Keypair::new();
        let lps: Vec<Keypair> = (0..setup.n_lps).map(|_| Keypair::new()).collect();
        let traders: Vec<Keypair> = (0..setup.n_traders).map(|_| Keypair::new()).collect();
        for k in [&payer, &authority, &guardian, &publisher, &treasury_owner, &mint_authority, &stranger].into_iter().chain(lps.iter()).chain(traders.iter()) {
            svm.airdrop(&k.pubkey(), 1_000_000_000_000).unwrap();
        }
        let mut mint_data = vec![0u8; spl_token_interface::state::Mint::LEN];
        spl_token_interface::state::Mint { mint_authority: Some(mint_authority.pubkey()).into(), supply: 0, decimals: 6, is_initialized: true, freeze_authority: None.into() }.pack_into_slice(&mut mint_data);
        svm.set_account(USDC_DEVNET, Account { lamports: 10_000_000_000, data: mint_data, owner: TOKEN_PROGRAM, executable: false, rent_epoch: 0 }).unwrap();
        let mut c = svm.get_sysvar::<Clock>();
        c.slot = setup.start_slot;
        c.unix_timestamp = setup.start_ts;
        svm.set_sysvar(&c);
        svm.warp_to_slot(setup.start_slot);
        let mut c2 = svm.get_sysvar::<Clock>();
        c2.unix_timestamp = setup.start_ts;
        svm.set_sysvar(&c2);

        let registry = pda(&[b"registry"], &INDEX);
        let benchmark = pda(&[b"benchmark", &BENCH_ID], &INDEX);
        let global = pda(&[b"global"], &SWAP_AMM);
        let fee_vault = pda(&[b"fee_vault"], &SWAP_AMM);
        let pool = pda(&[b"pool", benchmark.as_ref()], &SWAP_AMM);
        let share_mint = pda(&[b"shares", pool.as_ref()], &SWAP_AMM);
        let vault = pda(&[b"vault", pool.as_ref()], &SWAP_AMM);
        let mut env = Env { svm, payer, authority, guardian, publisher, treasury_owner, mint_authority, stranger, lps, traders, registry, benchmark, global, fee_vault, pool, share_mint, vault, hook_loaded, cu_log: Vec::new() };

        let a = env.authority.insecure_clone();
        let pub_ = env.publisher.pubkey();
        env.must(&[Instruction { program_id: INDEX, accounts: vec![rw(registry), sig(a.pubkey()), sigw(env.payer.pubkey()), ro(SYSTEM)], data: data("initialise", &pub_) }], &[&a]);
        let args = CreateBenchmarkArgs { id: BENCH_ID, source: Pubkey::new_unique(), band_bp: setup.band_bp, max_staleness_slots: setup.max_staleness_slots, half_life_slots: setup.half_life_slots, min_interval_slots: setup.min_interval_slots };
        env.must(&[Instruction { program_id: INDEX, accounts: [vec![rw(registry), sig(a.pubkey()), sigw(env.payer.pubkey()), rw(benchmark), ro(SYSTEM)], evt(INDEX).to_vec()].concat(), data: data("create_benchmark", &args) }], &[&a]);

        let t = env.treasury_owner.pubkey();
        let treasury = env.create_ata(&t, &USDC_DEVNET);
        let escrow = env.create_ata(&buyback_owner(&t), &USDC_DEVNET);
        let g = env.guardian.pubkey();
        env.must(&[Instruction { program_id: SWAP_AMM, accounts: vec![rw(global), sig(a.pubkey()), sigw(env.payer.pubkey()), ro(USDC_DEVNET), rw(fee_vault), ro(treasury), ro(escrow), ro(TOKEN_PROGRAM), ro(SYSTEM)], data: data("admin_initialise_global", &InitialiseGlobalArgs { guardian: g, param_delay_slots: setup.param_delay_slots, limited_mode_cap: setup.limited_mode_cap }) }], &[&a]);
        let (hook_program, hooks) = match (hook, hook_loaded) {
            (Some(flags), true) => (Some(HOOK), flags),
            _ => (None, HookFlags::default()),
        };
        let pool_args = CreatePoolArgs { params: to_params(&setup.params), hook_program, hooks, pricer: Pricer::Vernier, min_notional: setup.min_notional, max_notional: setup.max_notional };
        env.must(&[Instruction { program_id: SWAP_AMM, accounts: [vec![rw(global), sig(a.pubkey()), sigw(env.payer.pubkey()), ro(benchmark), rw(pool), rw(share_mint), rw(vault), ro(USDC_DEVNET), ro(TOKEN_PROGRAM), ro(SYSTEM)], evt(SWAP_AMM).to_vec()].concat(), data: data("admin_create_pool", &pool_args) }], &[&a]);

        for i in 0..env.lps.len() {
            let k = env.lps[i].pubkey();
            let u = env.create_ata(&k, &USDC_DEVNET);
            env.create_ata(&k, &share_mint);
            env.mint_usdc(&u, TRADER_FUNDING);
        }
        for i in 0..env.traders.len() {
            let k = env.traders[i].pubkey();
            let u = env.create_ata(&k, &USDC_DEVNET);
            env.mint_usdc(&u, TRADER_FUNDING);
        }
        let s = env.stranger.pubkey();
        let u = env.create_ata(&s, &USDC_DEVNET);
        env.create_ata(&s, &share_mint);
        env.mint_usdc(&u, TRADER_FUNDING);
        env
    }

    pub fn send(&mut self, ixs: &[Instruction], signers: &[&Keypair]) -> Result<Vec<String>, String> {
        let msg = Message::new(ixs, Some(&self.payer.pubkey()));
        let mut all: Vec<&Keypair> = vec![&self.payer];
        for s in signers {
            if s.pubkey() != self.payer.pubkey() && !all.iter().any(|k| k.pubkey() == s.pubkey()) {
                all.push(s);
            }
        }
        self.svm.expire_blockhash();
        let mut tx = Transaction::new_unsigned(msg);
        tx.sign(&all, self.svm.latest_blockhash());
        let name = if ixs.len() == 1 { ix_name(&ixs[0].program_id, &ixs[0].data) } else { None };
        match self.svm.send_transaction(tx) {
            Ok(m) => {
                if let Some(n) = &name {
                    self.cu_log.push((n.clone(), true, m.compute_units_consumed));
                    record_cu(n, true, m.compute_units_consumed);
                }
                Ok(m.logs)
            }
            Err(f) => {
                if let Some(n) = &name {
                    self.cu_log.push((n.clone(), false, f.meta.compute_units_consumed));
                    record_cu(n, false, f.meta.compute_units_consumed);
                }
                Err(format!("{:?}\n{}", f.err, f.meta.logs.join("\n")))
            }
        }
    }
    pub fn must(&mut self, ixs: &[Instruction], signers: &[&Keypair]) -> Vec<String> {
        match self.send(ixs, signers) {
            Ok(l) => l,
            Err(e) => panic!("transaction failed:\n{e}"),
        }
    }
    pub fn must_fail(&mut self, ixs: &[Instruction], signers: &[&Keypair], code: &str) -> String {
        match self.send(ixs, signers) {
            Ok(logs) => panic!("expected failure {code}, succeeded:\n{}", logs.join("\n")),
            Err(e) => {
                assert!(e.contains(code), "expected error {code}, got:\n{e}");
                e
            }
        }
    }
    pub fn acct<T: BorshDeserialize>(&self, name: &str, k: &Pubkey) -> T {
        decode(name, &self.svm.get_account(k).expect("account exists").data)
    }
    pub fn try_acct<T: BorshDeserialize>(&self, name: &str, k: &Pubkey) -> Option<T> {
        let a = self.svm.get_account(k)?;
        if a.data.len() < 8 {
            return None;
        }
        Some(decode(name, &a.data))
    }
    pub fn token_amount(&self, k: &Pubkey) -> u64 {
        let a = self.svm.get_account(k).expect("token account");
        spl_token_interface::state::Account::unpack(&a.data).unwrap().amount
    }
    pub fn mint_supply(&self, k: &Pubkey) -> u64 {
        let a = self.svm.get_account(k).expect("mint");
        spl_token_interface::state::Mint::unpack(&a.data).unwrap().supply
    }
    pub fn clock(&self) -> Clock {
        self.svm.get_sysvar::<Clock>()
    }
    pub fn warp(&mut self, slots: u64, seconds: i64) {
        let mut c = self.clock();
        c.slot += slots;
        c.unix_timestamp += seconds;
        self.svm.warp_to_slot(c.slot);
        let mut c2 = self.clock();
        c2.unix_timestamp = c.unix_timestamp;
        self.svm.set_sysvar(&c2);
    }
    pub fn create_ata(&mut self, owner: &Pubkey, mint: &Pubkey) -> Pubkey {
        let ix = spl_associated_token_account_interface::instruction::create_associated_token_account(&self.payer.pubkey(), owner, mint, &TOKEN_PROGRAM);
        let ix = Instruction { program_id: ix.program_id, accounts: ix.accounts.into_iter().map(|m| AccountMeta { pubkey: m.pubkey, is_signer: m.is_signer, is_writable: m.is_writable }).collect(), data: ix.data };
        self.must(&[ix], &[]);
        ata(owner, mint)
    }
    /// A token account for `mint` owned by `owner` at a fresh (non-ATA) address.
    pub fn create_token_account(&mut self, owner: &Pubkey, mint: &Pubkey) -> Pubkey {
        let acct = Keypair::new();
        let rent = self.svm.minimum_balance_for_rent_exemption(spl_token_interface::state::Account::LEN);
        let create = solana_system_interface::instruction::create_account(&self.payer.pubkey(), &acct.pubkey(), rent, spl_token_interface::state::Account::LEN as u64, &TOKEN_PROGRAM);
        let init = spl_token_interface::instruction::initialize_account3(&TOKEN_PROGRAM, &acct.pubkey(), mint, owner).unwrap();
        let conv = |ix: solana_instruction::Instruction| ix;
        let init = Instruction { program_id: init.program_id, accounts: init.accounts.into_iter().map(|m| AccountMeta { pubkey: m.pubkey, is_signer: m.is_signer, is_writable: m.is_writable }).collect(), data: init.data };
        self.must(&[conv(create), init], &[&acct]);
        acct.pubkey()
    }
    /// A second SPL mint (wrong-mint tests).
    pub fn create_mint(&mut self) -> Pubkey {
        let mint = Keypair::new();
        let rent = self.svm.minimum_balance_for_rent_exemption(spl_token_interface::state::Mint::LEN);
        let create = solana_system_interface::instruction::create_account(&self.payer.pubkey(), &mint.pubkey(), rent, spl_token_interface::state::Mint::LEN as u64, &TOKEN_PROGRAM);
        let init = spl_token_interface::instruction::initialize_mint2(&TOKEN_PROGRAM, &mint.pubkey(), &self.mint_authority.pubkey(), None, 6).unwrap();
        let init = Instruction { program_id: init.program_id, accounts: init.accounts.into_iter().map(|m| AccountMeta { pubkey: m.pubkey, is_signer: m.is_signer, is_writable: m.is_writable }).collect(), data: init.data };
        self.must(&[create, init], &[&mint]);
        mint.pubkey()
    }
    pub fn mint_to(&mut self, mint: &Pubkey, to: &Pubkey, amount: u64) {
        let auth = self.mint_authority.insecure_clone();
        let ix = spl_token_interface::instruction::mint_to(&TOKEN_PROGRAM, mint, to, &auth.pubkey(), &[], amount).unwrap();
        let ix = Instruction { program_id: ix.program_id, accounts: ix.accounts.into_iter().map(|m| AccountMeta { pubkey: m.pubkey, is_signer: m.is_signer, is_writable: m.is_writable }).collect(), data: ix.data };
        self.must(&[ix], &[&auth]);
    }
    pub fn mint_usdc(&mut self, to: &Pubkey, amount: u64) {
        self.mint_to(&USDC_DEVNET, to, amount);
    }

    // ---- instruction builders ----
    pub fn publish_ix(&self, value_bp: u16) -> Instruction {
        Instruction { program_id: INDEX, accounts: [vec![ro(self.registry), sig(self.publisher.pubkey()), rw(self.benchmark)], evt(INDEX).to_vec()].concat(), data: data("publish", &value_bp) }
    }
    pub fn set_mode_ix(&self, signer: &Pubkey, mode: OperatingMode) -> Instruction {
        Instruction { program_id: SWAP_AMM, accounts: [vec![rw(self.global), sig(*signer)], evt(SWAP_AMM).to_vec()].concat(), data: data("admin_set_mode", &mode) }
    }
    pub fn deposit_ix(&self, lp: &Pubkey, amount: u64, min_shares: u64) -> Instruction {
        self.deposit_ix_with(lp, &ata(lp, &USDC_DEVNET), &ata(lp, &self.share_mint), amount, min_shares)
    }
    pub fn deposit_ix_with(&self, lp: &Pubkey, lp_usdc: &Pubkey, lp_shares: &Pubkey, amount: u64, min_shares: u64) -> Instruction {
        let hook = if self.hook_loaded { HOOK } else { SWAP_AMM };
        Instruction { program_id: SWAP_AMM, accounts: [vec![rw(self.global), rw(self.pool), sig(*lp), rw(*lp_usdc), rw(*lp_shares), rw(self.vault), rw(self.share_mint), ro(USDC_DEVNET), ro(hook), ro(TOKEN_PROGRAM)], evt(SWAP_AMM).to_vec()].concat(), data: data("lp_deposit", &(amount, min_shares)) }
    }
    pub fn withdraw_ix(&self, lp: &Pubkey, shares: u64, min_amount: u64) -> Instruction {
        self.withdraw_ix_with(lp, &ata(lp, &USDC_DEVNET), &ata(lp, &self.share_mint), shares, min_amount)
    }
    pub fn withdraw_ix_with(&self, lp: &Pubkey, lp_usdc: &Pubkey, lp_shares: &Pubkey, shares: u64, min_amount: u64) -> Instruction {
        let hook = if self.hook_loaded { HOOK } else { SWAP_AMM };
        Instruction { program_id: SWAP_AMM, accounts: [vec![rw(self.global), rw(self.pool), sig(*lp), rw(*lp_usdc), rw(*lp_shares), rw(self.vault), rw(self.fee_vault), rw(self.share_mint), ro(USDC_DEVNET), ro(hook), ro(TOKEN_PROGRAM)], evt(SWAP_AMM).to_vec()].concat(), data: data("lp_withdraw", &(shares, min_amount)) }
    }
    pub fn swap_pda(&self, trader: &Pubkey, seed: u64) -> Pubkey {
        pda(&[b"swap", self.pool.as_ref(), trader.as_ref(), &seed.to_le_bytes()], &SWAP_AMM)
    }
    pub fn open_ix(&self, trader: &Pubkey, args: &OpenSwapArgs) -> Instruction {
        self.open_ix_with(trader, &ata(trader, &USDC_DEVNET), args)
    }
    pub fn open_ix_with(&self, trader: &Pubkey, trader_usdc: &Pubkey, args: &OpenSwapArgs) -> Instruction {
        let swap = self.swap_pda(trader, args.client_seed);
        let hook = if self.hook_loaded { HOOK } else { SWAP_AMM };
        Instruction { program_id: SWAP_AMM, accounts: [vec![rw(self.global), rw(self.pool), ro(self.benchmark), rw(swap), sigw(*trader), rw(*trader_usdc), rw(self.vault), rw(self.fee_vault), ro(USDC_DEVNET), ro(hook), ro(TOKEN_PROGRAM), ro(SYSTEM)], evt(SWAP_AMM).to_vec()].concat(), data: data("trader_open_swap", args) }
    }
    /// `cranker_usdc = Some(ata)` claims the crank bounty; `None` passes the program id for the optional account.
    pub fn close_ix(&self, name: &str, trader: &Pubkey, seed: u64, signer: &Pubkey, cranker_usdc: Option<Pubkey>, args: &impl BorshSerialize) -> Instruction {
        self.close_ix_with(name, trader, seed, signer, &ata(trader, &USDC_DEVNET), cranker_usdc, args)
    }
    pub fn close_ix_with(&self, name: &str, trader: &Pubkey, seed: u64, signer: &Pubkey, trader_usdc: &Pubkey, cranker_usdc: Option<Pubkey>, args: &impl BorshSerialize) -> Instruction {
        let swap = self.swap_pda(trader, seed);
        let cranker = cranker_usdc.map(rw).unwrap_or(ro(SWAP_AMM));
        let hook = if self.hook_loaded { HOOK } else { SWAP_AMM };
        Instruction { program_id: SWAP_AMM, accounts: [vec![rw(self.global), rw(self.pool), ro(self.benchmark), rw(swap), rw(*trader), sig(*signer), rw(*trader_usdc), cranker, rw(self.vault), rw(self.fee_vault), ro(USDC_DEVNET), ro(hook), ro(TOKEN_PROGRAM)], evt(SWAP_AMM).to_vec()].concat(), data: data(name, args) }
    }
    pub fn sweep_ix(&self) -> Instruction {
        let t = self.treasury_owner.pubkey();
        Instruction { program_id: SWAP_AMM, accounts: [vec![rw(self.global), rw(self.fee_vault), rw(ata(&t, &USDC_DEVNET)), rw(ata(&buyback_owner(&t), &USDC_DEVNET)), ro(USDC_DEVNET), ro(TOKEN_PROGRAM)], evt(SWAP_AMM).to_vec()].concat(), data: data("sweep_fees", &()) }
    }
    pub fn sync_ix(&self) -> Instruction {
        Instruction { program_id: SWAP_AMM, accounts: [vec![rw(self.pool), ro(self.vault)], evt(SWAP_AMM).to_vec()].concat(), data: data("sync_vault", &()) }
    }
    pub fn queue_calibration_ix(&self, signer: &Pubkey, p: &Params) -> Instruction {
        Instruction { program_id: SWAP_AMM, accounts: [vec![ro(self.global), rw(self.pool), sig(*signer)], evt(SWAP_AMM).to_vec()].concat(), data: data("admin_queue_calibration", p) }
    }

    pub fn actor_key(&self, a: Actor) -> Keypair {
        match a {
            Actor::Authority => self.authority.insecure_clone(),
            Actor::Guardian => self.guardian.insecure_clone(),
            Actor::Stranger => self.stranger.insecure_clone(),
        }
    }

    /// Applies one scenario action to the chain. Returns the logs on success or the formatted error on failure.
    /// `Warp` and `Donate` cannot fail.
    pub fn apply(&mut self, a: &Action) -> Result<Vec<String>, String> {
        match a {
            Action::Warp { slots, secs } => {
                self.warp(*slots, *secs);
                Ok(Vec::new())
            }
            Action::Publish { value_bp } => {
                let p = self.publisher.insecure_clone();
                let ix = self.publish_ix(*value_bp);
                self.send(&[ix], &[&p])
            }
            Action::Deposit { lp, amount, min_shares } => {
                let k = self.lps[usize::from(*lp)].insecure_clone();
                let ix = self.deposit_ix(&k.pubkey(), *amount, *min_shares);
                self.send(&[ix], &[&k])
            }
            Action::Withdraw { lp, shares, min_amount } => {
                let k = self.lps[usize::from(*lp)].insecure_clone();
                let ix = self.withdraw_ix(&k.pubkey(), *shares, *min_amount);
                self.send(&[ix], &[&k])
            }
            Action::Open { trader, id, leg, tenor, notional, limit_bp } => {
                let k = self.traders[usize::from(*trader)].insecure_clone();
                let args = OpenSwapArgs { leg: to_leg(*leg), tenor: *tenor, notional: *notional, limit_rate_bp: *limit_bp, client_seed: *id };
                let ix = self.open_ix(&k.pubkey(), &args);
                self.send(&[ix], &[&k])
            }
            Action::Cancel { signer, id, min_payout } => {
                let owner = self.swap_owner(*id);
                let k = self.traders[usize::from(*signer)].insecure_clone();
                let ix = self.close_ix("trader_cancel_swap", &owner, *id, &k.pubkey(), None, min_payout);
                self.send(&[ix], &[&k])
            }
            Action::Settle { id, cranker } | Action::Liquidate { id, cranker } => {
                let name = if matches!(a, Action::Settle { .. }) { "crank_settle_swap" } else { "crank_liquidate_swap" };
                let owner = self.swap_owner(*id);
                match cranker {
                    Some(c) => {
                        let k = self.traders[usize::from(*c)].insecure_clone();
                        let ix = self.close_ix(name, &owner, *id, &k.pubkey(), Some(ata(&k.pubkey(), &USDC_DEVNET)), &());
                        self.send(&[ix], &[&k])
                    }
                    None => {
                        // the owner cranks their own position: no bounty destination
                        let k = self.traders.iter().find(|t| t.pubkey() == owner).map(|t| t.insecure_clone()).unwrap_or_else(|| self.payer.insecure_clone());
                        let ix = self.close_ix(name, &owner, *id, &k.pubkey(), None, &());
                        self.send(&[ix], &[&k])
                    }
                }
            }
            Action::Donate { amount } => {
                let v = self.vault;
                self.mint_usdc(&v, *amount);
                Ok(Vec::new())
            }
            Action::SyncVault => {
                let ix = self.sync_ix();
                self.send(&[ix], &[])
            }
            Action::Sweep => {
                let ix = self.sweep_ix();
                self.send(&[ix], &[])
            }
            Action::SetMode { by, mode } => {
                let k = self.actor_key(*by);
                let ix = self.set_mode_ix(&k.pubkey(), to_mode(*mode));
                self.send(&[ix], &[&k])
            }
            Action::QueueCalibration { params } => {
                let a = self.authority.insecure_clone();
                let ix = self.queue_calibration_ix(&a.pubkey(), &to_params(params));
                self.send(&[ix], &[&a])
            }
        }
    }

    /// Owner of swap `id`: the scenario generator assigns ids once, so the owner is found by scanning traders.
    pub fn swap_owner(&self, id: u64) -> Pubkey {
        for t in &self.traders {
            let k = self.swap_pda(&t.pubkey(), id);
            if let Some(acc) = self.svm.get_account(&k) {
                if acc.data.len() > 8 {
                    return t.pubkey();
                }
            }
        }
        // unknown swap: fall back to the first trader (the instruction then fails with AccountNotInitialized)
        self.traders[0].pubkey()
    }

    pub fn pool(&self) -> Pool {
        self.acct("Pool", &self.pool)
    }
    pub fn global(&self) -> Global {
        self.acct("Global", &self.global)
    }
    pub fn bench(&self) -> Benchmark {
        self.acct("Benchmark", &self.benchmark)
    }
}

/// Summarises `results/compute-units.tsv` into a markdown table (count, min, median, max per instruction).
pub fn cu_table(rows: &[(String, bool, u64)]) -> String {
    use std::collections::BTreeMap;
    let mut by: BTreeMap<(String, bool), Vec<u64>> = BTreeMap::new();
    for (n, ok, cu) in rows {
        by.entry((n.clone(), *ok)).or_default().push(*cu);
    }
    let mut s = String::from("| Instruction | Outcome | Count | Min CU | Median CU | Max CU |\n|---|---|---:|---:|---:|---:|\n");
    for ((n, ok), mut v) in by {
        v.sort_unstable();
        let med = v[v.len() / 2];
        s.push_str(&format!("| {n} | {} | {} | {} | {med} | {} |\n", if ok { "ok" } else { "rejected" }, v.len(), v[0], v[v.len() - 1]));
    }
    s
}
