//! Shared LiteSVM harness for the scenario, regression, adversarial and compute-unit suites. Boots a fresh VM
//! with the three compiled programs from `target/deploy`, seeds a USDC mint at the devnet address, and exposes
//! instruction builders that mirror the on-chain account order. Tests live under `tests/`; this file holds no
//! assertions of its own.
use crate::*;
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

pub const USDC: u64 = 1_000_000; // 6 dp
pub const DAY: i64 = 86_400;
/// Boot timestamp: 2026-09-17 00:00:00 UTC, a day boundary.
pub const BOOT_TS: i64 = 1_790_035_200;
/// Shares minted per USDC base unit at par: the share mint has 9 decimals against USDC's 6 (E1 0004).
pub const SHARE: u64 = 1_000;
pub const BENCH_ID: [u8; 16] = *b"kamino-usdc\0\0\0\0\0";

pub struct Env {
    pub svm: LiteSVM,
    pub payer: Keypair,
    pub authority: Keypair,
    pub guardian: Keypair,
    pub publisher: Keypair,
    pub lp: Keypair,
    pub trader: Keypair,
    pub treasury_owner: Keypair,
    pub mint_authority: Keypair,
    pub registry: Pubkey,
    pub benchmark: Pubkey,
    pub global: Pubkey,
    pub fee_vault: Pubkey,
    pub pool: Pubkey,
    pub share_mint: Pubkey,
    pub vault: Pubkey,
}

pub fn deploy_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/deploy")
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
/// `ComputeBudget::SetComputeUnitLimit` (discriminator 2, little-endian u32), as the app and the keeper send in
/// front of an instruction whose venue calls take it past the default budget.
pub fn budget_ix(units: u32) -> Instruction {
    let mut data = vec![2u8];
    data.extend_from_slice(&units.to_le_bytes());
    Instruction {
        program_id: Pubkey::from_str_const("ComputeBudget111111111111111111111111111111"),
        accounts: vec![],
        data,
    }
}

impl Env {
    pub fn send(
        &mut self,
        ixs: &[Instruction],
        signers: &[&Keypair],
    ) -> Result<Vec<String>, String> {
        let msg = Message::new(ixs, Some(&self.payer.pubkey()));
        let mut all: Vec<&Keypair> = vec![&self.payer];
        for s in signers {
            if s.pubkey() != self.payer.pubkey() {
                all.push(s);
            }
        }
        self.svm.expire_blockhash(); // fresh hash each time so identical retries are not deduplicated
        let mut tx = Transaction::new_unsigned(msg);
        tx.sign(&all, self.svm.latest_blockhash());
        match self.svm.send_transaction(tx) {
            Ok(m) => {
                // Compute-unit table: one Brink instruction per transaction is the common case in these tests.
                if ixs.len() == 1 {
                    if let Some(n) = ix_name(&ixs[0].program_id, &ixs[0].data) {
                        record_cu(&n, m.compute_units_consumed);
                    }
                }
                Ok(m.logs)
            }
            Err(f) => Err(format!("{:?}\n{}", f.err, f.meta.logs.join("\n"))),
        }
    }
    pub fn must(&mut self, ixs: &[Instruction], signers: &[&Keypair]) -> Vec<String> {
        match self.send(ixs, signers) {
            Ok(l) => l,
            Err(e) => panic!("transaction failed:\n{e}"),
        }
    }
    pub fn must_fail(&mut self, ixs: &[Instruction], signers: &[&Keypair], code: &str) {
        match self.send(ixs, signers) {
            Ok(logs) => panic!("expected failure {code}, succeeded:\n{}", logs.join("\n")),
            Err(e) => assert!(e.contains(code), "expected error {code}, got:\n{e}"),
        }
    }
    pub fn acct<T: borsh::BorshDeserialize>(&self, name: &str, k: &Pubkey) -> T {
        decode(name, &self.svm.get_account(k).expect("account exists").data)
    }
    pub fn token_amount(&self, k: &Pubkey) -> u64 {
        let a = self.svm.get_account(k).expect("token account");
        spl_token_interface::state::Account::unpack(&a.data)
            .unwrap()
            .amount
    }
    pub fn mint_supply(&self, k: &Pubkey) -> u64 {
        let a = self.svm.get_account(k).expect("mint");
        spl_token_interface::state::Mint::unpack(&a.data)
            .unwrap()
            .supply
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
        let ix =
            spl_associated_token_account_interface::instruction::create_associated_token_account(
                &self.payer.pubkey(),
                owner,
                mint,
                &TOKEN_PROGRAM,
            );
        let ix = Instruction {
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
        };
        self.must(&[ix], &[]);
        ata(owner, mint)
    }
    pub fn mint_usdc(&mut self, to: &Pubkey, amount: u64) {
        let auth = self.mint_authority.insecure_clone();
        let ix = spl_token_interface::instruction::mint_to(
            &TOKEN_PROGRAM,
            &USDC_DEVNET,
            to,
            &auth.pubkey(),
            &[],
            amount,
        )
        .unwrap();
        let ix = Instruction {
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
        };
        self.must(&[ix], &[&auth]);
    }

    // ---- program instructions ----
    pub fn publish(&mut self, value_bp: u16) -> Result<Vec<String>, String> {
        let ix = Instruction {
            program_id: INDEX,
            accounts: [
                vec![
                    ro(self.registry),
                    sig(self.publisher.pubkey()),
                    rw(self.benchmark),
                ],
                evt(INDEX).to_vec(),
            ]
            .concat(),
            data: data("publish", &value_bp),
        };
        let p = self.publisher.insecure_clone();
        self.send(&[ix], &[&p])
    }
    pub fn set_mode(
        &mut self,
        signer: &Keypair,
        mode: OperatingMode,
    ) -> Result<Vec<String>, String> {
        let ix = Instruction {
            program_id: SWAP_AMM,
            accounts: [
                vec![rw(self.global), sig(signer.pubkey())],
                evt(SWAP_AMM).to_vec(),
            ]
            .concat(),
            data: data("admin_set_mode", &mode),
        };
        self.send(&[ix], &[signer])
    }
    pub fn deposit_ix(&self, lp: &Pubkey, amount: u64, min_shares: u64) -> Instruction {
        Instruction {
            program_id: SWAP_AMM,
            accounts: [
                vec![
                    ro(self.global),
                    rw(self.pool),
                    ro(self.benchmark),
                    sig(*lp),
                    rw(ata(lp, &USDC_DEVNET)),
                    rw(ata(lp, &self.share_mint)),
                    rw(self.vault),
                    rw(self.share_mint),
                    ro(USDC_DEVNET),
                    ro(SWAP_AMM),
                    ro(TOKEN_PROGRAM),
                ],
                evt(SWAP_AMM).to_vec(),
            ]
            .concat(),
            data: data("lp_deposit", &(amount, min_shares)),
        }
    }
    pub fn withdraw_ix(&self, lp: &Pubkey, shares: u64, min_amount: u64) -> Instruction {
        Instruction {
            program_id: SWAP_AMM,
            accounts: [
                vec![
                    ro(self.global),
                    rw(self.pool),
                    ro(self.benchmark),
                    sig(*lp),
                    rw(ata(lp, &USDC_DEVNET)),
                    rw(ata(lp, &self.share_mint)),
                    rw(self.vault),
                    rw(self.share_mint),
                    ro(USDC_DEVNET),
                    ro(TOKEN_PROGRAM),
                ],
                evt(SWAP_AMM).to_vec(),
            ]
            .concat(),
            data: data("lp_withdraw", &(shares, min_amount)),
        }
    }
    pub fn swap_pda(&self, trader: &Pubkey, seed: u64) -> Pubkey {
        pda(
            &[
                b"swap",
                self.pool.as_ref(),
                trader.as_ref(),
                &seed.to_le_bytes(),
            ],
            &SWAP_AMM,
        )
    }
    pub fn open_ix(&self, trader: &Pubkey, args: &OpenSwapArgs) -> Instruction {
        let swap = self.swap_pda(trader, args.client_seed);
        Instruction {
            program_id: SWAP_AMM,
            accounts: [
                vec![
                    ro(self.global),
                    rw(self.pool),
                    ro(self.benchmark),
                    rw(swap),
                    sigw(*trader),
                    rw(ata(trader, &USDC_DEVNET)),
                    rw(self.vault),
                    ro(USDC_DEVNET),
                    ro(SWAP_AMM),
                    ro(TOKEN_PROGRAM),
                    ro(SYSTEM),
                ],
                evt(SWAP_AMM).to_vec(),
            ]
            .concat(),
            data: data("trader_open_swap", args),
        }
    }
    /// `trader_open_forward_swap`: the single-swap accounts with the forward arguments.
    pub fn open_forward_ix(&self, trader: &Pubkey, args: &OpenForwardSwapArgs) -> Instruction {
        let swap = self.swap_pda(trader, args.client_seed);
        Instruction {
            program_id: SWAP_AMM,
            accounts: [
                vec![
                    ro(self.global),
                    rw(self.pool),
                    ro(self.benchmark),
                    rw(swap),
                    sigw(*trader),
                    rw(ata(trader, &USDC_DEVNET)),
                    rw(self.vault),
                    ro(USDC_DEVNET),
                    ro(SWAP_AMM),
                    ro(TOKEN_PROGRAM),
                    ro(SYSTEM),
                ],
                evt(SWAP_AMM).to_vec(),
            ]
            .concat(),
            data: data("trader_open_forward_swap", args),
        }
    }
    /// `crank_start_forward`: permissionless, no signer beyond the fee payer.
    pub fn start_forward_ix(&self, trader: &Pubkey, seed: u64) -> Instruction {
        let swap = self.swap_pda(trader, seed);
        Instruction {
            program_id: SWAP_AMM,
            accounts: [
                vec![rw(self.pool), ro(self.benchmark), rw(swap)],
                evt(SWAP_AMM).to_vec(),
            ]
            .concat(),
            data: data("crank_start_forward", &()),
        }
    }
    pub fn close_ix(
        &self,
        name: &str,
        trader: &Pubkey,
        seed: u64,
        signer: &Pubkey,
        args: &impl borsh::BorshSerialize,
    ) -> Instruction {
        self.close_ix_bounty(name, trader, seed, signer, None, args)
    }
    /// `cranker_usdc = Some(ata)` claims the crank bounty; `None` passes the program id for the optional account.
    pub fn close_ix_bounty(
        &self,
        name: &str,
        trader: &Pubkey,
        seed: u64,
        signer: &Pubkey,
        cranker_usdc: Option<Pubkey>,
        args: &impl borsh::BorshSerialize,
    ) -> Instruction {
        let swap = self.swap_pda(trader, seed);
        let cranker = cranker_usdc.map(rw).unwrap_or(ro(SWAP_AMM));
        Instruction {
            program_id: SWAP_AMM,
            accounts: [
                vec![
                    ro(self.global),
                    rw(self.pool),
                    ro(self.benchmark),
                    rw(swap),
                    rw(*trader),
                    sig(*signer),
                    rw(ata(trader, &USDC_DEVNET)),
                    cranker,
                    rw(self.vault),
                    ro(USDC_DEVNET),
                    ro(TOKEN_PROGRAM),
                ],
                evt(SWAP_AMM).to_vec(),
            ]
            .concat(),
            data: data(name, args),
        }
    }
    pub fn sweep_ix(&self) -> Instruction {
        let t = self.treasury_owner.pubkey();
        Instruction {
            program_id: SWAP_AMM,
            accounts: [
                vec![
                    rw(self.global),
                    rw(self.pool),
                    rw(self.vault),
                    rw(ata(&t, &USDC_DEVNET)),
                    rw(ata(&buyback_owner(&t), &USDC_DEVNET)),
                    ro(USDC_DEVNET),
                    ro(TOKEN_PROGRAM),
                ],
                evt(SWAP_AMM).to_vec(),
            ]
            .concat(),
            data: data("sweep_fees", &()),
        }
    }
}

/// Deterministic second owner for the buyback escrow in tests.
pub fn buyback_owner(treasury_owner: &Pubkey) -> Pubkey {
    pda(&[b"test-buyback", treasury_owner.as_ref()], &SYSTEM)
}

/// Boots the VM with the three programs and the USDC mint, funds the actors and derives every PDA, but creates no
/// protocol account: `setup` adds the registry, benchmark, global and pool. Tests of initialisation use `boot`.
/// Overwrites the loader's `ProgramData` header for `program` with `authority`. The header is bincode:
/// u32 variant (3) || u64 slot || Option<Pubkey> (u8 tag || 32 bytes).
pub fn set_upgrade_authority(svm: &mut LiteSVM, program: &Pubkey, authority: Option<&Pubkey>) {
    let pd = program_data(program);
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

pub fn boot() -> Env {
    let mut svm = LiteSVM::new();
    let d = deploy_dir();
    svm.add_program_from_file(SWAP_AMM, d.join("swap_amm.so"))
        .expect("swap_amm.so missing: run ./build-sbf.sh first");
    svm.add_program_from_file(INDEX, d.join("brink_index.so"))
        .unwrap();
    svm.add_program_from_file(TIMELOCK, d.join("brink_timelock.so"))
        .unwrap();
    let keys: Vec<Keypair> = (0..8).map(|_| Keypair::new()).collect();
    // LiteSVM loads programs with no upgrade authority; every `initialise` binds its payer to that authority
    // (F-14), so the harness payer is made the deployer of all three programs.
    for p in [SWAP_AMM, INDEX, TIMELOCK] {
        set_upgrade_authority(&mut svm, &p, Some(&keys[0].pubkey()));
    }
    for k in &keys {
        svm.airdrop(&k.pubkey(), 100_000_000_000).unwrap();
    }
    // USDC at the devnet address, with a test mint authority so tests can mint freely.
    let auth = keys[7].insecure_clone();
    let mut mint_data = vec![0u8; spl_token_interface::state::Mint::LEN];
    spl_token_interface::state::Mint {
        mint_authority: Some(auth.pubkey()).into(),
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
    // A sane clock: slot 1000, a real timestamp at 00:00 UTC so that a swap opened at boot matures exactly
    // at a day boundary (maturity is aligned up to midnight for the fixings record, E1 0002).
    let mut c = svm.get_sysvar::<Clock>();
    c.slot = 1_000;
    c.unix_timestamp = BOOT_TS;
    svm.set_sysvar(&c);
    svm.warp_to_slot(1_000);
    let mut c2 = svm.get_sysvar::<Clock>();
    c2.unix_timestamp = BOOT_TS;
    svm.set_sysvar(&c2);

    let [payer, authority, guardian, publisher, lp, trader, treasury_owner, mint_authority]: [Keypair; 8] = keys.try_into().unwrap();
    let registry = pda(&[b"registry"], &INDEX);
    let benchmark = pda(&[b"benchmark", &BENCH_ID], &INDEX);
    let global = pda(&[b"global"], &SWAP_AMM);
    let fee_vault = pda(&[b"fee_vault"], &SWAP_AMM);
    let pool = pda(&[b"pool", benchmark.as_ref()], &SWAP_AMM);
    let share_mint = pda(&[b"shares", pool.as_ref()], &SWAP_AMM);
    let vault = pda(&[b"vault", pool.as_ref()], &SWAP_AMM);
    let mut env = Env {
        svm,
        payer,
        authority,
        guardian,
        publisher,
        lp,
        trader,
        treasury_owner,
        mint_authority,
        registry,
        benchmark,
        global,
        fee_vault,
        pool,
        share_mint,
        vault,
    };
    // Fund LP and trader (USDC only; the share account needs the pool and is created in `setup`).
    let lp = env.lp.pubkey();
    let tr = env.trader.pubkey();
    let lp_usdc = env.create_ata(&lp, &USDC_DEVNET);
    let tr_usdc = env.create_ata(&tr, &USDC_DEVNET);
    env.mint_usdc(&lp_usdc, 20_000_000 * USDC);
    env.mint_usdc(&tr_usdc, 1_000_000 * USDC);
    env
}

pub fn setup() -> Env {
    let mut env = boot();
    let (registry, benchmark, global, fee_vault, pool, share_mint, vault) = (
        env.registry,
        env.benchmark,
        env.global,
        env.fee_vault,
        env.pool,
        env.share_mint,
        env.vault,
    );

    // Index: registry, benchmark, first publish.
    let a = env.authority.insecure_clone();
    let pub_ = env.publisher.pubkey();
    env.must(
        &[Instruction {
            program_id: INDEX,
            accounts: vec![
                rw(registry),
                sig(a.pubkey()),
                sigw(env.payer.pubkey()),
                ro(program_data(&INDEX)),
                ro(SYSTEM),
            ],
            data: data(
                "initialise",
                &InitialiseArgs::single(env.guardian.pubkey(), pub_),
            ),
        }],
        &[&a],
    );
    let args = CreateBenchmarkArgs {
        id: BENCH_ID,
        source: Pubkey::new_unique(),
        guards: DEFAULT_GUARDS,
    };
    env.must(
        &[Instruction {
            program_id: INDEX,
            accounts: [
                vec![
                    rw(registry),
                    sig(a.pubkey()),
                    sigw(env.payer.pubkey()),
                    rw(benchmark),
                    ro(SYSTEM),
                ],
                evt(INDEX).to_vec(),
            ]
            .concat(),
            data: data("create_benchmark", &args),
        }],
        &[&a],
    );
    env.publish(684).unwrap();

    // Treasury + buyback escrow token accounts, then the AMM global.
    let t = env.treasury_owner.pubkey();
    let treasury = env.create_ata(&t, &USDC_DEVNET);
    let escrow = env.create_ata(&buyback_owner(&t), &USDC_DEVNET);
    let g = env.guardian.pubkey();
    env.must(
        &[Instruction {
            program_id: SWAP_AMM,
            accounts: vec![
                rw(global),
                sig(a.pubkey()),
                sigw(env.payer.pubkey()),
                ro(program_data(&SWAP_AMM)),
                ro(USDC_DEVNET),
                rw(fee_vault),
                ro(treasury),
                ro(escrow),
                ro(TOKEN_PROGRAM),
                ro(SYSTEM),
            ],
            data: data(
                "admin_initialise_global",
                &InitialiseGlobalArgs {
                    guardian: g,
                    param_delay_slots: 432_000,
                    limited_mode_cap: 100_000 * USDC,
                },
            ),
        }],
        &[&a],
    );
    let pool_args = CreatePoolArgs {
        params: DEFAULT_PARAMS,
        hook_program: None,
        hooks: HookFlags::default(),
        pricer: Pricer::Vernier,
        min_notional: 1_000 * USDC,
        max_notional: 50_000_000 * USDC,
    };
    env.must(
        &[Instruction {
            program_id: SWAP_AMM,
            accounts: [
                vec![
                    rw(global),
                    sig(a.pubkey()),
                    sigw(env.payer.pubkey()),
                    ro(benchmark),
                    rw(pool),
                    rw(share_mint),
                    rw(vault),
                    ro(USDC_DEVNET),
                    ro(TOKEN_PROGRAM),
                    ro(SYSTEM),
                ],
                evt(SWAP_AMM).to_vec(),
            ]
            .concat(),
            data: data("admin_create_pool", &pool_args),
        }],
        &[&a],
    );

    // The LP's share account on the pool.
    let lp = env.lp.pubkey();
    env.create_ata(&lp, &share_mint);
    env
}

pub fn open_args(leg: LegKind, tenor: u8, notional: u64, limit: u16, seed: u64) -> OpenSwapArgs {
    OpenSwapArgs {
        leg,
        tenor,
        notional,
        limit_rate_bp: limit,
        client_seed: seed,
    }
}

/// Keys of one pool, so a test can hold two pools and switch the builders between them.
#[derive(Clone, Copy, Debug)]
pub struct PoolKeys {
    pub benchmark: Pubkey,
    pub pool: Pubkey,
    pub share_mint: Pubkey,
    pub vault: Pubkey,
}

/// Location of the re-entrancy probe hook built by the simulation harness (`programs/tests/sim/hook`).
/// Point 0 re-enters `sync_vault`, point 1 writes the read-only pool, point 2 re-enters `trader_cancel_swap`
/// with escalated privileges, point 6 vetoes, every other point returns Ok.
pub fn hook_so_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../sim/hook/deploy/brink_sim_hook.so")
}
pub const HOOK_PROGRAM: Pubkey = Pubkey::new_from_array([0x48; 32]);

impl Env {
    /// The keys of the default pool created by `setup`.
    pub fn pool_keys(&self) -> PoolKeys {
        PoolKeys {
            benchmark: self.benchmark,
            pool: self.pool,
            share_mint: self.share_mint,
            vault: self.vault,
        }
    }
    /// Points every builder at another pool; returns the previous keys so the caller can switch back.
    pub fn use_pool(&mut self, k: &PoolKeys) -> PoolKeys {
        let old = self.pool_keys();
        self.benchmark = k.benchmark;
        self.pool = k.pool;
        self.share_mint = k.share_mint;
        self.vault = k.vault;
        old
    }
    pub fn sync_ix(&self) -> Instruction {
        Instruction {
            program_id: SWAP_AMM,
            accounts: [vec![rw(self.pool), ro(self.vault)], evt(SWAP_AMM).to_vec()].concat(),
            data: data("sync_vault", &()),
        }
    }
    /// Creates and seeds a second benchmark under the same registry.
    pub fn create_benchmark(&mut self, id: [u8; 16], first_value_bp: u16) -> Pubkey {
        let a = self.authority.insecure_clone();
        let benchmark = pda(&[b"benchmark", &id], &INDEX);
        let args = CreateBenchmarkArgs {
            id,
            source: Pubkey::new_unique(),
            guards: DEFAULT_GUARDS,
        };
        let registry = self.registry;
        let payer = self.payer.pubkey();
        self.must(
            &[Instruction {
                program_id: INDEX,
                accounts: [
                    vec![
                        rw(registry),
                        sig(a.pubkey()),
                        sigw(payer),
                        rw(benchmark),
                        ro(SYSTEM),
                    ],
                    evt(INDEX).to_vec(),
                ]
                .concat(),
                data: data("create_benchmark", &args),
            }],
            &[&a],
        );
        let ix = Instruction {
            program_id: INDEX,
            accounts: [
                vec![ro(registry), sig(self.publisher.pubkey()), rw(benchmark)],
                evt(INDEX).to_vec(),
            ]
            .concat(),
            data: data("publish", &first_value_bp),
        };
        let p = self.publisher.insecure_clone();
        self.must(&[ix], &[&p]);
        benchmark
    }
    /// Creates a pool on `benchmark` with an optional hook program and flags. Returns its keys.
    pub fn create_pool(
        &mut self,
        benchmark: Pubkey,
        hook_program: Option<Pubkey>,
        hooks: HookFlags,
    ) -> Result<PoolKeys, String> {
        let a = self.authority.insecure_clone();
        let pool = pda(&[b"pool", benchmark.as_ref()], &SWAP_AMM);
        let share_mint = pda(&[b"shares", pool.as_ref()], &SWAP_AMM);
        let vault = pda(&[b"vault", pool.as_ref()], &SWAP_AMM);
        let args = CreatePoolArgs {
            params: DEFAULT_PARAMS,
            hook_program,
            hooks,
            pricer: Pricer::Vernier,
            min_notional: 1_000 * USDC,
            max_notional: 50_000_000 * USDC,
        };
        let global = self.global;
        let payer = self.payer.pubkey();
        self.send(
            &[Instruction {
                program_id: SWAP_AMM,
                accounts: [
                    vec![
                        rw(global),
                        sig(a.pubkey()),
                        sigw(payer),
                        ro(benchmark),
                        rw(pool),
                        rw(share_mint),
                        rw(vault),
                        ro(USDC_DEVNET),
                        ro(TOKEN_PROGRAM),
                        ro(SYSTEM),
                    ],
                    evt(SWAP_AMM).to_vec(),
                ]
                .concat(),
                data: data("admin_create_pool", &args),
            }],
            &[&a],
        )?;
        Ok(PoolKeys {
            benchmark,
            pool,
            share_mint,
            vault,
        })
    }
    /// The `BasisPair` PDA for pay-fixed on `pool_a` against receive-fixed on `pool_b`.
    pub fn basis_pair_pda(pool_a: &Pubkey, pool_b: &Pubkey) -> Pubkey {
        pda(&[b"basis", pool_a.as_ref(), pool_b.as_ref()], &SWAP_AMM)
    }
    /// Governance enables a basis pair with the given correlation offset (authority signs, payer pays rent).
    pub fn set_basis_pair_ix(
        &self,
        pool_a: &Pubkey,
        pool_b: &Pubkey,
        correlation_bp: u16,
    ) -> Instruction {
        Instruction {
            program_id: SWAP_AMM,
            accounts: [
                vec![
                    ro(self.global),
                    sig(self.authority.pubkey()),
                    sigw(self.payer.pubkey()),
                    ro(*pool_a),
                    ro(*pool_b),
                    rw(Self::basis_pair_pda(pool_a, pool_b)),
                    ro(SYSTEM),
                ],
                evt(SWAP_AMM).to_vec(),
            ]
            .concat(),
            data: data("admin_set_basis_pair", &SetBasisPairArgs { correlation_bp }),
        }
    }
    /// Governance changes an existing pair's offset.
    pub fn update_basis_pair_ix(
        &self,
        pool_a: &Pubkey,
        pool_b: &Pubkey,
        correlation_bp: u16,
    ) -> Instruction {
        Instruction {
            program_id: SWAP_AMM,
            accounts: [
                vec![
                    ro(self.global),
                    sig(self.authority.pubkey()),
                    rw(Self::basis_pair_pda(pool_a, pool_b)),
                ],
                evt(SWAP_AMM).to_vec(),
            ]
            .concat(),
            data: data(
                "admin_update_basis_pair",
                &SetBasisPairArgs { correlation_bp },
            ),
        }
    }
    /// The swap PDA of a leg on an arbitrary pool (basis legs share one seed across two pools).
    pub fn swap_pda_on(pool: &Pubkey, trader: &Pubkey, seed: u64) -> Pubkey {
        pda(
            &[b"swap", pool.as_ref(), trader.as_ref(), &seed.to_le_bytes()],
            &SWAP_AMM,
        )
    }
    /// Opens a basis swap: pay fixed on `a`, receive fixed on `b`. Hook slots carry the program id (no hook).
    pub fn open_basis_ix(
        &self,
        trader: &Pubkey,
        a: &PoolKeys,
        b: &PoolKeys,
        args: &OpenBasisSwapArgs,
    ) -> Instruction {
        Instruction {
            program_id: SWAP_AMM,
            accounts: [
                vec![
                    ro(self.global),
                    ro(Self::basis_pair_pda(&a.pool, &b.pool)),
                    rw(a.pool),
                    ro(a.benchmark),
                    rw(Self::swap_pda_on(&a.pool, trader, args.client_seed)),
                    rw(a.vault),
                    rw(b.pool),
                    ro(b.benchmark),
                    rw(Self::swap_pda_on(&b.pool, trader, args.client_seed)),
                    rw(b.vault),
                    sigw(*trader),
                    rw(ata(trader, &USDC_DEVNET)),
                    ro(USDC_DEVNET),
                    ro(SWAP_AMM),
                    ro(SWAP_AMM),
                    ro(TOKEN_PROGRAM),
                    ro(SYSTEM),
                ],
                evt(SWAP_AMM).to_vec(),
            ]
            .concat(),
            data: data("trader_open_basis_swap", args),
        }
    }
    /// Closes both legs of a basis swap with one floor on the net payout.
    pub fn cancel_basis_ix(
        &self,
        trader: &Pubkey,
        a: &PoolKeys,
        b: &PoolKeys,
        seed: u64,
        min_payout: u64,
    ) -> Instruction {
        Instruction {
            program_id: SWAP_AMM,
            accounts: [
                vec![
                    ro(self.global),
                    rw(a.pool),
                    ro(a.benchmark),
                    rw(Self::swap_pda_on(&a.pool, trader, seed)),
                    rw(a.vault),
                    rw(b.pool),
                    ro(b.benchmark),
                    rw(Self::swap_pda_on(&b.pool, trader, seed)),
                    rw(b.vault),
                    sigw(*trader),
                    rw(ata(trader, &USDC_DEVNET)),
                    ro(USDC_DEVNET),
                    ro(TOKEN_PROGRAM),
                ],
                evt(SWAP_AMM).to_vec(),
            ]
            .concat(),
            data: data("trader_cancel_basis_swap", &min_payout),
        }
    }
    /// Loads the simulation harness's hook probe at `HOOK_PROGRAM`. Returns false when the binary is absent.
    pub fn load_hook_program(&mut self) -> bool {
        let p = hook_so_path();
        if !p.exists() {
            return false;
        }
        self.svm.add_program_from_file(HOOK_PROGRAM, p).is_ok()
    }
    /// A funded second LP or trader with USDC and (optionally) a share account on the current pool.
    pub fn new_actor(&mut self, usdc: u64, with_shares: bool) -> Keypair {
        let k = Keypair::new();
        self.svm.airdrop(&k.pubkey(), 10_000_000_000).unwrap();
        let u = self.create_ata(&k.pubkey(), &USDC_DEVNET);
        if usdc > 0 {
            self.mint_usdc(&u, usdc);
        }
        if with_shares {
            let m = self.share_mint;
            self.create_ata(&k.pubkey(), &m);
        }
        k
    }
    /// Largest deposit that fits the `i64` share field of `LiquidityChanged` at par with 12-decimal shares
    /// (round-02 test finding T-1): 9,223,372,036,854 units, about 9.22 million USDC.
    pub const MAX_EVENT_DEPOSIT: u64 = (i64::MAX / 1_000_000) as u64;
    /// Deposits `amount` in tranches that stay under the event's `i64` share limit (one transaction each).
    pub fn deposit(&mut self, lp: &Keypair, amount: u64) {
        let mut left = amount;
        while left > 0 {
            let part = left.min(Self::MAX_EVENT_DEPOSIT);
            let ix = self.deposit_ix(&lp.pubkey(), part, 0);
            self.must(&[ix], &[lp]);
            left -= part;
        }
    }
    /// Withdraws `shares` in tranches that stay under the event's `i64` share limit (one transaction each).
    pub fn withdraw(&mut self, lp: &Keypair, shares: u64) {
        let mut left = shares;
        while left > 0 {
            let part = left.min(i64::MAX as u64);
            let ix = self.withdraw_ix(&lp.pubkey(), part, 0);
            self.must(&[ix], &[lp]);
            left -= part;
        }
    }
    /// ADR-009 withdraw queue PDA for the current pool.
    pub fn queue_pda(&self) -> Pubkey {
        pda(&[b"queue", self.pool.as_ref()], &SWAP_AMM)
    }
    pub fn escrow_pda(&self) -> Pubkey {
        pda(&[b"escrow", self.pool.as_ref()], &SWAP_AMM)
    }
    pub fn init_queue_ix(&self) -> Instruction {
        Instruction {
            program_id: SWAP_AMM,
            accounts: [
                vec![
                    ro(self.global),
                    ro(self.pool),
                    rw(self.queue_pda()),
                    rw(self.escrow_pda()),
                    ro(self.share_mint),
                    sigw(self.payer.pubkey()),
                    ro(TOKEN_PROGRAM),
                    ro(SYSTEM),
                ],
                evt(SWAP_AMM).to_vec(),
            ]
            .concat(),
            data: data("init_withdraw_queue", &()),
        }
    }
    pub fn request_pda(&self, lp: &Pubkey, seed: u64) -> Pubkey {
        pda(
            &[
                b"request",
                self.pool.as_ref(),
                lp.as_ref(),
                &seed.to_le_bytes(),
            ],
            &SWAP_AMM,
        )
    }
    pub fn enqueue_ix(&self, lp: &Pubkey, shares: u64, seed: u64) -> Instruction {
        Instruction {
            program_id: SWAP_AMM,
            accounts: [
                vec![
                    ro(self.global),
                    rw(self.pool),
                    rw(self.queue_pda()),
                    rw(self.request_pda(lp, seed)),
                    sigw(*lp),
                    rw(ata(lp, &self.share_mint)),
                    ro(ata(lp, &USDC_DEVNET)),
                    rw(self.escrow_pda()),
                    ro(self.share_mint),
                    ro(TOKEN_PROGRAM),
                    ro(SYSTEM),
                ],
                evt(SWAP_AMM).to_vec(),
            ]
            .concat(),
            data: data("lp_enqueue_withdraw", &(shares, seed)),
        }
    }
    /// Permissionless epoch processing (ADR-009): prices the queued shares at book value and fills what the caps
    /// allow. The pool's benchmark is read for the book value.
    pub fn process_ix(&self) -> Instruction {
        Instruction {
            program_id: SWAP_AMM,
            accounts: [
                vec![
                    ro(self.global),
                    rw(self.pool),
                    ro(self.benchmark),
                    rw(self.queue_pda()),
                    rw(self.escrow_pda()),
                    rw(self.share_mint),
                    rw(self.vault),
                    ro(TOKEN_PROGRAM),
                ],
                evt(SWAP_AMM).to_vec(),
            ]
            .concat(),
            data: data("crank_process_withdrawals", &()),
        }
    }
    /// Permissionless claim of a processed request: pays the fill to `lp_usdc`, returns unfilled shares, closes
    /// the request to `lp`. `signer` may be anyone.
    pub fn claim_ix(&self, lp: &Pubkey, seed: u64, signer: &Pubkey) -> Instruction {
        Instruction {
            program_id: SWAP_AMM,
            accounts: [
                vec![
                    ro(self.global),
                    rw(self.pool),
                    rw(self.queue_pda()),
                    rw(self.request_pda(lp, seed)),
                    rw(*lp),
                    sig(*signer),
                    rw(ata(lp, &USDC_DEVNET)),
                    rw(ata(lp, &self.share_mint)),
                    rw(self.vault),
                    rw(self.escrow_pda()),
                    rw(self.share_mint),
                    ro(USDC_DEVNET),
                    ro(TOKEN_PROGRAM),
                ],
                evt(SWAP_AMM).to_vec(),
            ]
            .concat(),
            data: data("lp_claim_withdrawal", &()),
        }
    }
    /// Replaces one account meta's pubkey in an instruction (account substitution helper).
    pub fn substitute(ix: &Instruction, from: Pubkey, to: Pubkey) -> Instruction {
        let mut ix = ix.clone();
        for m in ix.accounts.iter_mut() {
            if m.pubkey == from {
                m.pubkey = to;
            }
        }
        ix
    }
    /// Builds an `open_ix` whose optional hook account is set (the hook sits after `usdc_mint`).
    pub fn open_ix_hook(&self, trader: &Pubkey, args: &OpenSwapArgs, hook: Pubkey) -> Instruction {
        let ix = self.open_ix(trader, args);
        set_hook_slot(ix, 8, hook)
    }
    pub fn deposit_ix_hook(
        &self,
        lp: &Pubkey,
        amount: u64,
        min_shares: u64,
        hook: Pubkey,
    ) -> Instruction {
        let ix = self.deposit_ix(lp, amount, min_shares);
        set_hook_slot(ix, 9, hook)
    }
}

/// The optional `hook_program` account is encoded as the program id when absent; this replaces that slot.
fn set_hook_slot(mut ix: Instruction, slot: usize, hook: Pubkey) -> Instruction {
    assert_eq!(
        ix.accounts[slot].pubkey, SWAP_AMM,
        "slot {slot} is the hook placeholder"
    );
    ix.accounts[slot] = ro(hook);
    ix
}
