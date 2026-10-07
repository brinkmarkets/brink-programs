//! Permissioned pools: an ordinary AMM pool whose hook is the allowlist program. The AMM forwards the list and
//! the actor's entry (remaining accounts) to the hook, which vetoes `BeforeOpen` and `BeforeDeposit` for anyone
//! not on the list. Exits are never hooked, so a removed wallet can always leave.
use brink_svm_tests::harness::*;
use brink_svm_tests::products::*;
use brink_svm_tests::*;
use solana_signer::Signer;

const PERM_ID: [u8; 16] = *b"perm-usdc\0\0\0\0\0\0\0";

/// A hooked pool on a second benchmark; the builders point at it. Returns the default pool's keys too.
fn permissioned_env() -> (Env, PoolKeys, PoolKeys) {
    let mut e = setup_products();
    let bench = e.create_benchmark(PERM_ID, 684);
    let flags = HookFlags {
        before_open: true,
        before_deposit: true,
        ..HookFlags::default()
    };
    let keys = e
        .create_pool(bench, Some(ALLOWLIST), flags)
        .expect("hooked pool");
    let default = e.use_pool(&keys);
    let lp = e.lp.pubkey();
    e.create_ata(&lp, &keys.share_mint);
    (e, keys, default)
}

#[test]
fn only_the_amm_authority_creates_a_list_and_the_manager_runs_it() {
    let (mut e, keys, _) = permissioned_env();
    let manager = e.new_actor(0, false);
    let args = CreateListArgs {
        manager: manager.pubkey(),
        gate_open: true,
        gate_deposit: true,
    };
    let stranger = e.new_actor(0, false);
    let ix = e.create_list_ix(&keys.pool, &stranger.pubkey(), &args);
    e.must_fail(&[ix], &[&stranger], "NotAuthority");
    let a = e.authority.insecure_clone();
    let ix = e.create_list_ix(&keys.pool, &a.pubkey(), &args);
    e.must(&[ix], &[&a]);
    let l: List = e.acct("List", &list_pda(&keys.pool));
    assert_eq!(l.manager, manager.pubkey());
    assert!(l.gate_open && l.gate_deposit && !l.frozen);
    assert_eq!(l.entries, 0);
    // A second list on the same pool is refused by the PDA.
    let ix = e.create_list_ix(&keys.pool, &a.pubkey(), &args);
    assert!(e.send(&[ix], &[&a]).is_err());
    // Only the manager adds entries, and an entry needs at least one permission.
    let w = e.new_actor(0, false).pubkey();
    let ix = e.add_entry_ix(
        &keys.pool,
        &stranger.pubkey(),
        &w,
        &EntryTerms {
            may_open: true,
            may_deposit: true,
            expires_ts: 0,
        },
    );
    e.must_fail(&[ix], &[&stranger], "NotManager");
    let ix = e.add_entry_ix(
        &keys.pool,
        &manager.pubkey(),
        &w,
        &EntryTerms {
            may_open: false,
            may_deposit: false,
            expires_ts: 0,
        },
    );
    e.must_fail(&[ix], &[&manager], "NoPermission");
    let ix = e.add_entry_ix(
        &keys.pool,
        &manager.pubkey(),
        &w,
        &EntryTerms {
            may_open: true,
            may_deposit: false,
            expires_ts: 0,
        },
    );
    e.must(&[ix], &[&manager]);
    let en: Entry = e.acct("Entry", &entry_pda(&keys.pool, &w));
    assert!(en.may_open && !en.may_deposit);
    let l: List = e.acct("List", &list_pda(&keys.pool));
    assert_eq!(l.entries, 1);
    // Two-step manager handover.
    let next = e.new_actor(0, false);
    let ix = e.accept_manager_ix(&keys.pool, &next.pubkey());
    e.must_fail(&[ix], &[&next], "NotPending");
    let ix = e.set_manager_ix(&keys.pool, &manager.pubkey(), next.pubkey());
    e.must(&[ix], &[&manager]);
    let ix = e.accept_manager_ix(&keys.pool, &next.pubkey());
    e.must(&[ix], &[&next]);
    let l: List = e.acct("List", &list_pda(&keys.pool));
    assert_eq!(l.manager, next.pubkey());
    // The old manager is out; the new one removes the entry and gets its rent back.
    let ix = e.remove_entry_ix(&keys.pool, &manager.pubkey(), &w);
    e.must_fail(&[ix], &[&manager], "NotManager");
    let before = e.svm.get_balance(&next.pubkey()).unwrap();
    let ix = e.remove_entry_ix(&keys.pool, &next.pubkey(), &w);
    e.must(&[ix], &[&next]);
    assert!(
        e.svm.get_balance(&next.pubkey()).unwrap() > before,
        "rent returned to the manager"
    );
    assert!(e
        .svm
        .get_account(&entry_pda(&keys.pool, &w))
        .map(|a| a.data.is_empty())
        .unwrap_or(true));
    let l: List = e.acct("List", &list_pda(&keys.pool));
    assert_eq!(l.entries, 0);
}

#[test]
fn the_hook_gates_deposits_and_opens_and_never_exits() {
    let (mut e, keys, _) = permissioned_env();
    let manager = e.new_actor(0, false);
    let a = e.authority.insecure_clone();
    let ix = e.create_list_ix(
        &keys.pool,
        &a.pubkey(),
        &CreateListArgs {
            manager: manager.pubkey(),
            gate_open: true,
            gate_deposit: true,
        },
    );
    e.must(&[ix], &[&a]);
    let lp = e.lp.insecure_clone();
    let tr = e.trader.insecure_clone();
    // Without the list accounts the hook cannot find the list; without an entry it refuses. A hook's error
    // ends the transaction with the hook's own code, so the reason is visible to the wallet.
    let ix = e.deposit_ix_hook(&lp.pubkey(), 1_000_000 * USDC, 0, ALLOWLIST);
    e.must_fail(&[ix], &[&lp], "ListMissing");
    let ix = e.deposit_ix_hook(&lp.pubkey(), 1_000_000 * USDC, 0, ALLOWLIST);
    let ix = e.with_allowlist(ix, &lp.pubkey());
    e.must_fail(&[ix], &[&lp], "NotAllowed");
    // Listed to deposit: the deposit passes.
    let ix = e.add_entry_ix(
        &keys.pool,
        &manager.pubkey(),
        &lp.pubkey(),
        &EntryTerms {
            may_open: false,
            may_deposit: true,
            expires_ts: 0,
        },
    );
    e.must(&[ix], &[&manager]);
    let ix = e.deposit_ix_hook(&lp.pubkey(), 1_000_000 * USDC, 0, ALLOWLIST);
    let ix = e.with_allowlist(ix, &lp.pubkey());
    e.must(&[ix], &[&lp]);
    // The trader is not listed: the open is vetoed. Listed with an expiry: passes, then expires.
    let args = open_args(LegKind::PayFixed, 0, 10_000 * USDC, 2_000, 1);
    let ix = e.open_ix_hook(&tr.pubkey(), &args, ALLOWLIST);
    let ix = e.with_allowlist(ix, &tr.pubkey());
    e.must_fail(&[ix], &[&tr], "NotAllowed");
    let expires = e.clock().unix_timestamp + DAY;
    let ix = e.add_entry_ix(
        &keys.pool,
        &manager.pubkey(),
        &tr.pubkey(),
        &EntryTerms {
            may_open: true,
            may_deposit: false,
            expires_ts: expires,
        },
    );
    e.must(&[ix], &[&manager]);
    let ix = e.open_ix_hook(&tr.pubkey(), &args, ALLOWLIST);
    let ix = e.with_allowlist(ix, &tr.pubkey());
    e.must(&[ix], &[&tr]);
    e.warp(216_000, DAY + 1);
    e.publish(684).unwrap();
    let args2 = open_args(LegKind::PayFixed, 0, 10_000 * USDC, 2_000, 2);
    let ix = e.open_ix_hook(&tr.pubkey(), &args2, ALLOWLIST);
    let ix = e.with_allowlist(ix, &tr.pubkey());
    e.must_fail(&[ix], &[&tr], "Expired");
    // Permission to deposit is not permission to open.
    let args3 = open_args(LegKind::PayFixed, 0, 10_000 * USDC, 2_000, 3);
    let ix = e.open_ix_hook(&lp.pubkey(), &args3, ALLOWLIST);
    let ix = e.with_allowlist(ix, &lp.pubkey());
    e.must_fail(&[ix], &[&lp], "NotAllowed");
    // Gate off: everyone opens. Frozen: nobody does, listed or not.
    let ix = e.set_gates_ix(&keys.pool, &manager.pubkey(), false, true, false);
    e.must(&[ix], &[&manager]);
    let ix = e.open_ix_hook(&tr.pubkey(), &args2, ALLOWLIST);
    let ix = e.with_allowlist(ix, &tr.pubkey());
    e.must(&[ix], &[&tr]);
    let ix = e.set_gates_ix(&keys.pool, &manager.pubkey(), true, true, true);
    e.must(&[ix], &[&manager]);
    let ix = e.deposit_ix_hook(&lp.pubkey(), 1_000 * USDC, 0, ALLOWLIST);
    let ix = e.with_allowlist(ix, &lp.pubkey());
    e.must_fail(&[ix], &[&lp], "Frozen");
    // Exits are unconditional: the frozen list does not stop a withdrawal or an early close.
    let ix = e.withdraw_ix(&lp.pubkey(), 1_000 * SHARE * USDC, 0);
    e.must(&[ix], &[&lp]);
    let ix = e.close_ix("trader_cancel_swap", &tr.pubkey(), 1, &tr.pubkey(), &0u64);
    e.must(&[ix], &[&tr]);
}

#[test]
fn a_list_on_another_pool_or_a_pool_without_the_hook_is_refused() {
    let (mut e, keys, default) = permissioned_env();
    let manager = e.new_actor(0, false);
    let a = e.authority.insecure_clone();
    let ix = e.create_list_ix(
        &keys.pool,
        &a.pubkey(),
        &CreateListArgs {
            manager: manager.pubkey(),
            gate_open: true,
            gate_deposit: true,
        },
    );
    e.must(&[ix], &[&a]);
    // The default pool has no hook: creating a list for it is refused.
    let default_pool = default.pool;
    let ix = e.create_list_ix(
        &default_pool,
        &a.pubkey(),
        &CreateListArgs {
            manager: manager.pubkey(),
            gate_open: true,
            gate_deposit: true,
        },
    );
    e.must_fail(&[ix], &[&a], "PoolNotHooked");
    // An entry on the permissioned pool does not admit the wallet when the hook is handed the wrong list.
    let lp = e.lp.insecure_clone();
    let ix = e.add_entry_ix(
        &keys.pool,
        &manager.pubkey(),
        &lp.pubkey(),
        &EntryTerms {
            may_open: true,
            may_deposit: true,
            expires_ts: 0,
        },
    );
    e.must(&[ix], &[&manager]);
    let mut ix = e.deposit_ix_hook(&lp.pubkey(), 1_000 * USDC, 0, ALLOWLIST);
    ix.accounts.push(ro(list_pda(&default_pool)));
    ix.accounts.push(ro(entry_pda(&keys.pool, &lp.pubkey())));
    e.must_fail(&[ix], &[&lp], "ListMissing");
}
