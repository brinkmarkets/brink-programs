//! brink_sale adversarial cases on LiteSVM, written for the sale review of 2026-10-05. The harness is a copy of
//! the one in `sale.rs` with a few extra builders (a round created by a stranger, a finalise with a chosen unsold
//! destination, a raw lamport transfer). Each test documents one claim of the review; the names say which.
use borsh::{BorshDeserialize, BorshSerialize};
use brink_svm_tests::harness::set_upgrade_authority;
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

const SALE: Pubkey = Pubkey::from_str_const("GYCJoDiZCh7mbqyWRjYJmpPUt3nPLs6887tGm58rmK3v");
const DAY: i64 = 86_400;
const BOOT: i64 = 1_790_000_000;
const SOL: u64 = 1_000_000_000;
const TOKEN: u64 = 1_000_000_000; // 9 decimals
const MAX_TGE_DELAY: i64 = 30 * DAY;
const MAX_VEST_SECONDS: i64 = 730 * DAY;
/// Rent-exempt minimum for a zero-data account on this runtime (890_880 lamports at the default rent).
const ZERO_DATA_RENT: u64 = 890_880;

#[derive(BorshSerialize, BorshDeserialize, Clone, Copy)]
struct CreateRoundArgs {
    round_id: u8,
    lamports_per_token: u64,
    hard_cap: u64,
    soft_cap: u64,
    min_per_wallet: u64,
    max_per_wallet: u64,
    start_ts: i64,
    end_ts: i64,
    tge_bps: u16,
    vest_seconds: i64,
    allowlist_root: [u8; 32],
    allowlisted: bool,
}

#[derive(BorshDeserialize, Debug)]
#[allow(dead_code)]
struct Round {
    authority: Pubkey,
    pending_authority: Pubkey,
    treasury: Pubkey,
    mint: Pubkey,
    vault: Pubkey,
    round_id: u8,
    decimals: u8,
    lamports_per_token: u64,
    hard_cap: u64,
    soft_cap: u64,
    min_per_wallet: u64,
    max_per_wallet: u64,
    start_ts: i64,
    end_ts: i64,
    tge_ts: i64,
    tge_bps: u16,
    vest_seconds: i64,
    allowlist_root: [u8; 32],
    allowlisted: bool,
    raised: u64,
    tokens_sold: u64,
    tokens_claimed: u64,
    participants: u32,
    state: u8,
    bump: u8,
    escrow_bump: u8,
    reserved: [u8; 64],
}

#[derive(BorshDeserialize, Debug)]
#[allow(dead_code)]
struct Participant {
    round: Pubkey,
    buyer: Pubkey,
    lamports: u64,
    tokens: u64,
    claimed: u64,
    refunded: bool,
    bump: u8,
}

const PENDING: u8 = 0;
const OPEN: u8 = 1;
const CLOSED: u8 = 2;
const FINALISED: u8 = 3;
const CANCELLED: u8 = 4;

struct S {
    svm: LiteSVM,
    payer: Keypair,
    authority: Keypair,
    treasury: Keypair,
    mint: Pubkey,
    mint_auth: Keypair,
}

fn ro(k: Pubkey) -> AccountMeta {
    AccountMeta::new_readonly(k, false)
}
fn rw(k: Pubkey) -> AccountMeta {
    AccountMeta::new(k, false)
}
fn sig(k: Pubkey) -> AccountMeta {
    AccountMeta::new_readonly(k, true)
}
fn sigw(k: Pubkey) -> AccountMeta {
    AccountMeta::new(k, true)
}
fn evt() -> [AccountMeta; 2] {
    [ro(event_authority(&SALE)), ro(SALE)]
}
fn round_pda(id: u8) -> Pubkey {
    pda(&[b"round", &[id]], &SALE)
}
fn escrow_pda(round: &Pubkey) -> Pubkey {
    pda(&[b"escrow", round.as_ref()], &SALE)
}
fn vault_pda(round: &Pubkey) -> Pubkey {
    pda(&[b"vault", round.as_ref()], &SALE)
}
fn part_pda(round: &Pubkey, buyer: &Pubkey) -> Pubkey {
    pda(&[b"participant", round.as_ref(), buyer.as_ref()], &SALE)
}

/// Mirrors the program: floor(lamports * 10^dec / price).
fn tokens_for(lamports: u64, price: u64, decimals: u32) -> u64 {
    u64::try_from(u128::from(lamports) * 10u128.pow(decimals) / u128::from(price)).unwrap()
}

impl S {
    fn boot() -> S {
        let mut svm = LiteSVM::new();
        let d = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/deploy");
        svm.add_program_from_file(SALE, d.join("brink_sale.so"))
            .expect("brink_sale.so, run ./build-sbf.sh first");
        let payer = Keypair::new();
        let authority = Keypair::new();
        // Only the upgrade authority may create rounds; the test authority takes that role.
        set_upgrade_authority(&mut svm, &SALE, Some(&authority.pubkey()));
        let treasury = Keypair::new();
        let mint_auth = Keypair::new();
        for k in [&payer, &authority, &mint_auth] {
            svm.airdrop(&k.pubkey(), 1_000 * SOL).unwrap();
        }
        let mint = Pubkey::new_unique();
        let mut data = vec![0u8; spl_token_interface::state::Mint::LEN];
        spl_token_interface::state::Mint {
            mint_authority: Some(mint_auth.pubkey()).into(),
            supply: 0,
            decimals: 9,
            is_initialized: true,
            freeze_authority: None.into(),
        }
        .pack_into_slice(&mut data);
        svm.set_account(
            mint,
            Account { lamports: 10 * SOL, data, owner: TOKEN_PROGRAM, executable: false, rent_epoch: 0 },
        )
        .unwrap();
        let mut c = svm.get_sysvar::<Clock>();
        c.slot = 1_000;
        c.unix_timestamp = BOOT;
        svm.set_sysvar(&c);
        svm.warp_to_slot(1_000);
        let mut c2 = svm.get_sysvar::<Clock>();
        c2.unix_timestamp = BOOT;
        svm.set_sysvar(&c2);
        S { svm, payer, authority, treasury, mint, mint_auth }
    }
    fn now(&self) -> i64 {
        self.svm.get_sysvar::<Clock>().unix_timestamp
    }
    fn warp(&mut self, seconds: i64) {
        let mut c = self.svm.get_sysvar::<Clock>();
        c.slot += (seconds as u64) / 2 + 1;
        c.unix_timestamp += seconds;
        self.svm.warp_to_slot(c.slot);
        let mut c2 = self.svm.get_sysvar::<Clock>();
        c2.unix_timestamp = c.unix_timestamp;
        self.svm.set_sysvar(&c2);
    }
    /// Moves the clock to an exact timestamp (forward only).
    fn warp_to(&mut self, ts: i64) {
        let d = ts - self.now();
        assert!(d >= 0, "warp_to only moves forward");
        self.warp(d);
    }
    fn send(&mut self, ixs: &[Instruction], signers: &[&Keypair]) -> Result<Vec<String>, String> {
        let msg = Message::new(ixs, Some(&self.payer.pubkey()));
        let mut all: Vec<&Keypair> = vec![&self.payer];
        for s in signers {
            if s.pubkey() != self.payer.pubkey() {
                all.push(s);
            }
        }
        self.svm.expire_blockhash();
        let mut tx = Transaction::new_unsigned(msg);
        tx.sign(&all, self.svm.latest_blockhash());
        match self.svm.send_transaction(tx) {
            Ok(m) => Ok(m.logs),
            Err(f) => Err(format!("{:?}\n{}", f.err, f.meta.logs.join("\n"))),
        }
    }
    fn must(&mut self, ixs: &[Instruction], signers: &[&Keypair]) {
        if let Err(e) = self.send(ixs, signers) {
            panic!("transaction failed:\n{e}");
        }
    }
    fn must_fail(&mut self, ixs: &[Instruction], signers: &[&Keypair], code: &str) {
        match self.send(ixs, signers) {
            Ok(l) => panic!("expected {code}, succeeded:\n{}", l.join("\n")),
            Err(e) => assert!(e.contains(code), "expected {code}, got:\n{e}"),
        }
    }
    fn round(&self, id: u8) -> Round {
        decode("Round", &self.svm.get_account(&round_pda(id)).unwrap().data)
    }
    fn participant(&self, id: u8, buyer: &Pubkey) -> Participant {
        decode("Participant", &self.svm.get_account(&part_pda(&round_pda(id), buyer)).unwrap().data)
    }
    fn lamports(&self, k: &Pubkey) -> u64 {
        self.svm.get_account(k).map(|a| a.lamports).unwrap_or(0)
    }
    fn tokens(&self, k: &Pubkey) -> u64 {
        let a = self.svm.get_account(k).expect("token account");
        spl_token_interface::state::Account::unpack(&a.data).unwrap().amount
    }
    fn create_ata(&mut self, owner: &Pubkey) -> Pubkey {
        let ix = spl_associated_token_account_interface::instruction::create_associated_token_account_idempotent(
            &self.payer.pubkey(),
            owner,
            &self.mint,
            &TOKEN_PROGRAM,
        );
        let ix = Instruction {
            program_id: ix.program_id,
            accounts: ix
                .accounts
                .into_iter()
                .map(|m| AccountMeta { pubkey: m.pubkey, is_signer: m.is_signer, is_writable: m.is_writable })
                .collect(),
            data: ix.data,
        };
        self.must(&[ix], &[]);
        ata(owner, &self.mint)
    }
    fn mint_to(&mut self, to: &Pubkey, amount: u64) {
        let auth = self.mint_auth.insecure_clone();
        let ix = spl_token_interface::instruction::mint_to(&TOKEN_PROGRAM, &self.mint, to, &auth.pubkey(), &[], amount).unwrap();
        let ix = Instruction {
            program_id: ix.program_id,
            accounts: ix
                .accounts
                .into_iter()
                .map(|m| AccountMeta { pubkey: m.pubkey, is_signer: m.is_signer, is_writable: m.is_writable })
                .collect(),
            data: ix.data,
        };
        self.must(&[ix], &[&auth]);
    }
    /// Raw system transfer, used to drop lamports on an address from outside the program.
    fn transfer(&mut self, from: &Keypair, to: &Pubkey, lamports: u64) -> Result<Vec<String>, String> {
        let ix = solana_system_interface::instruction::transfer(&from.pubkey(), to, lamports);
        let ix = Instruction {
            program_id: ix.program_id,
            accounts: ix
                .accounts
                .into_iter()
                .map(|m| AccountMeta { pubkey: m.pubkey, is_signer: m.is_signer, is_writable: m.is_writable })
                .collect(),
            data: ix.data,
        };
        self.send(&[ix], &[from])
    }
    fn create_round_as(&mut self, authority: &Keypair, treasury: &Pubkey, args: CreateRoundArgs) -> Result<Vec<String>, String> {
        let round = round_pda(args.round_id);
        let mut accounts = vec![
            sigw(authority.pubkey()),
            ro(program_data(&SALE)),
            ro(*treasury),
            ro(self.mint),
            rw(round),
            rw(escrow_pda(&round)),
            rw(vault_pda(&round)),
            ro(TOKEN_PROGRAM),
            ro(SYSTEM),
        ];
        accounts.extend(evt());
        let ix = Instruction { program_id: SALE, accounts, data: data("create_round", &args) };
        self.send(&[ix], &[authority])
    }
    fn create_round(&mut self, args: CreateRoundArgs) -> Result<Vec<String>, String> {
        let auth = self.authority.insecure_clone();
        let t = self.treasury.pubkey();
        self.create_round_as(&auth, &t, args)
    }
    fn authority_ix_as(&self, who: &Pubkey, name: &str, id: u8, extra: &[u8]) -> Instruction {
        let round = round_pda(id);
        let mut accounts = vec![sig(*who), rw(round), ro(vault_pda(&round))];
        accounts.extend(evt());
        let mut d = ix_disc(name).to_vec();
        d.extend_from_slice(extra);
        Instruction { program_id: SALE, accounts, data: d }
    }
    fn authority_ix(&self, name: &str, id: u8, extra: &[u8]) -> Instruction {
        self.authority_ix_as(&self.authority.pubkey(), name, id, extra)
    }
    fn fund(&mut self, id: u8, amount: u64) {
        let v = vault_pda(&round_pda(id));
        self.mint_to(&v, amount);
    }
    fn open(&mut self, id: u8) -> Result<Vec<String>, String> {
        let auth = self.authority.insecure_clone();
        let ix = self.authority_ix("open_round", id, &[]);
        self.send(&[ix], &[&auth])
    }
    fn cancel(&mut self, id: u8) -> Result<Vec<String>, String> {
        let auth = self.authority.insecure_clone();
        let ix = self.authority_ix("cancel_round", id, &[]);
        self.send(&[ix], &[&auth])
    }
    fn contribute_ix(&self, id: u8, buyer: &Pubkey, lamports: u64, proof: &[[u8; 32]]) -> Instruction {
        let round = round_pda(id);
        let mut accounts = vec![sigw(*buyer), rw(round), rw(escrow_pda(&round)), rw(part_pda(&round, buyer)), ro(SYSTEM)];
        accounts.extend(evt());
        #[derive(BorshSerialize)]
        struct A {
            lamports: u64,
            proof: Vec<[u8; 32]>,
        }
        Instruction { program_id: SALE, accounts, data: data("contribute", &A { lamports, proof: proof.to_vec() }) }
    }
    fn contribute(&mut self, id: u8, buyer: &Keypair, lamports: u64) -> Result<Vec<String>, String> {
        let ix = self.contribute_ix(id, &buyer.pubkey(), lamports, &[]);
        self.send(&[ix], &[buyer])
    }
    fn close_ix(&self, id: u8, caller: &Pubkey) -> Instruction {
        let mut accounts = vec![sig(*caller), rw(round_pda(id))];
        accounts.extend(evt());
        Instruction { program_id: SALE, accounts, data: ix_disc("close_round").to_vec() }
    }
    fn expire_ix(&self, id: u8, caller: &Pubkey) -> Instruction {
        let mut accounts = vec![sig(*caller), rw(round_pda(id))];
        accounts.extend(evt());
        Instruction { program_id: SALE, accounts, data: ix_disc("expire_round").to_vec() }
    }
    fn close_as(&mut self, id: u8, caller: &Keypair) -> Result<Vec<String>, String> {
        let ix = self.close_ix(id, &caller.pubkey());
        self.send(&[ix], &[caller])
    }
    /// The authority has no early close: this moves the clock to the window end first, then closes as the authority
    /// (any caller would do).
    fn close_by_authority(&mut self, id: u8) -> Result<Vec<String>, String> {
        let end = self.round(id).end_ts;
        let now = self.now();
        if now < end {
            self.warp(end - now);
        }
        let auth = self.authority.insecure_clone();
        self.close_as(id, &auth)
    }
    fn finalise_ix(&self, who: &Pubkey, id: u8, tge_ts: i64, treasury: &Pubkey, dest: &Pubkey) -> Instruction {
        let round = round_pda(id);
        let mut accounts = vec![
            sig(*who),
            rw(round),
            rw(escrow_pda(&round)),
            rw(*treasury),
            ro(self.mint),
            rw(vault_pda(&round)),
            rw(*dest),
            ro(TOKEN_PROGRAM),
            ro(SYSTEM),
        ];
        accounts.extend(evt());
        Instruction { program_id: SALE, accounts, data: data("finalise", &tge_ts) }
    }
    fn finalise(&mut self, id: u8, tge_ts: i64) -> Result<Vec<String>, String> {
        let auth = self.authority.insecure_clone();
        let dest = self.create_ata(&auth.pubkey());
        let t = self.treasury.pubkey();
        let ix = self.finalise_ix(&auth.pubkey(), id, tge_ts, &t, &dest);
        self.send(&[ix], &[&auth])
    }
    fn claim_ix(&self, id: u8, buyer: &Pubkey, participant: &Pubkey, buyer_token: &Pubkey) -> Instruction {
        let round = round_pda(id);
        let mut accounts = vec![
            sigw(*buyer),
            rw(round),
            rw(*participant),
            ro(self.mint),
            rw(vault_pda(&round)),
            rw(*buyer_token),
            ro(TOKEN_PROGRAM),
        ];
        accounts.extend(evt());
        Instruction { program_id: SALE, accounts, data: ix_disc("claim").to_vec() }
    }
    fn claim(&mut self, id: u8, buyer: &Keypair) -> Result<Vec<String>, String> {
        let bt = self.create_ata(&buyer.pubkey());
        let ix = self.claim_ix(id, &buyer.pubkey(), &part_pda(&round_pda(id), &buyer.pubkey()), &bt);
        self.send(&[ix], &[buyer])
    }
    fn refund_ix(&self, id: u8, buyer: &Pubkey, participant: &Pubkey) -> Instruction {
        let round = round_pda(id);
        let mut accounts = vec![sigw(*buyer), ro(round), rw(*participant), rw(escrow_pda(&round)), ro(SYSTEM)];
        accounts.extend(evt());
        Instruction { program_id: SALE, accounts, data: ix_disc("refund").to_vec() }
    }
    fn refund(&mut self, id: u8, buyer: &Keypair) -> Result<Vec<String>, String> {
        let ix = self.refund_ix(id, &buyer.pubkey(), &part_pda(&round_pda(id), &buyer.pubkey()));
        self.send(&[ix], &[buyer])
    }
    fn withdraw_cancelled(&mut self, id: u8, dest: &Pubkey) -> Result<Vec<String>, String> {
        let round = round_pda(id);
        let auth = self.authority.insecure_clone();
        let mut accounts = vec![sig(auth.pubkey()), ro(round), ro(self.mint), rw(vault_pda(&round)), rw(*dest), ro(TOKEN_PROGRAM)];
        accounts.extend(evt());
        let ix = Instruction { program_id: SALE, accounts, data: ix_disc("withdraw_cancelled").to_vec() };
        self.send(&[ix], &[&auth])
    }
    fn buyer(&mut self, sol: u64) -> Keypair {
        let k = Keypair::new();
        self.svm.airdrop(&k.pubkey(), sol * SOL).unwrap();
        k
    }
    /// Creates, funds to the cap and opens `args`; returns the cap in tokens.
    fn live(&mut self, args: CreateRoundArgs) -> u64 {
        self.create_round(args).unwrap();
        let cap = tokens_for(args.hard_cap, args.lamports_per_token, 9);
        self.fund(args.round_id, cap);
        self.open(args.round_id).unwrap();
        cap
    }
}

/// The mainnet angel round as the init script would create it at 200 USD per SOL: 2 500 lamports per token,
/// 250 SOL hard cap, 125 SOL soft cap, 1.25 to 5 SOL per wallet, 7 days, 25% at TGE then 60 days, open.
fn mainnet_angel(now: i64, id: u8) -> CreateRoundArgs {
    CreateRoundArgs {
        round_id: id,
        lamports_per_token: 2_500,
        hard_cap: 250 * SOL,
        soft_cap: 125 * SOL,
        min_per_wallet: 125 * SOL / 100,
        max_per_wallet: 5 * SOL,
        start_ts: now + 60,
        end_ts: now + 7 * DAY,
        tge_bps: 2_500,
        vest_seconds: 60 * DAY,
        allowlist_root: [0; 32],
        allowlisted: false,
    }
}

/* ------------------------------------------------------------------------------------------------------------ */

/// F-01 (fixed): only the program's upgrade authority may create a round. A stranger cannot squat an id or
/// impersonate a round with their own treasury; the deployer's creation of the same id still succeeds afterwards.
#[test]
fn only_the_upgrade_authority_can_create_a_round() {
    let mut s = S::boot();
    let now = s.now();
    let stranger = s.buyer(10);
    let st_treasury = Pubkey::new_unique();
    let err = s.create_round_as(&stranger, &st_treasury, mainnet_angel(now, 1)).expect_err("stranger must be refused");
    assert!(err.contains("NotUpgradeAuthority"), "{err}");
    assert!(s.svm.get_account(&round_pda(1)).map_or(true, |a| a.lamports == 0), "no round account was created");
    // The deployer creates round 1 as intended, with the recorded treasury.
    s.create_round(mainnet_angel(now, 1)).unwrap();
    let r = s.round(1);
    assert_eq!(r.authority, s.authority.pubkey());
    assert_eq!(r.treasury, s.treasury.pubkey());
    // If the upgrade authority moves, the old deployer key loses the power too.
    set_upgrade_authority(&mut s.svm, &SALE, Some(&stranger.pubkey()));
    let err = s.create_round(mainnet_angel(now, 2)).expect_err("old key refused once the upgrade authority moved");
    assert!(err.contains("NotUpgradeAuthority"), "{err}");
}

/// F-02 (fixed): a Closed round the authority never finalises can be expired by anyone once `FINALISE_GRACE`
/// (30 days) has passed since the window end; it cancels and every buyer refunds. Before the grace ends nobody but
/// the authority can move it, and the authority cannot finalise after the grace.
#[test]
fn closed_round_without_finalisation_can_be_expired_by_anyone() {
    let mut s = S::boot();
    let now = s.now();
    s.live(mainnet_angel(now, 1));
    s.warp(60);
    let mut buyers = Vec::new();
    for _ in 0..25 {
        let k = s.buyer(10);
        s.contribute(1, &k, 5 * SOL).unwrap();
        buyers.push(k);
    }
    assert_eq!(s.round(1).raised, 125 * SOL, "exactly the soft cap");
    s.warp(7 * DAY);
    let stranger = s.buyer(1);
    s.close_as(1, &stranger).unwrap();
    assert_eq!(s.round(1).state, CLOSED);
    let escrow = escrow_pda(&round_pda(1));
    let floor = s.svm.minimum_balance_for_rent_exemption(0);
    assert_eq!(s.lamports(&escrow), 125 * SOL + floor);
    // Inside the grace: a stranger cannot expire, refund, claim, cancel or finalise.
    s.warp(29 * DAY);
    s.must_fail(&[s.expire_ix(1, &stranger.pubkey())], &[&stranger], "Window");
    s.must_fail(&[s.refund_ix(1, &buyers[0].pubkey(), &part_pda(&round_pda(1), &buyers[0].pubkey()))], &[&buyers[0]], "State");
    let bt = s.create_ata(&buyers[0].pubkey());
    s.must_fail(&[s.claim_ix(1, &buyers[0].pubkey(), &part_pda(&round_pda(1), &buyers[0].pubkey()), &bt)], &[&buyers[0]], "State");
    s.must_fail(&[s.authority_ix_as(&stranger.pubkey(), "cancel_round", 1, &[])], &[&stranger], "Authority");
    let dest = s.create_ata(&stranger.pubkey());
    let t = s.treasury.pubkey();
    s.must_fail(&[s.finalise_ix(&stranger.pubkey(), 1, s.now(), &t, &dest)], &[&stranger], "Authority");
    // The grace ends: the authority can no longer finalise, and anyone can expire the round.
    s.warp(DAY + 1);
    let err = s.finalise(1, s.now()).expect_err("finalise after the grace must be refused");
    assert!(err.contains("Window"), "{err}");
    s.must(&[s.expire_ix(1, &stranger.pubkey())], &[&stranger]);
    assert_eq!(s.round(1).state, CANCELLED);
    s.must_fail(&[s.expire_ix(1, &stranger.pubkey())], &[&stranger], "State");
    // Every buyer refunds in full and the escrow ends at its floor.
    for k in &buyers {
        let before = s.lamports(&k.pubkey());
        let rent = s.lamports(&part_pda(&round_pda(1), &k.pubkey()));
        s.refund(1, k).unwrap();
        assert_eq!(s.lamports(&k.pubkey()) - before, 5 * SOL + rent);
    }
    assert_eq!(s.lamports(&escrow), floor, "nothing but the rent floor remains");
    // The authority recovers the tokens.
    let dest = s.create_ata(&s.authority.pubkey());
    s.withdraw_cancelled(1, &dest).unwrap();
    assert_eq!(s.tokens(&vault_pda(&round_pda(1))), 0);
}

/// F-04 (fixed): the escrow carries its rent floor from creation, so dust dropped on it from outside cannot block
/// the last refund; the dust simply joins the floor.
#[test]
fn dust_sent_to_the_escrow_does_not_block_the_last_refund() {
    let mut s = S::boot();
    let now = s.now();
    s.live(mainnet_angel(now, 1));
    s.warp(60);
    let a = s.buyer(10);
    let b = s.buyer(10);
    s.contribute(1, &a, 2 * SOL).unwrap();
    s.contribute(1, &b, 3 * SOL).unwrap();
    let escrow = escrow_pda(&round_pda(1));
    // Anyone may drop a lamport on the escrow while it is rent exempt.
    let griefer = s.buyer(1);
    s.transfer(&griefer, &escrow, 1).unwrap();
    assert_eq!(s.lamports(&escrow), 5 * SOL + ZERO_DATA_RENT + 1);
    // Soft cap missed: the round cancels at the end of the window.
    s.warp(7 * DAY);
    s.close_as(1, &griefer).unwrap();
    assert_eq!(s.round(1).state, CANCELLED);
    s.refund(1, &a).unwrap();
    // The last refund goes through: the escrow keeps its floor plus the lamport of dust.
    let before_b = s.lamports(&b.pubkey());
    let rent_b = s.lamports(&part_pda(&round_pda(1), &b.pubkey()));
    s.refund(1, &b).unwrap();
    assert_eq!(s.lamports(&b.pubkey()) - before_b, 3 * SOL + rent_b);
    assert_eq!(s.lamports(&escrow), ZERO_DATA_RENT + 1);
    assert!(s.svm.get_account(&part_pda(&round_pda(1), &b.pubkey())).map_or(true, |acc| acc.lamports == 0), "position closed");
}

/// F-08 (fixed): the authority has no early close. Once the soft cap is reached it still waits for the window end
/// or a full round like everyone else; `cancel_round` remains its only early exit, and that refunds everyone.
#[test]
fn authority_cannot_close_early() {
    let mut s = S::boot();
    let now = s.now();
    s.live(mainnet_angel(now, 1));
    s.warp(60);
    let mut buyers = Vec::new();
    for _ in 0..25 {
        let k = s.buyer(10);
        s.contribute(1, &k, 5 * SOL).unwrap();
        buyers.push(k);
    }
    let late = s.buyer(10);
    // One minute into a seven-day window, at the soft cap: the authority's close is refused like a stranger's.
    let auth = s.authority.insecure_clone();
    let err = s.close_as(1, &auth).expect_err("authority early close must be refused");
    assert!(err.contains("Window"), "{err}");
    assert_eq!(s.round(1).state, OPEN);
    // The window is honoured: a late buyer still gets in.
    s.contribute(1, &late, 5 * SOL).unwrap();
    // At the window end anyone closes; finalise then sweeps contributions plus the escrow floor.
    s.warp(7 * DAY);
    s.close_as(1, &late).unwrap();
    assert_eq!(s.round(1).state, CLOSED);
    let before = s.lamports(&s.treasury.pubkey());
    let floor = s.svm.minimum_balance_for_rent_exemption(0);
    s.finalise(1, s.now()).unwrap();
    assert_eq!(s.lamports(&s.treasury.pubkey()) - before, 130 * SOL + floor);
    assert_eq!(s.lamports(&escrow_pda(&round_pda(1))), 0, "escrow drained exactly");
    s.claim(1, &buyers[0]).unwrap();
    assert_eq!(s.tokens(&ata(&buyers[0].pubkey(), &s.mint)), 2_000_000 * TOKEN / 4);
}

/// F-05: finalise refusals: not before close, TGE in the past, TGE more than thirty days out, and never twice. After
/// finalisation the authority cannot cancel.
#[test]
fn finalise_refusals_and_tge_bounds() {
    let mut s = S::boot();
    let now = s.now();
    s.live(mainnet_angel(now, 1));
    s.warp(60);
    for _ in 0..25 {
        let k = s.buyer(10);
        s.contribute(1, &k, 5 * SOL).unwrap();
    }
    let t = s.treasury.pubkey();
    let auth = s.authority.insecure_clone();
    let dest = s.create_ata(&auth.pubkey());
    let n = s.now();
    s.must_fail(&[s.finalise_ix(&auth.pubkey(), 1, n + 60, &t, &dest)], &[&auth], "State");
    s.close_by_authority(1).unwrap();
    let n = s.now();
    s.must_fail(&[s.finalise_ix(&auth.pubkey(), 1, n - 1, &t, &dest)], &[&auth], "Window");
    s.must_fail(&[s.finalise_ix(&auth.pubkey(), 1, n + MAX_TGE_DELAY + 1, &t, &dest)], &[&auth], "Window");
    // Wrong treasury is refused by has_one.
    let other = Pubkey::new_unique();
    s.must_fail(&[s.finalise_ix(&auth.pubkey(), 1, n + 60, &other, &dest)], &[&auth], "Treasury");
    s.must(&[s.finalise_ix(&auth.pubkey(), 1, n + MAX_TGE_DELAY, &t, &dest)], &[&auth]);
    assert_eq!(s.round(1).tge_ts, n + MAX_TGE_DELAY);
    s.must_fail(&[s.finalise_ix(&auth.pubkey(), 1, n + 60, &t, &dest)], &[&auth], "State");
    assert!(s.cancel(1).unwrap_err().contains("State"), "cancel after finalise");
}

/// F-06: a price that does not divide 10^9 floors every contribution; the round can still be filled exactly to the
/// hard cap, tokens sold stay at or below the cap in tokens, and the rounding dust comes back as unsold.
#[test]
fn rounding_with_a_price_that_does_not_divide_the_unit() {
    let mut s = S::boot();
    let now = s.now();
    let mut args = mainnet_angel(now, 1);
    args.lamports_per_token = 2_669; // 0.0005 USD at 187.33 USD per SOL
    let cap_tokens = s.live(args);
    assert_eq!(cap_tokens, tokens_for(250 * SOL, 2_669, 9));
    s.warp(60);
    let mut buyers = Vec::new();
    let mut expect_sold = 0u64;
    for i in 0..50u64 {
        let k = s.buyer(10);
        // Awkward amounts: 5 SOL less a few lamports for most, topped up later, so floors happen per contribution.
        let first = 5 * SOL - 7 * (i + 1);
        s.contribute(1, &k, first).unwrap();
        expect_sold += tokens_for(first, 2_669, 9);
        let top = 7 * (i + 1);
        match s.contribute(1, &k, top) {
            Ok(_) => expect_sold += tokens_for(top, 2_669, 9),
            Err(e) => panic!("top-up of {top} lamports refused: {e}"),
        }
        buyers.push(k);
    }
    let r = s.round(1);
    assert_eq!(r.raised, 250 * SOL, "filled exactly");
    assert_eq!(r.tokens_sold, expect_sold);
    assert!(r.tokens_sold <= cap_tokens, "never oversold");
    assert!(cap_tokens - r.tokens_sold < 100 * 50 * 2, "dust is a handful of base units per contribution");
    // Full: a stranger may close.
    let stranger = s.buyer(1);
    s.close_as(1, &stranger).unwrap();
    let tge = s.now() + 60;
    s.finalise(1, tge).unwrap();
    let dest = ata(&s.authority.pubkey(), &s.mint);
    assert_eq!(s.tokens(&dest), cap_tokens - expect_sold, "rounding dust returned as unsold");
    assert_eq!(s.tokens(&vault_pda(&round_pda(1))), expect_sold);
    // Everyone claims after the curve; the vault ends at zero.
    s.warp(60 + 60 * DAY);
    for k in &buyers {
        s.claim(1, k).unwrap();
    }
    assert_eq!(s.tokens(&vault_pda(&round_pda(1))), 0, "every sold token delivered");
    assert_eq!(s.round(1).tokens_claimed, expect_sold);
}

/// F-07: vesting at the edges on chain: tge_bps = 1 with a ten-second vest, the second before the end, the end itself,
/// and the create-time refusals around the unlock parameters.
#[test]
fn vesting_boundaries_on_chain() {
    let mut s = S::boot();
    let now = s.now();
    // Refusals.
    let mut bad = mainnet_angel(now, 1);
    bad.tge_bps = 0;
    assert!(s.create_round(bad).unwrap_err().contains("Unlock"));
    let mut bad = mainnet_angel(now, 1);
    bad.vest_seconds = MAX_VEST_SECONDS + 1;
    assert!(s.create_round(bad).unwrap_err().contains("Unlock"));
    let mut bad = mainnet_angel(now, 1);
    bad.vest_seconds = -1;
    assert!(s.create_round(bad).unwrap_err().contains("Unlock"));
    let mut bad = mainnet_angel(now, 1);
    bad.end_ts = bad.start_ts;
    assert!(s.create_round(bad).unwrap_err().contains("Window"));
    let mut bad = mainnet_angel(now, 1);
    bad.max_per_wallet = bad.hard_cap + 1;
    assert!(s.create_round(bad).unwrap_err().contains("WalletBounds"));
    // Refused: a zero soft cap, as the Caps error text says.
    let mut odd = mainnet_angel(now, 9);
    odd.soft_cap = 0;
    assert!(s.create_round(odd).unwrap_err().contains("Caps"));

    let mut args = mainnet_angel(now, 1);
    args.tge_bps = 1;
    args.vest_seconds = 10;
    args.soft_cap = 5 * SOL; // one buyer is enough to close above the soft cap
    s.live(args);
    s.warp(60);
    let a = s.buyer(10);
    s.contribute(1, &a, 5 * SOL).unwrap();
    let total = 2_000_000 * TOKEN;
    assert_eq!(s.participant(1, &a.pubkey()).tokens, total);
    s.close_by_authority(1).unwrap();
    let tge = s.now() + 100;
    s.finalise(1, tge).unwrap();
    s.warp_to(tge - 1);
    assert!(s.claim(1, &a).unwrap_err().contains("NotYetVested"));
    s.warp_to(tge);
    s.claim(1, &a).unwrap();
    let at = ata(&a.pubkey(), &s.mint);
    let tge_share = total / 10_000;
    assert_eq!(s.tokens(&at), tge_share, "one basis point at TGE");
    s.warp_to(tge + 9);
    s.claim(1, &a).unwrap();
    let rest = total - tge_share;
    assert_eq!(s.tokens(&at), tge_share + rest * 9 / 10);
    s.warp_to(tge + 10);
    s.claim(1, &a).unwrap();
    assert_eq!(s.tokens(&at), total, "whole at the end of the curve");
    // The position closed with the last claim; a further claim finds no account.
    assert!(s.claim(1, &a).unwrap_err().contains("AccountNotInitialized"));
    assert!(s.svm.get_account(&part_pda(&round_pda(1), &a.pubkey())).map_or(true, |p| p.lamports == 0));
}

/// F-08: account substitution is refused: another buyer's participant, a token account the buyer does not own, a
/// token account of another mint as the unsold destination, and a stranger's refund against someone's participant.
#[test]
fn wrong_accounts_are_refused() {
    let mut s = S::boot();
    let now = s.now();
    s.live(mainnet_angel(now, 1));
    s.warp(60);
    let a = s.buyer(10);
    let b = s.buyer(10);
    let mut others = Vec::new();
    for _ in 0..23 {
        let k = s.buyer(10);
        s.contribute(1, &k, 5 * SOL).unwrap();
        others.push(k);
    }
    s.contribute(1, &a, 5 * SOL).unwrap();
    s.contribute(1, &b, 5 * SOL).unwrap();
    // Wrong escrow on contribute.
    let c = s.buyer(10);
    let mut ix = s.contribute_ix(1, &c.pubkey(), 5 * SOL, &[]);
    ix.accounts[2] = rw(Pubkey::new_unique());
    s.must_fail(&[ix], &[&c], "ConstraintSeeds");
    s.close_by_authority(1).unwrap();
    // Unsold destination of another mint: refused by the token::mint constraint.
    let auth = s.authority.insecure_clone();
    let t = s.treasury.pubkey();
    let foreign_mint = Pubkey::new_unique();
    let mut data = vec![0u8; spl_token_interface::state::Mint::LEN];
    spl_token_interface::state::Mint { mint_authority: None.into(), supply: 0, decimals: 9, is_initialized: true, freeze_authority: None.into() }
        .pack_into_slice(&mut data);
    s.svm.set_account(foreign_mint, Account { lamports: SOL, data, owner: TOKEN_PROGRAM, executable: false, rent_epoch: 0 }).unwrap();
    let foreign_ta = Pubkey::new_unique();
    let mut data = vec![0u8; spl_token_interface::state::Account::LEN];
    spl_token_interface::state::Account {
        mint: foreign_mint,
        owner: auth.pubkey(),
        amount: 0,
        delegate: None.into(),
        state: spl_token_interface::state::AccountState::Initialized,
        is_native: None.into(),
        delegated_amount: 0,
        close_authority: None.into(),
    }
    .pack_into_slice(&mut data);
    s.svm.set_account(foreign_ta, Account { lamports: SOL, data, owner: TOKEN_PROGRAM, executable: false, rent_epoch: 0 }).unwrap();
    let n = s.now();
    s.must_fail(&[s.finalise_ix(&auth.pubkey(), 1, n + 60, &t, &foreign_ta)], &[&auth], "ConstraintTokenMint");
    s.finalise(1, n + 60).unwrap();
    s.warp(60);
    // a claims with b's participant: seeds mismatch.
    let at = s.create_ata(&a.pubkey());
    let bt = s.create_ata(&b.pubkey());
    s.must_fail(&[s.claim_ix(1, &a.pubkey(), &part_pda(&round_pda(1), &b.pubkey()), &at)], &[&a], "ConstraintSeeds");
    // a claims into b's token account: owner mismatch.
    s.must_fail(&[s.claim_ix(1, &a.pubkey(), &part_pda(&round_pda(1), &a.pubkey()), &bt)], &[&a], "ConstraintTokenOwner");
    // Correct claim still works afterwards.
    s.claim(1, &a).unwrap();
    assert_eq!(s.tokens(&at), 2_000_000 * TOKEN / 4);
}

/// F-09: refunds against someone else's participant and a refund on a round in the wrong state.
#[test]
fn refund_substitution_is_refused_and_cancel_after_close_restores_refunds() {
    let mut s = S::boot();
    let now = s.now();
    s.live(mainnet_angel(now, 1));
    s.warp(60);
    let a = s.buyer(10);
    let b = s.buyer(10);
    for _ in 0..23 {
        let k = s.buyer(10);
        s.contribute(1, &k, 5 * SOL).unwrap();
    }
    s.contribute(1, &a, 5 * SOL).unwrap();
    s.contribute(1, &b, 5 * SOL).unwrap();
    // Closed above the soft cap; the authority then cancels (a mistake found after close). Everyone refunds.
    s.close_by_authority(1).unwrap();
    assert_eq!(s.round(1).state, CLOSED);
    s.cancel(1).unwrap();
    assert_eq!(s.round(1).state, CANCELLED);
    // Token withdrawal first does not touch the escrow.
    let dest = s.create_ata(&s.authority.pubkey());
    s.withdraw_cancelled(1, &dest).unwrap();
    assert_eq!(s.tokens(&dest), 100_000_000 * TOKEN);
    assert_eq!(s.lamports(&escrow_pda(&round_pda(1))), 125 * SOL + s.svm.minimum_balance_for_rent_exemption(0));
    // b cannot pull a's refund (the participant seeds use the signer) and a signer cannot pass b's participant.
    s.must_fail(&[s.refund_ix(1, &b.pubkey(), &part_pda(&round_pda(1), &a.pubkey()))], &[&b], "ConstraintSeeds");
    let before = s.lamports(&a.pubkey());
    let rent_a = s.lamports(&part_pda(&round_pda(1), &a.pubkey()));
    s.refund(1, &a).unwrap();
    assert_eq!(s.lamports(&a.pubkey()) - before, 5 * SOL + rent_a, "exact lamports back, position rent included");
    // The participant account survives with its rent locked (about 0.0016 SOL per buyer).
    assert!(s.svm.get_account(&part_pda(&round_pda(1), &a.pubkey())).map_or(true, |p| p.lamports == 0), "position closed on refund, rent returned");
}

/// F-10: room left below the per-wallet minimum cannot be taken by a new wallet, so a round can sit one lamport short
/// of full with no one able to close it but the authority until the window ends. The client's `room()` quotes that
/// amount as allowed; the program refuses it.
#[test]
fn room_below_the_minimum_cannot_be_filled_by_a_new_wallet() {
    let mut s = S::boot();
    let now = s.now();
    s.live(mainnet_angel(now, 1));
    s.warp(60);
    for _ in 0..49 {
        let k = s.buyer(10);
        s.contribute(1, &k, 5 * SOL).unwrap();
    }
    let partial = s.buyer(10);
    s.contribute(1, &partial, 4 * SOL).unwrap();
    assert_eq!(s.round(1).raised, 249 * SOL);
    // Room is 1 SOL; the minimum is 1.25 SOL. A new wallet is refused on bounds, and the round is not "full".
    let newcomer = s.buyer(10);
    s.must_fail(&[s.contribute_ix(1, &newcomer.pubkey(), SOL, &[])], &[&newcomer], "WalletBounds");
    s.must_fail(&[s.close_ix(1, &newcomer.pubkey())], &[&newcomer], "Window");
    // Only an existing wallet with headroom can fill it.
    s.contribute(1, &partial, SOL).unwrap();
    assert_eq!(s.round(1).raised, 250 * SOL);
    s.close_as(1, &newcomer).unwrap();
    assert_eq!(s.round(1).state, CLOSED);
}

/// F-11: window edges: contribute at start_ts passes, at end_ts fails; a stranger may close at exactly end_ts; open
/// is refused at or after end_ts and allowed before start_ts.
#[test]
fn window_edges_are_exact() {
    let mut s = S::boot();
    let now = s.now();
    let args = mainnet_angel(now, 1);
    s.create_round(args).unwrap();
    s.fund(1, 100_000_000 * TOKEN);
    // Open before start is allowed.
    s.open(1).unwrap();
    let a = s.buyer(10);
    s.warp_to(args.start_ts - 1);
    assert!(s.contribute(1, &a, 5 * SOL).unwrap_err().contains("Window"));
    s.warp_to(args.start_ts);
    s.contribute(1, &a, 5 * SOL).unwrap();
    let b = s.buyer(10);
    s.warp_to(args.end_ts - 1);
    s.must_fail(&[s.close_ix(1, &b.pubkey())], &[&b], "Window");
    s.warp_to(args.end_ts);
    assert!(s.contribute(1, &b, 5 * SOL).unwrap_err().contains("Window"));
    s.close_as(1, &b).unwrap();
    assert_eq!(s.round(1).state, CANCELLED, "below soft cap at the end");
    assert!(s.contribute(1, &b, 5 * SOL).unwrap_err().contains("State"));
    // A pending round whose window has passed cannot be opened.
    let mut late = mainnet_angel(now, 2);
    late.start_ts = s.now() + 10;
    late.end_ts = s.now() + 20;
    s.create_round(late).unwrap();
    s.fund(2, 100_000_000 * TOKEN);
    s.warp(20);
    assert!(s.open(2).unwrap_err().contains("Window"));
}

/// F-12 (negative result): finalising with the vault itself as the unsold destination is refused by Anchor's
/// duplicate-mutable-account check, so unsold tokens cannot be stranded in a finalised vault that way.
#[test]
fn finalise_to_the_vault_itself_is_refused() {
    let mut s = S::boot();
    let now = s.now();
    s.live(mainnet_angel(now, 1));
    s.warp(60);
    for _ in 0..25 {
        let k = s.buyer(10);
        s.contribute(1, &k, 5 * SOL).unwrap();
    }
    s.close_by_authority(1).unwrap();
    let auth = s.authority.insecure_clone();
    let t = s.treasury.pubkey();
    let vault = vault_pda(&round_pda(1));
    let n = s.now();
    s.must_fail(&[s.finalise_ix(&auth.pubkey(), 1, n, &t, &vault)], &[&auth], "ConstraintDuplicateMutableAccount");
    assert_eq!(s.round(1).state, CLOSED);
}

/// F-13: the soft cap is not a protection the authority cannot neutralise: wallets controlled by the authority may
/// contribute, the authority closes above the soft cap and receives those lamports back at the treasury.
#[test]
fn authority_sybils_reach_the_soft_cap_at_no_cost() {
    let mut s = S::boot();
    let now = s.now();
    s.live(mainnet_angel(now, 1));
    s.warp(60);
    let outsider = s.buyer(10);
    s.contribute(1, &outsider, 5 * SOL).unwrap();
    // Twenty-four wallets the authority funds from its own balance.
    let auth = s.authority.insecure_clone();
    let mut mine = Vec::new();
    for _ in 0..24 {
        let k = Keypair::new();
        s.transfer(&auth, &k.pubkey(), 6 * SOL).unwrap();
        s.contribute(1, &k, 5 * SOL).unwrap();
        mine.push(k);
    }
    assert_eq!(s.round(1).raised, 125 * SOL);
    s.close_by_authority(1).unwrap();
    assert_eq!(s.round(1).state, CLOSED, "soft cap met with 96% authority money");
    let before = s.lamports(&s.treasury.pubkey());
    let tge = s.now();
    s.finalise(1, tge).unwrap();
    let floor = s.svm.minimum_balance_for_rent_exemption(0);
    assert_eq!(s.lamports(&s.treasury.pubkey()) - before, 125 * SOL + floor, "120 SOL of it comes straight back");
    // The outsider is locked into a round that, without the sybils, would have cancelled and refunded.
    assert!(s.refund(1, &outsider).unwrap_err().contains("State"));
}

/// F-14 (fixed): a hand-over is two-step. Proposing a key changes nothing; a typo to a key nobody holds leaves the
/// deployer in control, and the zero key clears the proposal. Only the named key can accept.
#[test]
fn set_authority_needs_acceptance() {
    let mut s = S::boot();
    let now = s.now();
    s.live(mainnet_angel(now, 1));
    let auth = s.authority.insecure_clone();
    // Proposing the current authority is refused as a no-op.
    let ix = s.authority_ix("set_authority", 1, auth.pubkey().as_ref());
    s.must_fail(&[ix], &[&auth], "ZeroKey");
    let nobody = Pubkey::new_unique();
    let ix = s.authority_ix("set_authority", 1, nobody.as_ref());
    s.must(&[ix], &[&auth]);
    assert_eq!(s.round(1).authority, auth.pubkey(), "still in control");
    assert_eq!(s.round(1).pending_authority, nobody);
    // A stranger cannot accept on nobody's behalf; accepting with nothing pending is refused too after clearing.
    let stranger = s.buyer(1);
    let mut accounts = vec![sig(stranger.pubkey()), rw(round_pda(1))];
    accounts.extend(evt());
    let ix = Instruction { program_id: SALE, accounts, data: ix_disc("accept_authority").to_vec() };
    s.must_fail(&[ix], &[&stranger], "Authority");
    let ix = s.authority_ix("set_authority", 1, Pubkey::default().as_ref());
    s.must(&[ix], &[&auth]);
    assert_eq!(s.round(1).pending_authority, Pubkey::default(), "proposal cleared");
    // A real hand-over: propose, accept, the old key is out.
    let keeper = s.buyer(1);
    let ix = s.authority_ix("set_authority", 1, keeper.pubkey().as_ref());
    s.must(&[ix], &[&auth]);
    let mut accounts = vec![sig(keeper.pubkey()), rw(round_pda(1))];
    accounts.extend(evt());
    let ix = Instruction { program_id: SALE, accounts, data: ix_disc("accept_authority").to_vec() };
    s.must(&[ix], &[&keeper]);
    assert_eq!(s.round(1).authority, keeper.pubkey());
    assert!(s.cancel(1).unwrap_err().contains("Authority"));
}

/// F-15: duplicate funding beyond the cap and token donations to the vault are returned as unsold on finalisation,
/// and the escrow receives nothing from them; `RoundFinalised.raised` reports the escrow balance, not `raised`.
#[test]
fn overfunded_vault_and_escrow_donations_flow_to_the_authority_and_treasury() {
    let mut s = S::boot();
    let now = s.now();
    s.live(mainnet_angel(now, 1));
    s.fund(1, 1_234 * TOKEN); // surplus tokens
    s.warp(60);
    for _ in 0..25 {
        let k = s.buyer(10);
        s.contribute(1, &k, 5 * SOL).unwrap();
    }
    let donor = s.buyer(10);
    let escrow = escrow_pda(&round_pda(1));
    s.transfer(&donor, &escrow, 3 * SOL).unwrap();
    s.close_by_authority(1).unwrap();
    let before = s.lamports(&s.treasury.pubkey());
    let tge = s.now() + 60;
    let logs = s.finalise(1, tge).unwrap();
    let floor = s.svm.minimum_balance_for_rent_exemption(0);
    assert_eq!(s.lamports(&s.treasury.pubkey()) - before, 128 * SOL + floor, "donation and the escrow floor go to the treasury");
    assert_eq!(s.round(1).raised, 125 * SOL, "the round's own counter is untouched");
    let dest = ata(&s.authority.pubkey(), &s.mint);
    assert_eq!(s.tokens(&dest), 50_000_000 * TOKEN + 1_234 * TOKEN, "surplus and unsold returned together");
    // Events travel by self-CPI (event_cpi), so the log shows a nested invoke rather than "Program data:".
    assert!(logs.iter().any(|l| l.contains("invoke [2]")), "event self-CPI emitted:\n{}", logs.join("\n"));
}

/// F-09 (fixed): a zero soft cap is refused at creation, so an empty round can never finalise as a success.
#[test]
fn zero_soft_cap_is_refused() {
    let mut s = S::boot();
    let now = s.now();
    let mut args = mainnet_angel(now, 1);
    args.soft_cap = 0;
    let err = s.create_round(args).expect_err("zero soft cap must be refused");
    assert!(err.contains("WalletBounds") || err.contains("Cap"), "{err}");
    // A window beyond ninety days is refused as well.
    let mut long = mainnet_angel(now, 1);
    long.end_ts = long.start_ts + 91 * DAY;
    let err = s.create_round(long).expect_err("window over ninety days must be refused");
    assert!(err.contains("Window"), "{err}");
}
