//! brink_sale on LiteSVM: the full life of a round (create, fund, open, contribute, close, finalise, claim through
//! the vesting curve), the cancellation path with refunds, the allowlist, and every refusal a client relies on.
use borsh::{BorshDeserialize, BorshSerialize};
use brink_svm_tests::harness::set_upgrade_authority;
use brink_svm_tests::*;
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

const SALE: Pubkey = Pubkey::from_str_const("GYCJoDiZCh7mbqyWRjYJmpPUt3nPLs6887tGm58rmK3v");
const DAY: i64 = 86_400;
const BOOT: i64 = 1_790_000_000;
const SOL: u64 = 1_000_000_000;
const TOKEN: u64 = 1_000_000_000; // 9 decimals

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
fn leaf(k: &Pubkey) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update([0u8]);
    h.update(k.as_ref());
    h.finalize().into()
}
fn node(a: &[u8; 32], b: &[u8; 32]) -> [u8; 32] {
    let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
    let mut h = Sha256::new();
    h.update([1u8]);
    h.update(lo);
    h.update(hi);
    h.finalize().into()
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
            Account {
                lamports: 10 * SOL,
                data,
                owner: TOKEN_PROGRAM,
                executable: false,
                rent_epoch: 0,
            },
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
        S {
            svm,
            payer,
            authority,
            treasury,
            mint,
            mint_auth,
        }
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
        decode(
            "Participant",
            &self
                .svm
                .get_account(&part_pda(&round_pda(id), buyer))
                .unwrap()
                .data,
        )
    }
    fn lamports(&self, k: &Pubkey) -> u64 {
        self.svm.get_account(k).map(|a| a.lamports).unwrap_or(0)
    }
    fn tokens(&self, k: &Pubkey) -> u64 {
        let a = self.svm.get_account(k).expect("token account");
        spl_token_interface::state::Account::unpack(&a.data)
            .unwrap()
            .amount
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
                .map(|m| AccountMeta {
                    pubkey: m.pubkey,
                    is_signer: m.is_signer,
                    is_writable: m.is_writable,
                })
                .collect(),
            data: ix.data,
        };
        self.must(&[ix], &[]);
        ata(owner, &self.mint)
    }
    fn mint_to(&mut self, to: &Pubkey, amount: u64) {
        let auth = self.mint_auth.insecure_clone();
        let ix = spl_token_interface::instruction::mint_to(
            &TOKEN_PROGRAM,
            &self.mint,
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
    fn create_round(&mut self, args: CreateRoundArgs) -> Result<Vec<String>, String> {
        let round = round_pda(args.round_id);
        let auth = self.authority.insecure_clone();
        let mut accounts = vec![
            sigw(auth.pubkey()),
            ro(program_data(&SALE)),
            ro(self.treasury.pubkey()),
            ro(self.mint),
            rw(round),
            rw(escrow_pda(&round)),
            rw(vault_pda(&round)),
            ro(TOKEN_PROGRAM),
            ro(SYSTEM),
        ];
        accounts.extend(evt());
        let ix = Instruction {
            program_id: SALE,
            accounts,
            data: data("create_round", &args),
        };
        self.send(&[ix], &[&auth])
    }
    fn authority_ix(&self, name: &str, id: u8, extra: &[u8]) -> Instruction {
        let round = round_pda(id);
        let mut accounts = vec![
            sig(self.authority.pubkey()),
            rw(round),
            ro(vault_pda(&round)),
        ];
        accounts.extend(evt());
        let mut d = ix_disc(name).to_vec();
        d.extend_from_slice(extra);
        Instruction {
            program_id: SALE,
            accounts,
            data: d,
        }
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
    fn contribute_ix(
        &self,
        id: u8,
        buyer: &Pubkey,
        lamports: u64,
        proof: &[[u8; 32]],
    ) -> Instruction {
        let round = round_pda(id);
        let mut accounts = vec![
            sigw(*buyer),
            rw(round),
            rw(escrow_pda(&round)),
            rw(part_pda(&round, buyer)),
            ro(SYSTEM),
        ];
        accounts.extend(evt());
        #[derive(BorshSerialize)]
        struct A {
            lamports: u64,
            proof: Vec<[u8; 32]>,
        }
        Instruction {
            program_id: SALE,
            accounts,
            data: data(
                "contribute",
                &A {
                    lamports,
                    proof: proof.to_vec(),
                },
            ),
        }
    }
    fn contribute(
        &mut self,
        id: u8,
        buyer: &Keypair,
        lamports: u64,
    ) -> Result<Vec<String>, String> {
        let ix = self.contribute_ix(id, &buyer.pubkey(), lamports, &[]);
        self.send(&[ix], &[buyer])
    }
    fn close_ix(&self, id: u8, caller: &Pubkey) -> Instruction {
        let mut accounts = vec![sig(*caller), rw(round_pda(id))];
        accounts.extend(evt());
        Instruction {
            program_id: SALE,
            accounts,
            data: ix_disc("close_round").to_vec(),
        }
    }
    fn finalise(&mut self, id: u8, tge_ts: i64) -> Result<Vec<String>, String> {
        let round = round_pda(id);
        let auth = self.authority.insecure_clone();
        let dest = self.create_ata(&auth.pubkey());
        let mut accounts = vec![
            sig(auth.pubkey()),
            rw(round),
            rw(escrow_pda(&round)),
            rw(self.treasury.pubkey()),
            ro(self.mint),
            rw(vault_pda(&round)),
            rw(dest),
            ro(TOKEN_PROGRAM),
            ro(SYSTEM),
        ];
        accounts.extend(evt());
        let ix = Instruction {
            program_id: SALE,
            accounts,
            data: data("finalise", &tge_ts),
        };
        self.send(&[ix], &[&auth])
    }
    fn claim(&mut self, id: u8, buyer: &Keypair) -> Result<Vec<String>, String> {
        let round = round_pda(id);
        let bt = self.create_ata(&buyer.pubkey());
        let mut accounts = vec![
            sigw(buyer.pubkey()),
            rw(round),
            rw(part_pda(&round, &buyer.pubkey())),
            ro(self.mint),
            rw(vault_pda(&round)),
            rw(bt),
            ro(TOKEN_PROGRAM),
        ];
        accounts.extend(evt());
        let ix = Instruction {
            program_id: SALE,
            accounts,
            data: ix_disc("claim").to_vec(),
        };
        self.send(&[ix], &[buyer])
    }
    fn refund(&mut self, id: u8, buyer: &Keypair) -> Result<Vec<String>, String> {
        let round = round_pda(id);
        let mut accounts = vec![
            sigw(buyer.pubkey()),
            ro(round),
            rw(part_pda(&round, &buyer.pubkey())),
            rw(escrow_pda(&round)),
            ro(SYSTEM),
        ];
        accounts.extend(evt());
        let ix = Instruction {
            program_id: SALE,
            accounts,
            data: ix_disc("refund").to_vec(),
        };
        self.send(&[ix], &[buyer])
    }
    fn buyer(&mut self, sol: u64) -> Keypair {
        let k = Keypair::new();
        self.svm.airdrop(&k.pubkey(), sol * SOL).unwrap();
        k
    }
}

/// Angel-shaped round: 10 000 lamports per token, 500 SOL hard cap, 250 SOL soft cap, 2.5 to 50 SOL per wallet,
/// 25% at TGE then 60 days.
fn angel(now: i64, root: [u8; 32], allowlisted: bool) -> CreateRoundArgs {
    CreateRoundArgs {
        round_id: 1,
        lamports_per_token: 10_000,
        hard_cap: 500 * SOL,
        soft_cap: 250 * SOL,
        min_per_wallet: 25 * SOL / 10,
        max_per_wallet: 50 * SOL,
        start_ts: now + 60,
        end_ts: now + 7 * DAY,
        tge_bps: 2_500,
        vest_seconds: 60 * DAY,
        allowlist_root: root,
        allowlisted,
    }
}

/// Public-shaped round: 50 000 lamports per token, 10 000 SOL hard cap, 2 500 soft, 1 to 200 SOL per wallet, 100% at TGE.
fn public(now: i64) -> CreateRoundArgs {
    CreateRoundArgs {
        round_id: 2,
        lamports_per_token: 50_000,
        hard_cap: 10_000 * SOL,
        soft_cap: 2_500 * SOL,
        min_per_wallet: SOL,
        max_per_wallet: 200 * SOL,
        start_ts: now + 60,
        end_ts: now + 14 * DAY,
        tge_bps: 10_000,
        vest_seconds: 0,
        allowlist_root: [0; 32],
        allowlisted: false,
    }
}

#[test]
fn angel_round_full_life() {
    let mut s = S::boot();
    let now = s.now();
    s.create_round(angel(now, [0; 32], false)).unwrap();
    let r = s.round(1);
    assert_eq!(r.state, 0, "pending");
    assert_eq!(r.decimals, 9);

    // Cannot open before the vault can honour the hard cap: 500 SOL / 10 000 lamports = 50M tokens.
    s.fund(1, 49_999_999 * TOKEN);
    s.must_fail(
        &[s.authority_ix("open_round", 1, &[])],
        &[&s.authority.insecure_clone()],
        "Unfunded",
    );
    s.fund(1, TOKEN);
    s.open(1).unwrap();
    assert_eq!(s.round(1).state, 1, "open");

    // Before start: refused.
    let a = s.buyer(100);
    let b = s.buyer(100);
    let c = s.buyer(400);
    match s.contribute(1, &a, 10 * SOL) {
        Err(e) => assert!(e.contains("Window"), "{e}"),
        Ok(_) => panic!("contributed before start"),
    }
    s.warp(120);

    // Bounds.
    match s.contribute(1, &a, 2 * SOL) {
        Err(e) => assert!(e.contains("WalletBounds"), "{e}"),
        Ok(_) => panic!("below minimum accepted"),
    }
    match s.contribute(1, &a, 51 * SOL) {
        Err(e) => assert!(e.contains("WalletBounds"), "{e}"),
        Ok(_) => panic!("above maximum accepted"),
    }
    s.contribute(1, &a, 10 * SOL).unwrap();
    let pa = s.participant(1, &a.pubkey());
    assert_eq!(pa.lamports, 10 * SOL);
    assert_eq!(
        pa.tokens,
        1_000_000 * TOKEN,
        "10 SOL at 10 000 lamports per token"
    );
    let floor = s.svm.minimum_balance_for_rent_exemption(0);
    assert_eq!(
        s.lamports(&escrow_pda(&round_pda(1))),
        10 * SOL + floor,
        "contributions on top of the escrow rent floor"
    );
    // Top up within bounds.
    s.contribute(1, &a, 40 * SOL).unwrap();
    assert_eq!(s.participant(1, &a.pubkey()).lamports, 50 * SOL);
    match s.contribute(1, &a, 25 * SOL / 10) {
        Err(e) => assert!(e.contains("WalletBounds"), "{e}"),
        Ok(_) => panic!("wallet cap exceeded"),
    }
    let r = s.round(1);
    assert_eq!(r.raised, 50 * SOL);
    assert_eq!(r.participants, 1);

    // Nobody but the authority can close early.
    s.must_fail(&[s.close_ix(1, &b.pubkey())], &[&b], "Window");

    // Fill to the hard cap with eight more wallets of 50 and one of 50 split across two.
    for _ in 0..8 {
        let k = s.buyer(60);
        s.contribute(1, &k, 50 * SOL).unwrap();
    }
    s.contribute(1, &b, 25 * SOL).unwrap();
    // Room left is 25 SOL; 30 is refused as RoundFull, 25 fills.
    match s.contribute(1, &c, 30 * SOL) {
        Err(e) => assert!(e.contains("RoundFull"), "{e}"),
        Ok(_) => panic!("hard cap exceeded"),
    }
    s.contribute(1, &c, 25 * SOL).unwrap();
    let r = s.round(1);
    assert_eq!(r.raised, 500 * SOL);
    assert_eq!(r.tokens_sold, 50_000_000 * TOKEN);
    assert_eq!(r.participants, 11);
    match s.contribute(1, &b, 25 * SOL) {
        Err(e) => assert!(e.contains("RoundFull"), "{e}"),
        Ok(_) => panic!("full round accepted"),
    }

    // Full: anyone may close. Nothing to claim before finalisation.
    let cl = s.close_ix(1, &b.pubkey());
    s.must(&[cl], &[&b]);
    assert_eq!(s.round(1).state, 2, "closed");
    match s.claim(1, &a) {
        Err(e) => assert!(e.contains("State"), "{e}"),
        Ok(_) => panic!("claimed before finalisation"),
    }

    // Finalise: escrow to treasury, TGE in two days.
    let before = s.lamports(&s.treasury.pubkey());
    let tge = s.now() + 2 * DAY;
    s.finalise(1, tge).unwrap();
    let floor = s.svm.minimum_balance_for_rent_exemption(0);
    assert_eq!(
        s.lamports(&s.treasury.pubkey()) - before,
        500 * SOL + floor,
        "contributions plus the escrow rent floor"
    );
    assert_eq!(s.lamports(&escrow_pda(&round_pda(1))), 0);
    assert_eq!(s.round(1).state, 3, "finalised");
    assert_eq!(
        s.tokens(&vault_pda(&round_pda(1))),
        50_000_000 * TOKEN,
        "nothing unsold"
    );

    // Before TGE nothing vests.
    match s.claim(1, &a) {
        Err(e) => assert!(e.contains("NotYetVested"), "{e}"),
        Ok(_) => panic!("claimed before TGE"),
    }
    s.warp(2 * DAY);
    s.claim(1, &a).unwrap();
    let at = ata(&a.pubkey(), &s.mint);
    let total_a = 5_000_000 * TOKEN;
    assert_eq!(s.tokens(&at), total_a / 4, "25% at TGE");
    match s.claim(1, &a) {
        Err(e) => assert!(e.contains("NothingToClaim"), "{e}"),
        Ok(_) => panic!("double claim"),
    }
    // Half way: 25% + 37.5%.
    s.warp(30 * DAY);
    s.claim(1, &a).unwrap();
    let rest = (total_a - total_a / 4) as u128;
    let expect = total_a / 4 + (rest * (30 * DAY) as u128 / (60 * DAY) as u128) as u64;
    assert_eq!(s.tokens(&at), expect);
    // After the curve: everything, and the round's claimed total adds up.
    s.warp(40 * DAY);
    s.claim(1, &a).unwrap();
    assert_eq!(s.tokens(&at), total_a);
    assert!(
        s.svm
            .get_account(&part_pda(&round_pda(1), &a.pubkey()))
            .is_none_or(|acc| acc.lamports == 0),
        "fully claimed position closes and returns its rent"
    );
    assert_eq!(s.round(1).tokens_claimed, total_a);
    // A closed position cannot claim again: the record is gone.
    match s.claim(1, &a) {
        Err(e) => assert!(
            e.contains("AccountNotInitialized") || e.contains("3012"),
            "{e}"
        ),
        Ok(_) => panic!("claimed from a closed position"),
    }
    // A late claimer gets everything in one go.
    s.claim(1, &c).unwrap();
    assert_eq!(s.tokens(&ata(&c.pubkey(), &s.mint)), 2_500_000 * TOKEN);
    // Refund is not a thing on a finalised round (b still holds an open position).
    match s.refund(1, &b) {
        Err(e) => assert!(e.contains("State"), "{e}"),
        Ok(_) => panic!("refund after finalisation"),
    }
}

#[test]
fn public_round_unlocked_and_unsold_returned() {
    let mut s = S::boot();
    let now = s.now();
    s.create_round(public(now)).unwrap();
    s.fund(2, 200_000_000 * TOKEN);
    s.open(2).unwrap();
    s.warp(120);
    let a = s.buyer(300);
    s.contribute(2, &a, 200 * SOL).unwrap();
    assert_eq!(
        s.participant(2, &a.pubkey()).tokens,
        4_000_000 * TOKEN,
        "200 SOL at 50 000 lamports"
    );
    // Many wallets to pass the soft cap.
    for _ in 0..12 {
        let k = s.buyer(210);
        s.contribute(2, &k, 200 * SOL).unwrap();
    }
    assert_eq!(s.round(2).raised, 2_600 * SOL);
    // Window not over, not full: nobody can close early, the authority included.
    s.must_fail(&[s.close_ix(2, &a.pubkey())], &[&a], "Window");
    let cl = s.close_ix(2, &s.authority.pubkey());
    s.must_fail(&[cl], &[&s.authority.insecure_clone()], "Window");
    let r2 = s.round(2);
    s.warp(r2.end_ts - s.now());
    let cl = s.close_ix(2, &a.pubkey());
    s.must(&[cl], &[&a]);
    assert_eq!(s.round(2).state, 2, "closed above soft cap");
    let tge = s.now() + 60;
    s.finalise(2, tge).unwrap();
    // Unsold tokens returned: 200M minus 52M sold.
    let dest = ata(&s.authority.pubkey(), &s.mint);
    assert_eq!(s.tokens(&dest), (200_000_000 - 52_000_000) * TOKEN);
    assert_eq!(s.tokens(&vault_pda(&round_pda(2))), 52_000_000 * TOKEN);
    s.warp(60);
    s.claim(2, &a).unwrap();
    assert_eq!(
        s.tokens(&ata(&a.pubkey(), &s.mint)),
        4_000_000 * TOKEN,
        "fully unlocked at TGE"
    );
}

#[test]
fn soft_cap_missed_cancels_and_refunds() {
    let mut s = S::boot();
    let now = s.now();
    s.create_round(public(now)).unwrap();
    s.fund(2, 200_000_000 * TOKEN);
    s.open(2).unwrap();
    s.warp(120);
    let a = s.buyer(10);
    let b = s.buyer(10);
    s.contribute(2, &a, 5 * SOL).unwrap();
    s.contribute(2, &b, 3 * SOL).unwrap();
    // Window ends: anyone closes; below soft cap it cancels.
    s.warp(14 * DAY);
    let cl = s.close_ix(2, &b.pubkey());
    s.must(&[cl], &[&b]);
    assert_eq!(s.round(2).state, 4, "cancelled");
    match s.finalise(2, s.now() + 60) {
        Err(e) => assert!(e.contains("State"), "{e}"),
        Ok(_) => panic!("finalised a cancelled round"),
    }
    let before_a = s.lamports(&a.pubkey());
    let part_rent = s.lamports(&part_pda(&round_pda(2), &a.pubkey()));
    s.refund(2, &a).unwrap();
    assert_eq!(
        s.lamports(&a.pubkey()) - before_a,
        5 * SOL + part_rent,
        "the deposit and the position rent both return"
    );
    assert!(
        s.svm
            .get_account(&part_pda(&round_pda(2), &a.pubkey()))
            .is_none_or(|acc| acc.lamports == 0),
        "position record closed"
    );
    match s.refund(2, &a) {
        Err(e) => assert!(
            e.contains("AccountNotInitialized") || e.contains("3012"),
            "{e}"
        ),
        Ok(_) => panic!("double refund"),
    }
    s.refund(2, &b).unwrap();
    let floor = s.svm.minimum_balance_for_rent_exemption(0);
    assert_eq!(
        s.lamports(&escrow_pda(&round_pda(2))),
        floor,
        "escrow holds only its rent floor after all refunds"
    );
    // Authority recovers the tokens.
    let round = round_pda(2);
    let dest = s.create_ata(&s.authority.pubkey());
    let mut accounts = vec![
        sig(s.authority.pubkey()),
        ro(round),
        ro(s.mint),
        rw(vault_pda(&round)),
        rw(dest),
        ro(TOKEN_PROGRAM),
    ];
    accounts.extend(evt());
    let ix = Instruction {
        program_id: SALE,
        accounts,
        data: ix_disc("withdraw_cancelled").to_vec(),
    };
    let auth = s.authority.insecure_clone();
    s.must(&[ix], &[&auth]);
    assert_eq!(s.tokens(&dest), 200_000_000 * TOKEN);
}

#[test]
fn allowlist_gates_the_angel_round() {
    let mut s = S::boot();
    let now = s.now();
    let a = s.buyer(100);
    let b = s.buyer(100);
    let stranger = s.buyer(100);
    let la = leaf(&a.pubkey());
    let lb = leaf(&b.pubkey());
    let root = node(&la, &lb);
    s.create_round(angel(now, root, true)).unwrap();
    s.fund(1, 50_000_000 * TOKEN);
    s.open(1).unwrap();
    s.warp(120);
    // No proof, wrong proof, stranger with a real proof: all refused.
    match s.contribute(1, &a, 10 * SOL) {
        Err(e) => assert!(e.contains("Allowlist"), "{e}"),
        Ok(_) => panic!("accepted without proof"),
    }
    let ix = s.contribute_ix(1, &a.pubkey(), 10 * SOL, &[la]);
    s.must_fail(&[ix], &[&a], "Allowlist");
    let ix = s.contribute_ix(1, &stranger.pubkey(), 10 * SOL, &[lb]);
    s.must_fail(&[ix], &[&stranger], "Allowlist");
    // Right proofs pass.
    let ix = s.contribute_ix(1, &a.pubkey(), 10 * SOL, &[lb]);
    s.must(&[ix], &[&a]);
    let ix = s.contribute_ix(1, &b.pubkey(), 10 * SOL, &[la]);
    s.must(&[ix], &[&b]);
    assert_eq!(s.round(1).participants, 2);
}

#[test]
fn creation_refuses_bad_parameters_and_authority_is_enforced() {
    let mut s = S::boot();
    let now = s.now();
    let mut bad = angel(now, [0; 32], false);
    bad.lamports_per_token = 0;
    assert!(s.create_round(bad).unwrap_err().contains("Price"));
    let mut bad = angel(now, [0; 32], false);
    bad.soft_cap = bad.hard_cap + 1;
    assert!(s.create_round(bad).unwrap_err().contains("Caps"));
    let mut bad = angel(now, [0; 32], false);
    bad.min_per_wallet = 1;
    assert!(s.create_round(bad).unwrap_err().contains("WalletBounds"));
    let mut bad = angel(now, [0; 32], false);
    bad.start_ts = now - 1;
    assert!(s.create_round(bad).unwrap_err().contains("Window"));
    let mut bad = angel(now, [0; 32], false);
    bad.tge_bps = 10_000; // with a 60-day vest: inconsistent
    assert!(s.create_round(bad).unwrap_err().contains("Unlock"));
    let mut bad = angel(now, [0; 32], false);
    bad.vest_seconds = 0; // with 25% at TGE: inconsistent
    assert!(s.create_round(bad).unwrap_err().contains("Unlock"));

    s.create_round(angel(now, [0; 32], false)).unwrap();
    // A stranger cannot open, cancel or re-point the authority.
    let stranger = s.buyer(10);
    let round = round_pda(1);
    for name in ["open_round", "cancel_round"] {
        let mut accounts = vec![sig(stranger.pubkey()), rw(round), ro(vault_pda(&round))];
        accounts.extend(evt());
        let ix = Instruction {
            program_id: SALE,
            accounts,
            data: ix_disc(name).to_vec(),
        };
        s.must_fail(&[ix], &[&stranger], "Authority");
    }
    // Cancel before opening, then the vault can be withdrawn.
    let auth = s.authority.insecure_clone();
    let ix = s.authority_ix("cancel_round", 1, &[]);
    s.must(&[ix], &[&auth]);
    assert_eq!(s.round(1).state, 4);
    // Hand-over is two-step: the proposal changes nothing until the named key accepts; then the old key is out.
    let new_auth = s.buyer(10);
    let ix = s.authority_ix("set_authority", 1, new_auth.pubkey().as_ref());
    s.must(&[ix], &[&auth]);
    assert_eq!(
        s.round(1).authority,
        auth.pubkey(),
        "unchanged until accepted"
    );
    assert_eq!(s.round(1).pending_authority, new_auth.pubkey());
    let stranger2 = s.buyer(1);
    let mut accounts = vec![sig(stranger2.pubkey()), rw(round_pda(1))];
    accounts.extend(evt());
    let ix = Instruction {
        program_id: SALE,
        accounts,
        data: ix_disc("accept_authority").to_vec(),
    };
    s.must_fail(&[ix], &[&stranger2], "Authority");
    let mut accounts = vec![sig(new_auth.pubkey()), rw(round_pda(1))];
    accounts.extend(evt());
    let ix = Instruction {
        program_id: SALE,
        accounts,
        data: ix_disc("accept_authority").to_vec(),
    };
    s.must(&[ix], &[&new_auth]);
    assert_eq!(s.round(1).authority, new_auth.pubkey());
    assert_eq!(s.round(1).pending_authority, Pubkey::default());
    let ix = s.authority_ix("set_authority", 1, auth.pubkey().as_ref());
    s.must_fail(&[ix], &[&auth], "Authority");
}
