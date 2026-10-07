//! Harness extension for the product programs built on the AMM: the allowlist hook (permissioned pools), the
//! venue adapter (devnet venue), fixed vaults and yield splitting. Mirrors their account layouts and
//! instruction account orders; the assertions live in `tests/{allowlist,venue,vaults,split}.rs`.
use crate::harness::*;
use crate::*;
use borsh::{BorshDeserialize, BorshSerialize};
use solana_instruction::{AccountMeta, Instruction};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signer::Signer;

pub const ALLOWLIST: Pubkey =
    Pubkey::from_str_const("FaNttHSrjcQruS8qBNFDdmAnKq9Z4ZGu2y1UkMYXe1UR");
pub const VENUE: Pubkey = Pubkey::from_str_const("Boj38zJFfsN72DBL2nrJEfk1ewskn11EbdikgHMBX7v2");
pub const VAULTS: Pubkey = Pubkey::from_str_const("HhDTKdT36W1vE1DBgKnXDp7gFtxpxhRxovmfanuhaQiy");
pub const SPLIT: Pubkey = Pubkey::from_str_const("ASYzAAxpLwQW5XwdL1GJbpQ15onLR6uM1HBSCRTza9sz");

// ---- mirrored layouts ----
#[derive(BorshDeserialize, Debug)]
pub struct List {
    pub version: u8,
    pub pool: Pubkey,
    pub manager: Pubkey,
    pub pending_manager: Pubkey,
    pub gate_open: bool,
    pub gate_deposit: bool,
    pub frozen: bool,
    pub entries: u32,
    pub bump: u8,
    pub reserved: [u8; 64],
}
#[derive(BorshDeserialize, Debug)]
pub struct Entry {
    pub version: u8,
    pub list: Pubkey,
    pub wallet: Pubkey,
    pub may_open: bool,
    pub may_deposit: bool,
    pub expires_ts: i64,
    pub added_ts: i64,
    pub bump: u8,
    pub reserved: [u8; 32],
}
#[derive(BorshSerialize, Clone, Copy)]
pub struct CreateListArgs {
    pub manager: Pubkey,
    pub gate_open: bool,
    pub gate_deposit: bool,
}
#[derive(BorshSerialize, Clone, Copy)]
pub struct EntryTerms {
    pub may_open: bool,
    pub may_deposit: bool,
    pub expires_ts: i64,
}
/// Mirrors `brink_venue::MAX_WALK_DAYS`: midnights one instruction walks before `touch` reports a partial step.
pub const MAX_WALK_DAYS: u32 = 60;
#[derive(BorshDeserialize, Debug)]
pub struct Venue {
    pub version: u8,
    pub authority: Pubkey,
    pub benchmark: Pubkey,
    pub usdc_mint: Pubkey,
    pub reserve: Pubkey,
    pub receipt_mint: Pubkey,
    pub index_e18: u128,
    pub bench_accrual_e18: u128,
    pub tail_bps: u128,
    pub last_ts: i64,
    pub principal: u64,
    pub receipts: u64,
    pub funded: u64,
    pub paused: bool,
    pub bump: u8,
    pub anchor_index_e18: u128,
    pub anchor_day: u32,
    pub anchor_accrual: u128,
    pub reserved: [u8; 28],
}
#[derive(BorshSerialize, BorshDeserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReserveParams {
    pub working_bps: u16,
    pub band_bps: u16,
    pub max_place_bps: u16,
    pub min_interval_slots: u64,
    pub bounty_cap: u64,
}
#[derive(BorshDeserialize, Debug)]
pub struct PoolReserve {
    pub version: u8,
    pub pool: Pubkey,
    pub venue: Pubkey,
    pub receipt_mint: Pubkey,
    pub receipts: Pubkey,
    pub params: ReserveParams,
    pub last_rebalance_slot: u64,
    pub yield_realised: u64,
    pub bounties_paid: u64,
    pub placed_lifetime: u64,
    pub recalled_lifetime: u64,
    pub rebalances: u32,
    pub paused: bool,
    pub bump: u8,
    pub last_place_slot: u64,
    pub reserved: [u8; 56],
}
#[derive(BorshDeserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeriesStatus {
    Subscribing,
    Hedged,
    Settled,
    Cancelled,
}
#[derive(BorshDeserialize, Debug)]
pub struct Series {
    pub version: u8,
    pub authority: Pubkey,
    pub pool: Pubkey,
    pub benchmark: Pubkey,
    pub venue: Pubkey,
    pub usdc_mint: Pubkey,
    pub share_mint: Pubkey,
    pub trader: Pubkey,
    pub usdc_account: Pubkey,
    pub receipts_account: Pubkey,
    pub series_id: u16,
    pub tenor: u8,
    pub status: SeriesStatus,
    pub created_ts: i64,
    pub subscribe_until_ts: i64,
    pub hedged_ts: i64,
    pub matures_ts: i64,
    pub deposits: u64,
    pub cap: u64,
    pub min_total: u64,
    pub fee_bp: u16,
    pub fixed_bp: u16,
    pub notional: u64,
    pub collateral: u64,
    pub placed: u64,
    pub receipts: u64,
    pub swap: Pubkey,
    pub client_seed: u64,
    pub settled_assets: u64,
    pub fee_paid: u64,
    pub bump: u8,
    pub trader_bump: u8,
    pub min_fixed_bp: u16,
    pub reserved: [u8; 62],
}
#[derive(BorshSerialize, Clone, Copy)]
pub struct CreateSeriesArgs {
    pub series_id: u16,
    pub tenor: u8,
    pub subscribe_seconds: i64,
    pub cap: u64,
    pub min_total: u64,
    pub fee_bp: u16,
    pub client_seed: u64,
    pub min_fixed_bp: u16,
}
#[derive(BorshSerialize, BorshDeserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    BrinkPool,
    BrinkVenue,
}
#[derive(BorshDeserialize, Debug)]
pub struct Market {
    pub version: u8,
    pub authority: Pubkey,
    pub source_kind: SourceKind,
    pub source: Pubkey,
    pub sy_mint: Pubkey,
    pub sy_vault: Pubkey,
    pub fee_account: Pubkey,
    pub pt_mint: Pubkey,
    pub yt_mint: Pubkey,
    pub maturity_ts: i64,
    pub created_ts: i64,
    pub min_mint: u64,
    pub rate_at_maturity_e18: u128,
    pub settled: bool,
    pub sy_locked: u64,
    pub py_supply: u64,
    pub sy_for_pt: u64,
    pub sy_for_yt: u64,
    pub pt_at_settle: u64,
    pub yt_at_settle: u64,
    pub fees: u64,
    pub bump: u8,
    pub rate_origin_e18: u128,
    pub reserved: [u8; 48],
}
#[derive(BorshSerialize, Clone, Copy)]
pub struct CreateMarketArgs {
    pub source_kind: SourceKind,
    pub maturity_ts: i64,
    pub min_mint: u64,
}

// ---- PDAs ----
pub fn list_pda(pool: &Pubkey) -> Pubkey {
    pda(&[b"list", pool.as_ref()], &ALLOWLIST)
}
pub fn entry_pda(pool: &Pubkey, wallet: &Pubkey) -> Pubkey {
    pda(&[b"entry", pool.as_ref(), wallet.as_ref()], &ALLOWLIST)
}
pub fn venue_pda(benchmark: &Pubkey) -> Pubkey {
    pda(&[b"venue", benchmark.as_ref()], &VENUE)
}
pub fn receipt_mint_pda(venue: &Pubkey) -> Pubkey {
    pda(&[b"receipt", venue.as_ref()], &VENUE)
}
pub fn reserve_pda(venue: &Pubkey) -> Pubkey {
    pda(&[b"reserve", venue.as_ref()], &VENUE)
}
pub fn pool_reserve_pda(pool: &Pubkey) -> Pubkey {
    pda(&[b"reserve", pool.as_ref()], &SWAP_AMM)
}
pub fn pool_receipts_pda(pool: &Pubkey) -> Pubkey {
    pda(&[b"receipts", pool.as_ref()], &SWAP_AMM)
}
pub fn series_pda(pool: &Pubkey, id: u16) -> Pubkey {
    pda(&[b"series", pool.as_ref(), &id.to_le_bytes()], &VAULTS)
}
pub fn series_trader(series: &Pubkey) -> Pubkey {
    pda(&[b"trader", series.as_ref()], &VAULTS)
}
pub fn series_shares(series: &Pubkey) -> Pubkey {
    pda(&[b"shares", series.as_ref()], &VAULTS)
}
pub fn series_usdc(series: &Pubkey) -> Pubkey {
    pda(&[b"usdc", series.as_ref()], &VAULTS)
}
pub fn series_receipts(series: &Pubkey) -> Pubkey {
    pda(&[b"receipts", series.as_ref()], &VAULTS)
}
pub fn market_pda(sy_mint: &Pubkey, maturity_ts: i64) -> Pubkey {
    pda(
        &[b"market", sy_mint.as_ref(), &maturity_ts.to_le_bytes()],
        &SPLIT,
    )
}
pub fn market_sy_vault(market: &Pubkey) -> Pubkey {
    pda(&[b"sy", market.as_ref()], &SPLIT)
}
pub fn market_fees(market: &Pubkey) -> Pubkey {
    pda(&[b"fees", market.as_ref()], &SPLIT)
}
pub fn market_pt(market: &Pubkey) -> Pubkey {
    pda(&[b"pt", market.as_ref()], &SPLIT)
}
pub fn market_yt(market: &Pubkey) -> Pubkey {
    pda(&[b"yt", market.as_ref()], &SPLIT)
}

/// Loads the four product programs into a booted environment and makes the harness authority their upgrade
/// authority (every `create_*` is upgrade-authority gated).
pub fn load_products(env: &mut Env) {
    let d = deploy_dir();
    for (id, file) in [
        (ALLOWLIST, "brink_allowlist.so"),
        (VENUE, "brink_venue.so"),
        (VAULTS, "brink_vaults.so"),
        (SPLIT, "brink_split.so"),
    ] {
        env.svm
            .add_program_from_file(id, d.join(file))
            .unwrap_or_else(|_| panic!("{file}, run ./build-sbf.sh first"));
        set_upgrade_authority(&mut env.svm, &id, Some(&env.authority.pubkey()));
    }
}

/// `setup()` plus the product programs.
pub fn setup_products() -> Env {
    let mut env = setup();
    load_products(&mut env);
    env
}

impl Env {
    // ---- allowlist ----
    pub fn create_list_ix(
        &self,
        pool: &Pubkey,
        signer: &Pubkey,
        args: &CreateListArgs,
    ) -> Instruction {
        Instruction {
            program_id: ALLOWLIST,
            accounts: [
                vec![
                    ro(self.global),
                    sig(*signer),
                    ro(*pool),
                    rw(list_pda(pool)),
                    sigw(self.payer.pubkey()),
                    ro(SYSTEM),
                ],
                evt(ALLOWLIST).to_vec(),
            ]
            .concat(),
            data: data("create_list", args),
        }
    }
    pub fn add_entry_ix(
        &self,
        pool: &Pubkey,
        manager: &Pubkey,
        wallet: &Pubkey,
        args: &EntryTerms,
    ) -> Instruction {
        Instruction {
            program_id: ALLOWLIST,
            accounts: [
                vec![
                    rw(list_pda(pool)),
                    sig(*manager),
                    ro(*wallet),
                    rw(entry_pda(pool, wallet)),
                    sigw(self.payer.pubkey()),
                    ro(SYSTEM),
                ],
                evt(ALLOWLIST).to_vec(),
            ]
            .concat(),
            data: data("add_entry", args),
        }
    }
    pub fn update_entry_ix(
        &self,
        pool: &Pubkey,
        manager: &Pubkey,
        wallet: &Pubkey,
        args: &EntryTerms,
    ) -> Instruction {
        Instruction {
            program_id: ALLOWLIST,
            accounts: [
                vec![
                    ro(list_pda(pool)),
                    sig(*manager),
                    rw(entry_pda(pool, wallet)),
                ],
                evt(ALLOWLIST).to_vec(),
            ]
            .concat(),
            data: data("update_entry", args),
        }
    }
    pub fn remove_entry_ix(&self, pool: &Pubkey, manager: &Pubkey, wallet: &Pubkey) -> Instruction {
        Instruction {
            program_id: ALLOWLIST,
            accounts: [
                vec![
                    rw(list_pda(pool)),
                    sigw(*manager),
                    rw(entry_pda(pool, wallet)),
                ],
                evt(ALLOWLIST).to_vec(),
            ]
            .concat(),
            data: data("remove_entry", &()),
        }
    }
    pub fn set_gates_ix(
        &self,
        pool: &Pubkey,
        manager: &Pubkey,
        open: bool,
        deposit: bool,
        frozen: bool,
    ) -> Instruction {
        Instruction {
            program_id: ALLOWLIST,
            accounts: [
                vec![rw(list_pda(pool)), sig(*manager)],
                evt(ALLOWLIST).to_vec(),
            ]
            .concat(),
            data: data("set_gates", &(open, deposit, frozen)),
        }
    }
    pub fn set_manager_ix(
        &self,
        pool: &Pubkey,
        manager: &Pubkey,
        new_manager: Pubkey,
    ) -> Instruction {
        Instruction {
            program_id: ALLOWLIST,
            accounts: [
                vec![rw(list_pda(pool)), sig(*manager)],
                evt(ALLOWLIST).to_vec(),
            ]
            .concat(),
            data: data("set_manager", &new_manager),
        }
    }
    pub fn accept_manager_ix(&self, pool: &Pubkey, pending: &Pubkey) -> Instruction {
        Instruction {
            program_id: ALLOWLIST,
            accounts: [
                vec![rw(list_pda(pool)), sig(*pending)],
                evt(ALLOWLIST).to_vec(),
            ]
            .concat(),
            data: data("accept_manager", &()),
        }
    }
    /// Appends the list and the actor's entry as remaining accounts, which the AMM forwards to the hook.
    pub fn with_allowlist(&self, mut ix: Instruction, actor: &Pubkey) -> Instruction {
        ix.accounts.push(ro(list_pda(&self.pool)));
        ix.accounts.push(ro(entry_pda(&self.pool, actor)));
        ix
    }

    // ---- venue ----
    pub fn create_venue_ix(&self, benchmark: &Pubkey, signer: &Pubkey) -> Instruction {
        let venue = venue_pda(benchmark);
        Instruction {
            program_id: VENUE,
            accounts: [
                vec![
                    sigw(*signer),
                    ro(program_data(&VENUE)),
                    ro(*benchmark),
                    ro(USDC_DEVNET),
                    rw(venue),
                    rw(receipt_mint_pda(&venue)),
                    rw(reserve_pda(&venue)),
                    ro(TOKEN_PROGRAM),
                    ro(SYSTEM),
                ],
                evt(VENUE).to_vec(),
            ]
            .concat(),
            data: data("create_venue", &()),
        }
    }
    fn venue_move_ix(
        &self,
        name: &str,
        benchmark: &Pubkey,
        owner: &Pubkey,
        args: &(u64, u64),
    ) -> Instruction {
        let venue = venue_pda(benchmark);
        let receipt = receipt_mint_pda(&venue);
        Instruction {
            program_id: VENUE,
            accounts: [
                vec![
                    rw(venue),
                    ro(*benchmark),
                    sig(*owner),
                    rw(ata(owner, &USDC_DEVNET)),
                    rw(ata(owner, &receipt)),
                    rw(reserve_pda(&venue)),
                    rw(receipt),
                    ro(USDC_DEVNET),
                    ro(TOKEN_PROGRAM),
                ],
                evt(VENUE).to_vec(),
            ]
            .concat(),
            data: data(name, args),
        }
    }
    pub fn place_ix(
        &self,
        benchmark: &Pubkey,
        owner: &Pubkey,
        amount: u64,
        min_receipts: u64,
    ) -> Instruction {
        self.venue_move_ix("place", benchmark, owner, &(amount, min_receipts))
    }
    pub fn venue_redeem_ix(
        &self,
        benchmark: &Pubkey,
        owner: &Pubkey,
        receipts: u64,
        min_amount: u64,
    ) -> Instruction {
        self.venue_move_ix("redeem", benchmark, owner, &(receipts, min_amount))
    }
    pub fn fund_ix(&self, benchmark: &Pubkey, funder: &Pubkey, amount: u64) -> Instruction {
        let venue = venue_pda(benchmark);
        Instruction {
            program_id: VENUE,
            accounts: [
                vec![
                    rw(venue),
                    sig(*funder),
                    rw(ata(funder, &USDC_DEVNET)),
                    rw(reserve_pda(&venue)),
                    ro(USDC_DEVNET),
                    ro(TOKEN_PROGRAM),
                ],
                evt(VENUE).to_vec(),
            ]
            .concat(),
            data: data("fund", &amount),
        }
    }
    // ---- Stacked Treasuries reserve on the AMM pool ----
    pub fn enable_reserve_ix(
        &self,
        benchmark: &Pubkey,
        signer: &Pubkey,
        params: &ReserveParams,
    ) -> Instruction {
        let venue = venue_pda(benchmark);
        Instruction {
            program_id: SWAP_AMM,
            accounts: [
                vec![
                    ro(self.global),
                    sig(*signer),
                    sigw(self.payer.pubkey()),
                    rw(self.pool),
                    ro(venue),
                    ro(receipt_mint_pda(&venue)),
                    rw(pool_reserve_pda(&self.pool)),
                    rw(pool_receipts_pda(&self.pool)),
                    ro(USDC_DEVNET),
                    ro(TOKEN_PROGRAM),
                    ro(SYSTEM),
                ],
                evt(SWAP_AMM).to_vec(),
            ]
            .concat(),
            data: data("admin_enable_reserve", params),
        }
    }
    pub fn set_reserve_ix(
        &self,
        signer: &Pubkey,
        params: &ReserveParams,
        paused: bool,
    ) -> Instruction {
        Instruction {
            program_id: SWAP_AMM,
            accounts: [
                vec![
                    ro(self.global),
                    sig(*signer),
                    rw(self.pool),
                    rw(pool_reserve_pda(&self.pool)),
                ],
                evt(SWAP_AMM).to_vec(),
            ]
            .concat(),
            data: data("admin_set_reserve", &(*params, paused)),
        }
    }
    pub fn rebalance_reserve_ix(&self, benchmark: &Pubkey, cranker: &Pubkey) -> Instruction {
        let venue = venue_pda(benchmark);
        Instruction {
            program_id: SWAP_AMM,
            accounts: [
                vec![
                    ro(self.global),
                    rw(self.pool),
                    rw(pool_reserve_pda(&self.pool)),
                    rw(self.vault),
                    rw(pool_receipts_pda(&self.pool)),
                    rw(venue),
                    ro(*benchmark),
                    rw(reserve_pda(&venue)),
                    rw(receipt_mint_pda(&venue)),
                    ro(USDC_DEVNET),
                    sig(*cranker),
                    rw(ata(cranker, &USDC_DEVNET)),
                    ro(event_authority(&VENUE)),
                    ro(VENUE),
                    ro(TOKEN_PROGRAM),
                ],
                evt(SWAP_AMM).to_vec(),
            ]
            .concat(),
            data: data("crank_rebalance_reserve", &()),
        }
    }
    /// The reserve set a payout path takes as remaining accounts for an inline recall, in the program's order.
    pub fn reserve_remaining(&self, benchmark: &Pubkey) -> Vec<AccountMeta> {
        let venue = venue_pda(benchmark);
        vec![
            rw(pool_reserve_pda(&self.pool)),
            rw(pool_receipts_pda(&self.pool)),
            rw(venue),
            ro(*benchmark),
            rw(reserve_pda(&venue)),
            rw(receipt_mint_pda(&venue)),
            ro(event_authority(&VENUE)),
            ro(VENUE),
        ]
    }
    /// An LP deposit on a pool with an active reserve, which is priced through the venue (external scan 2,
    /// finding 6) and so carries the reserve set.
    pub fn deposit_with_reserve_ix(
        &self,
        benchmark: &Pubkey,
        lp: &Pubkey,
        amount: u64,
        min_shares: u64,
    ) -> Instruction {
        let mut ix = self.deposit_ix(lp, amount, min_shares);
        ix.accounts.extend(self.reserve_remaining(benchmark));
        ix
    }
    /// An LP withdrawal that recalls from the reserve inline when the working balance is short.
    pub fn withdraw_with_reserve_ix(
        &self,
        benchmark: &Pubkey,
        lp: &Pubkey,
        shares: u64,
        min_amount: u64,
    ) -> Instruction {
        let mut ix = self.withdraw_ix(lp, shares, min_amount);
        ix.accounts.extend(self.reserve_remaining(benchmark));
        ix
    }
    pub fn touch_ix(&self, benchmark: &Pubkey) -> Instruction {
        Instruction {
            program_id: VENUE,
            accounts: [
                vec![rw(venue_pda(benchmark)), ro(*benchmark)],
                evt(VENUE).to_vec(),
            ]
            .concat(),
            data: data("touch", &()),
        }
    }
    /// Touches the venue until its index is at the clock: one call per `MAX_WALK_DAYS` of gap.
    pub fn touch_until_current(&mut self, benchmark: &Pubkey) -> u32 {
        let mut touches = 0;
        loop {
            let ix = self.touch_ix(benchmark);
            self.must(&[ix], &[]);
            touches += 1;
            let v: Venue = self.acct("Venue", &venue_pda(benchmark));
            if v.last_ts >= self.clock().unix_timestamp {
                return touches;
            }
        }
    }
    pub fn set_paused_ix(&self, benchmark: &Pubkey, signer: &Pubkey, paused: bool) -> Instruction {
        Instruction {
            program_id: VENUE,
            accounts: [
                vec![rw(venue_pda(benchmark)), sig(*signer)],
                evt(VENUE).to_vec(),
            ]
            .concat(),
            data: data("set_paused", &paused),
        }
    }

    // ---- vaults ----
    pub fn create_series_ix(&self, signer: &Pubkey, args: &CreateSeriesArgs) -> Instruction {
        let venue = venue_pda(&self.benchmark);
        let series = series_pda(&self.pool, args.series_id);
        Instruction {
            program_id: VAULTS,
            accounts: [
                vec![
                    sigw(*signer),
                    ro(program_data(&VAULTS)),
                    ro(self.pool),
                    ro(venue),
                    ro(USDC_DEVNET),
                    ro(receipt_mint_pda(&venue)),
                    rw(series),
                    rw(series_trader(&series)),
                    rw(series_shares(&series)),
                    rw(series_usdc(&series)),
                    rw(series_receipts(&series)),
                    ro(TOKEN_PROGRAM),
                    ro(SYSTEM),
                ],
                evt(VAULTS).to_vec(),
            ]
            .concat(),
            data: data("create_series", args),
        }
    }
    pub fn series_deposit_ix(&self, id: u16, depositor: &Pubkey, amount: u64) -> Instruction {
        let series = series_pda(&self.pool, id);
        Instruction {
            program_id: VAULTS,
            accounts: [
                vec![
                    rw(series),
                    sig(*depositor),
                    rw(ata(depositor, &USDC_DEVNET)),
                    rw(ata(depositor, &series_shares(&series))),
                    rw(series_usdc(&series)),
                    rw(series_shares(&series)),
                    ro(USDC_DEVNET),
                    ro(TOKEN_PROGRAM),
                ],
                evt(VAULTS).to_vec(),
            ]
            .concat(),
            data: data("deposit", &amount),
        }
    }
    fn series_holder_ix(&self, name: &str, id: u16, holder: &Pubkey, shares: u64) -> Instruction {
        let series = series_pda(&self.pool, id);
        Instruction {
            program_id: VAULTS,
            accounts: [
                vec![
                    rw(series),
                    ro(series_trader(&series)),
                    sig(*holder),
                    rw(ata(holder, &USDC_DEVNET)),
                    rw(ata(holder, &series_shares(&series))),
                    rw(series_usdc(&series)),
                    rw(series_shares(&series)),
                    ro(USDC_DEVNET),
                    ro(TOKEN_PROGRAM),
                ],
                evt(VAULTS).to_vec(),
            ]
            .concat(),
            data: data(name, &shares),
        }
    }
    pub fn series_withdraw_ix(&self, id: u16, holder: &Pubkey, shares: u64) -> Instruction {
        self.series_holder_ix("withdraw", id, holder, shares)
    }
    pub fn series_redeem_ix(&self, id: u16, holder: &Pubkey, shares: u64) -> Instruction {
        self.series_holder_ix("redeem", id, holder, shares)
    }
    pub fn series_swap(&self, id: u16, client_seed: u64) -> Pubkey {
        let series = series_pda(&self.pool, id);
        let trader = series_trader(&series);
        pda(
            &[
                b"swap",
                self.pool.as_ref(),
                trader.as_ref(),
                &client_seed.to_le_bytes(),
            ],
            &SWAP_AMM,
        )
    }
    pub fn hedge_ix(
        &self,
        id: u16,
        client_seed: u64,
        cranker: &Pubkey,
        limit_rate_bp: u16,
    ) -> Instruction {
        let series = series_pda(&self.pool, id);
        let venue = venue_pda(&self.benchmark);
        Instruction {
            program_id: VAULTS,
            accounts: [
                vec![
                    rw(series),
                    rw(series_trader(&series)),
                    sig(*cranker),
                    ro(self.global),
                    rw(self.pool),
                    ro(self.benchmark),
                    rw(self.series_swap(id, client_seed)),
                    rw(self.vault),
                    ro(event_authority(&SWAP_AMM)),
                    ro(SWAP_AMM),
                    rw(venue),
                    rw(reserve_pda(&venue)),
                    rw(receipt_mint_pda(&venue)),
                    ro(event_authority(&VENUE)),
                    ro(VENUE),
                    rw(series_usdc(&series)),
                    rw(series_receipts(&series)),
                    ro(USDC_DEVNET),
                    ro(TOKEN_PROGRAM),
                    ro(SYSTEM),
                ],
                evt(VAULTS).to_vec(),
            ]
            .concat(),
            data: data("hedge", &limit_rate_bp),
        }
    }
    pub fn series_cancel_ix(&self, id: u16, signer: &Pubkey) -> Instruction {
        Instruction {
            program_id: VAULTS,
            accounts: [
                vec![rw(series_pda(&self.pool, id)), sig(*signer)],
                evt(VAULTS).to_vec(),
            ]
            .concat(),
            data: data("cancel", &()),
        }
    }
    pub fn series_settle_ix(&self, id: u16, client_seed: u64, cranker: &Pubkey) -> Instruction {
        let series = series_pda(&self.pool, id);
        let venue = venue_pda(&self.benchmark);
        let t = self.treasury_owner.pubkey();
        Instruction {
            program_id: VAULTS,
            accounts: [
                vec![
                    rw(series),
                    rw(series_trader(&series)),
                    sig(*cranker),
                    ro(self.global),
                    rw(self.pool),
                    ro(self.benchmark),
                    rw(self.series_swap(id, client_seed)),
                    rw(self.vault),
                    ro(event_authority(&SWAP_AMM)),
                    ro(SWAP_AMM),
                    rw(ata(&t, &USDC_DEVNET)),
                    rw(ata(&buyback_owner(&t), &USDC_DEVNET)),
                    rw(venue),
                    rw(reserve_pda(&venue)),
                    rw(receipt_mint_pda(&venue)),
                    ro(event_authority(&VENUE)),
                    ro(VENUE),
                    rw(series_usdc(&series)),
                    rw(series_receipts(&series)),
                    ro(USDC_DEVNET),
                    ro(TOKEN_PROGRAM),
                ],
                evt(VAULTS).to_vec(),
            ]
            .concat(),
            data: data("settle", &()),
        }
    }

    // ---- split ----
    pub fn create_market_ix(
        &self,
        signer: &Pubkey,
        source: &Pubkey,
        benchmark: Option<&Pubkey>,
        sy_mint: &Pubkey,
        args: &CreateMarketArgs,
    ) -> Instruction {
        let market = market_pda(sy_mint, args.maturity_ts);
        Instruction {
            program_id: SPLIT,
            accounts: [
                vec![
                    sigw(*signer),
                    ro(program_data(&SPLIT)),
                    ro(*source),
                    // Anchor optional account: the program id stands for `None`.
                    ro(*benchmark.unwrap_or(&SPLIT)),
                    ro(*sy_mint),
                    rw(market),
                    rw(market_sy_vault(&market)),
                    rw(market_fees(&market)),
                    rw(market_pt(&market)),
                    rw(market_yt(&market)),
                    ro(TOKEN_PROGRAM),
                    ro(SYSTEM),
                ],
                evt(SPLIT).to_vec(),
            ]
            .concat(),
            data: data("create_market", args),
        }
    }
    pub fn mint_py_ix(
        &self,
        market: &Pubkey,
        sy_mint: &Pubkey,
        owner: &Pubkey,
        sy_amount: u64,
        min_py: u64,
    ) -> Instruction {
        Instruction {
            program_id: SPLIT,
            accounts: [
                vec![
                    rw(*market),
                    sig(*owner),
                    rw(ata(owner, sy_mint)),
                    rw(ata(owner, &market_pt(market))),
                    rw(ata(owner, &market_yt(market))),
                    rw(market_sy_vault(market)),
                    rw(market_fees(market)),
                    rw(market_pt(market)),
                    rw(market_yt(market)),
                    ro(*sy_mint),
                    ro(TOKEN_PROGRAM),
                ],
                evt(SPLIT).to_vec(),
            ]
            .concat(),
            data: data("mint_py", &(sy_amount, min_py)),
        }
    }
    pub fn redeem_py_ix(
        &self,
        market: &Pubkey,
        sy_mint: &Pubkey,
        owner: &Pubkey,
        py: u64,
        min_sy: u64,
    ) -> Instruction {
        Instruction {
            program_id: SPLIT,
            accounts: [
                vec![
                    rw(*market),
                    sig(*owner),
                    rw(ata(owner, sy_mint)),
                    rw(ata(owner, &market_pt(market))),
                    rw(ata(owner, &market_yt(market))),
                    rw(market_sy_vault(market)),
                    rw(market_pt(market)),
                    rw(market_yt(market)),
                    ro(*sy_mint),
                    ro(TOKEN_PROGRAM),
                ],
                evt(SPLIT).to_vec(),
            ]
            .concat(),
            data: data("redeem_py", &(py, min_sy)),
        }
    }
    pub fn market_settle_ix(
        &self,
        market: &Pubkey,
        source: &Pubkey,
        benchmark: Option<&Pubkey>,
        cranker: &Pubkey,
    ) -> Instruction {
        Instruction {
            program_id: SPLIT,
            accounts: [
                vec![
                    rw(*market),
                    ro(*source),
                    ro(*benchmark.unwrap_or(&SPLIT)),
                    ro(market_sy_vault(market)),
                    ro(market_pt(market)),
                    ro(market_yt(market)),
                    sig(*cranker),
                ],
                evt(SPLIT).to_vec(),
            ]
            .concat(),
            data: data("settle", &()),
        }
    }
    pub fn redeem_side_ix(
        &self,
        principal: bool,
        market: &Pubkey,
        sy_mint: &Pubkey,
        owner: &Pubkey,
        amount: u64,
    ) -> Instruction {
        let side = if principal {
            market_pt(market)
        } else {
            market_yt(market)
        };
        Instruction {
            program_id: SPLIT,
            accounts: [
                vec![
                    rw(*market),
                    sig(*owner),
                    rw(ata(owner, sy_mint)),
                    rw(side),
                    rw(ata(owner, &side)),
                    rw(market_sy_vault(market)),
                    ro(*sy_mint),
                    ro(TOKEN_PROGRAM),
                ],
                evt(SPLIT).to_vec(),
            ]
            .concat(),
            data: data(if principal { "redeem_pt" } else { "redeem_yt" }, &amount),
        }
    }
    pub fn sweep_split_fees_ix(&self, market: &Pubkey, sy_mint: &Pubkey) -> Instruction {
        let t = self.treasury_owner.pubkey();
        let b = buyback_owner(&t);
        Instruction {
            program_id: SPLIT,
            accounts: [
                vec![
                    ro(*market),
                    ro(self.global),
                    ro(ata(&t, &USDC_DEVNET)),
                    ro(ata(&b, &USDC_DEVNET)),
                    rw(ata(&t, sy_mint)),
                    rw(ata(&b, sy_mint)),
                    rw(market_fees(market)),
                    ro(*sy_mint),
                    ro(TOKEN_PROGRAM),
                ],
                evt(SPLIT).to_vec(),
            ]
            .concat(),
            data: data("sweep_fees", &()),
        }
    }
    /// A funded actor with USDC and an account on `mint`.
    pub fn actor_with(&mut self, usdc: u64, mints: &[Pubkey]) -> Keypair {
        let k = self.new_actor(usdc, false);
        for m in mints {
            self.create_ata(&k.pubkey(), m);
        }
        k
    }
}
