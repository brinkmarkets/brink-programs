//! Thin Anchor-compatible client for the tests: discriminators, PDAs, account decoding.
use borsh::{BorshDeserialize, BorshSerialize};
use sha2::{Digest, Sha256};
use solana_pubkey::Pubkey;

pub const SWAP_AMM: Pubkey = Pubkey::from_str_const("EbD862QySygYKCMd8RHM2JaHqnduLQXB8Y632U3pQtce");
pub const INDEX: Pubkey = Pubkey::from_str_const("J2udZ8xzSsETsrSnbW3DeLFBTVooKRup3P7LWKSRwuvS");
pub const TIMELOCK: Pubkey = Pubkey::from_str_const("CeQz4x7Ad7Hg715Tn4PHtvtAxYin2PHv4KGaD3KS3kM5");
pub const USDC_DEVNET: Pubkey =
    Pubkey::from_str_const("4zMMC9srt5Ri5X14GAgXhaHii3GnPAEERYPJgZJDncDU");
pub const TOKEN_PROGRAM: Pubkey =
    Pubkey::from_str_const("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
pub const ATA_PROGRAM: Pubkey =
    Pubkey::from_str_const("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
pub const SYSTEM: Pubkey = Pubkey::from_str_const("11111111111111111111111111111111");
pub const LOADER: Pubkey = Pubkey::from_str_const("BPFLoaderUpgradeab1e11111111111111111111111");
/// The loader's `ProgramData` account of `program`; every `initialise` binds its payer to the upgrade authority.
pub fn program_data(program: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[program.as_ref()], &LOADER).0
}
pub const MAX_PUBLISHERS: usize = 5;
pub const FIXING_DAYS: u32 = 128;
pub const FIXING_BYTES: usize = 16 * FIXING_DAYS as usize;

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
    pda(
        &[owner.as_ref(), TOKEN_PROGRAM.as_ref(), mint.as_ref()],
        &ATA_PROGRAM,
    )
}
pub fn decode<T: BorshDeserialize>(name: &str, bytes: &[u8]) -> T {
    assert_eq!(
        &bytes[..8],
        &acc_disc(name),
        "discriminator mismatch for {name}"
    );
    T::try_from_slice(&bytes[8..])
        .or_else(|_| T::deserialize(&mut &bytes[8..]))
        .unwrap()
}

// ---- mirrored layouts (field order is the on-chain contract; keep in sync with the programs) ----
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
pub const DEFAULT_PARAMS: Params = Params {
    model_pay_bp: [11, 22, 31, 54],
    model_rec_bp: [9, 12, 14, 19],
    term_bp: [3, 5, 7, 12],
    demand_k_bp: 45,
    demand_cap_bp: 60,
    collateral_bp: [120, 230, 330, 600],
};
#[derive(BorshSerialize, BorshDeserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HookFlags {
    pub before_open: bool,
    pub after_open: bool,
    /// Retired exit point (review F-30); must be false.
    pub reserved_2: bool,
    pub reserved_3: bool,
    pub before_deposit: bool,
    pub after_deposit: bool,
    pub reserved_6: bool,
    pub reserved_7: bool,
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
    Capped { pnl: i64 },
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
    pub book_pay: BookSide,
    pub book_rec: BookSide,
    pub collateral_pay: u64,
    pub collateral_rec: u64,
    pub limited_window_start: i64,
    pub limited_window_notional: u64,
    pub fees_buyback_accrued: u64,
    pub fees_treasury_accrued: u64,
    pub withdraw_reserved: u64,
    pub ladder: [u8; 1536],
    pub share_supply: u64,
    pub queued_shares: u64,
    pub queue_first_slot: u64,
    pub _reserved: [u8; 24],
}
#[derive(BorshSerialize, BorshDeserialize, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BookSide {
    pub notional: i128,
    pub accrual_start: i128,
    pub maturity_weight: i128,
    pub fixed_leg: i128,
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
    pub limited_window_start: i64,
    /// Key of the other leg of a basis swap; zero for an ordinary swap.
    pub link: Pubkey,
    pub link_flags: u8,
    /// Forward-starting swap: first accrual instant; zero on a spot swap.
    pub start_ts: i64,
    pub _reserved: [u8; 15],
}
/// Mirror of `BasisPair`: governance record enabling pay-fixed on `pool_a` against
/// receive-fixed on `pool_b` with a correlation offset on the demand charge.
#[derive(BorshSerialize, BorshDeserialize, Debug)]
pub struct BasisPair {
    pub pool_a: Pubkey,
    pub pool_b: Pubkey,
    pub correlation_bp: u16,
    pub bump: u8,
    pub _reserved: [u8; 32],
}
pub const LINK_BASIS: u8 = 1;
pub const LINK_LEG_B: u8 = 2;
pub const LINK_FORWARD: u8 = 4;
pub const FORWARD_STARTED: u8 = 8;
/// Mirror of `queue::WithdrawQueue` (ADR-009).
#[derive(BorshSerialize, BorshDeserialize, Debug, Clone, Copy, Default)]
pub struct EpochFill {
    pub epoch: u64,
    pub shares_queued: u64,
    pub shares_filled: u64,
    pub net_amount: u64,
    pub amount_left: u64,
    pub shares_return_left: u64,
    pub unclaimed: u32,
}
#[derive(BorshSerialize, BorshDeserialize, Debug)]
pub struct WithdrawQueue {
    pub pool: Pubkey,
    pub escrow: Pubkey,
    pub epoch: u64,
    pub first_request_slot: u64,
    pub queued_shares: u64,
    pub request_count: u32,
    pub bump: u8,
    pub escrow_bump: u8,
    pub fills: [EpochFill; 8],
    pub _reserved: [u8; 64],
}
#[derive(BorshSerialize, BorshDeserialize, Debug)]
pub struct Registry {
    pub version: u8,
    pub authority: Pubkey,
    pub guardian: Pubkey,
    pub publishers: [Pubkey; MAX_PUBLISHERS],
    pub quorum: u8,
    pub single_publisher: bool,
    pub count: u32,
    pub bump: u8,
    pub _reserved: [u8; 64],
}
#[derive(BorshSerialize, BorshDeserialize, Debug, Clone, Copy, Default)]
pub struct Observation {
    pub publisher: Pubkey,
    pub value_bp: u16,
    pub slot: u64,
}
/// Mirror of `brink_index::Benchmark` layout version 2 (quorum, drift bound, milli-bp EMA, fixings ring).
#[derive(BorshSerialize, BorshDeserialize, Debug)]
pub struct Benchmark {
    pub version: u8,
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
    pub prev_value_bp: u16,
    pub prev_unix_ts: i64,
    pub max_drift_bp: u16,
    pub drift_window_slots: u64,
    pub drift_anchor_bp: u16,
    pub drift_anchor_slot: u64,
    pub clamped: bool,
    pub disputed: bool,
    pub support: u8,
    pub observations: [Observation; MAX_PUBLISHERS],
    pub ema_milli_bp: u32,
    pub fixing_first_day: u32,
    pub fixing_head_day: u32,
    pub fixings: [u8; FIXING_BYTES],
    pub ema_slot: u64,
    pub accepted_slot: u64,
    pub _reserved: [u8; 12],
}
impl Benchmark {
    /// Cumulative accrual at 00:00 UTC of UTC day `day` (`accrual_e18` units); 0 when never written.
    pub fn fixing(&self, day: u32) -> u128 {
        let at = 16 * (day % FIXING_DAYS) as usize;
        u128::from_le_bytes(self.fixings[at..at + 16].try_into().unwrap())
    }
    /// Byte offset of `fixings` inside the account data (after the 8-byte discriminator).
    pub const FIXINGS_OFFSET: usize = 8
        + 1
        + 32
        + 16
        + 32
        + 2
        + 2
        + 8
        + 8
        + 16
        + 8
        + 2
        + 8
        + 8
        + 1
        + 8
        + 1
        + 2
        + 8
        + 2
        + 8
        + 2
        + 8
        + 1
        + 1
        + 1
        + 42 * MAX_PUBLISHERS
        + 4
        + 4
        + 4;
    /// Cumulative accrual at 00:00 UTC of UTC day `day`, read straight from the raw account bytes.
    pub fn fixing_from_bytes(data: &[u8], day: u32) -> u128 {
        let at = Self::FIXINGS_OFFSET + 16 * (day % FIXING_DAYS) as usize;
        u128::from_le_bytes(data[at..at + 16].try_into().unwrap())
    }
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
    Upgrade {
        program: Pubkey,
        buffer: Pubkey,
        buffer_hash: [u8; 32],
    },
    SetUpgradeAuthority {
        program: Pubkey,
        new_authority: Pubkey,
    },
    SetDelay {
        delay_slots: u64,
    },
    SetRoles {
        proposer: Pubkey,
        executor: Pubkey,
        guardian: Pubkey,
    },
    /// Generic delayed CPI signed by the `["authority"]` PDA (ADR-011, review F-13).
    Invoke {
        program: Pubkey,
        accounts_hash: [u8; 32],
        data_hash: [u8; 32],
    },
}
/// Mirror of `brink_timelock::invoke_accounts_hash`: sha256 over the program id then `key || signer || writable`
/// for each account meta.
pub fn invoke_accounts_hash(program: &Pubkey, metas: &[(Pubkey, bool, bool)]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(program.as_ref());
    for (k, s, w) in metas {
        h.update(k.as_ref());
        h.update([u8::from(*s), u8::from(*w)]);
    }
    h.finalize().into()
}
pub fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
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
    pub payer: Pubkey,
}

// ---- instruction args ----
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
#[derive(BorshSerialize, Clone, Copy)]
pub struct OpenBasisSwapArgs {
    pub tenor: u8,
    pub notional: u64,
    pub limit_pay_bp: u16,
    pub limit_receive_bp: u16,
    pub client_seed: u64,
}
#[derive(BorshSerialize, Clone, Copy)]
pub struct OpenForwardSwapArgs {
    pub leg: LegKind,
    pub start: u8,
    pub tenor: u8,
    pub notional: u64,
    pub limit_rate_bp: u16,
    pub client_seed: u64,
}
#[derive(BorshSerialize, Clone, Copy)]
pub struct SetBasisPairArgs {
    pub correlation_bp: u16,
}
#[derive(BorshSerialize, Clone, Copy)]
pub struct GuardArgs {
    pub band_bp: u16,
    pub max_staleness_slots: u64,
    pub half_life_slots: u64,
    pub min_interval_slots: u64,
    pub max_drift_bp: u16,
    pub drift_window_slots: u64,
}
pub const DEFAULT_GUARDS: GuardArgs = GuardArgs {
    band_bp: 300,
    max_staleness_slots: 2_000,
    half_life_slots: 10_000,
    min_interval_slots: 0,
    max_drift_bp: 400,
    drift_window_slots: 216_000,
};
#[derive(BorshSerialize)]
pub struct CreateBenchmarkArgs {
    pub id: [u8; 16],
    pub source: Pubkey,
    pub guards: GuardArgs,
}
#[derive(BorshSerialize)]
pub struct InitialiseArgs {
    pub guardian: Pubkey,
    pub publishers: [Pubkey; MAX_PUBLISHERS],
    pub quorum: u8,
    pub allow_single_publisher: bool,
}
impl InitialiseArgs {
    /// Devnet shape: one publisher, quorum 1, flagged `single_publisher`.
    pub fn single(guardian: Pubkey, publisher: Pubkey) -> Self {
        let mut publishers = [Pubkey::default(); MAX_PUBLISHERS];
        publishers[0] = publisher;
        InitialiseArgs {
            guardian,
            publishers,
            quorum: 1,
            allow_single_publisher: true,
        }
    }
}

/// Every instruction name across the three programs, for mapping a discriminator back to a name (compute-unit table).
pub const IX_NAMES: &[&str] = &[
    "admin_initialise_global",
    "admin_set_mode",
    "admin_set_authority",
    "admin_queue_calibration",
    "admin_create_pool",
    "lp_deposit",
    "lp_withdraw",
    "init_withdraw_queue",
    "lp_enqueue_withdraw",
    "lp_dequeue_withdraw",
    "crank_process_withdrawals",
    "lp_claim_withdrawal",
    "trader_open_swap",
    "trader_cancel_swap",
    "crank_settle_swap",
    "crank_liquidate_swap",
    "admin_set_basis_pair",
    "admin_update_basis_pair",
    "trader_open_basis_swap",
    "trader_cancel_basis_swap",
    "sync_vault",
    "sweep_fees",
    "initialise",
    "create_benchmark",
    "publish",
    "set_guards",
    "set_publishers",
    "remove_publisher",
    "set_guardian",
    "set_authority",
    "queue",
    "cancel",
    "execute_upgrade",
    "execute_set_upgrade_authority",
    "execute_invoke",
    "execute_config",
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
    IX_NAMES
        .iter()
        .find(|n| ix_disc(n) == data[..8])
        .map(|n| format!("{prefix}::{n}"))
}
/// Appends one compute-unit observation (`program::instruction<TAB>cu`) to `target/compute-units.tsv`.
pub fn record_cu(name: &str, cu: u64) {
    use std::io::Write;
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/compute-units.tsv");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        // One write call per row: with O_APPEND a single short write is atomic, so parallel tests do not
        // interleave rows.
        let row = format!("{name}\t{cu}\n");
        let _ = f.write_all(row.as_bytes());
    }
}
pub mod harness;
