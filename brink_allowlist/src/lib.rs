//! Brink permissioned pools: the allow-list hook.
//!
//! A permissioned pool is an ordinary swap AMM pool created with this program as its hook and the `before_open`
//! and `before_deposit` points enabled. The AMM calls the hook with the pool read-only, a fixed payload and the
//! instruction's remaining accounts; the hook has no signer privileges and cannot move funds. It answers one
//! question: may this wallet take this entry on this pool. Exits (withdraw, cancel, settle, liquidate) never
//! consult a hook, so a wallet removed from the list can always leave.
//!
//! ```text
//! create_list (swap AMM authority, once per pool) ──▶ add_entry / update_entry / remove_entry (list manager)
//!                                                  ──▶ set_gates (which points are gated)
//!                                                  ──▶ set_manager ──▶ accept_manager (two step)
//! hook calls (from the AMM, via the fallback): BeforeOpen, BeforeDeposit veto; AfterOpen, AfterDeposit observe
//! ```
//!
//! * One `List` per pool at `["list", pool]`, created only by the AMM's protocol authority, which names the list
//!   manager. The manager runs the list day to day and hands over in two steps.
//! * One `Entry` per wallet at `["entry", pool, wallet]` with separate permissions to open swaps and to deposit as
//!   an LP, and an optional expiry. Removing an entry closes it and returns its rent to the manager.
//! * A gate that is switched off admits everyone at that point; a frozen list admits no one. Both are reversible.
//! * The hook reads the `List` and the wallet's `Entry` from the remaining accounts the caller passes. Accounts
//!   are identified by their derived address, not by position, so a client may pass them in any order.
#![allow(clippy::module_name_repetitions)]
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::arithmetic_side_effects,
        clippy::indexing_slicing
    )
)]

use anchor_lang::prelude::*;
use swap_amm::state::{Global as AmmGlobal, Pool as AmmPool};

declare_id!("FaNttHSrjcQruS8qBNFDdmAnKq9Z4ZGu2y1UkMYXe1UR");

/// Hook points as the AMM sends them (the first byte of the hook call data).
pub const POINT_BEFORE_OPEN: u8 = 0;
pub const POINT_AFTER_OPEN: u8 = 1;
pub const POINT_BEFORE_DEPOSIT: u8 = 4;
pub const POINT_AFTER_DEPOSIT: u8 = 5;

/// Mirror of the AMM's `HookPayload`: the fields after the point byte.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy)]
pub struct HookPayload {
    pub pool: Pubkey,
    pub actor: Pubkey,
    pub amount: u64,
    pub notional: u64,
    pub leg: Option<u8>,
    pub tenor: u8,
}

#[program]
pub mod brink_allowlist {
    use super::*;

    /// Creates the list for a pool whose hook is this program. Only the AMM's protocol authority may do so, and
    /// it names the manager who runs the list.
    pub fn create_list(ctx: Context<CreateList>, args: CreateListArgs) -> Result<()> {
        require!(
            ctx.accounts.pool.hook_program == crate::ID,
            AllowError::PoolNotHooked
        );
        require!(args.manager != Pubkey::default(), AllowError::ZeroKey);
        let l = &mut ctx.accounts.list;
        l.version = 1;
        l.pool = ctx.accounts.pool.key();
        l.manager = args.manager;
        l.pending_manager = Pubkey::default();
        l.gate_open = args.gate_open;
        l.gate_deposit = args.gate_deposit;
        l.frozen = false;
        l.entries = 0;
        l.bump = ctx.bumps.list;
        l.reserved = [0; 64];
        emit_cpi!(ListCreated {
            list: l.key(),
            pool: l.pool,
            manager: l.manager,
            gate_open: l.gate_open,
            gate_deposit: l.gate_deposit,
        });
        Ok(())
    }

    /// Adds a wallet to the list with its permissions and an optional expiry (`0` for none).
    pub fn add_entry(ctx: Context<AddEntry>, args: EntryTerms) -> Result<()> {
        check_entry_terms(&args)?;
        let now = Clock::get()?.unix_timestamp;
        require!(
            args.expires_ts == 0 || args.expires_ts > now,
            AllowError::ExpiryInPast
        );
        let e = &mut ctx.accounts.entry;
        e.version = 1;
        e.list = ctx.accounts.list.key();
        e.wallet = ctx.accounts.wallet.key();
        e.may_open = args.may_open;
        e.may_deposit = args.may_deposit;
        e.expires_ts = args.expires_ts;
        e.added_ts = now;
        e.bump = ctx.bumps.entry;
        e.reserved = [0; 32];
        let l = &mut ctx.accounts.list;
        l.entries = l.entries.checked_add(1).ok_or(AllowError::Overflow)?;
        emit_cpi!(EntryChanged {
            list: l.key(),
            wallet: e.wallet,
            may_open: e.may_open,
            may_deposit: e.may_deposit,
            expires_ts: e.expires_ts,
            removed: false,
        });
        Ok(())
    }

    /// Changes a wallet's permissions or expiry.
    pub fn update_entry(ctx: Context<UpdateEntry>, args: EntryTerms) -> Result<()> {
        check_entry_terms(&args)?;
        let now = Clock::get()?.unix_timestamp;
        require!(
            args.expires_ts == 0 || args.expires_ts > now,
            AllowError::ExpiryInPast
        );
        let e = &mut ctx.accounts.entry;
        e.may_open = args.may_open;
        e.may_deposit = args.may_deposit;
        e.expires_ts = args.expires_ts;
        emit_cpi!(EntryChanged {
            list: ctx.accounts.list.key(),
            wallet: e.wallet,
            may_open: e.may_open,
            may_deposit: e.may_deposit,
            expires_ts: e.expires_ts,
            removed: false,
        });
        Ok(())
    }

    /// Removes a wallet: closes its entry and returns the rent to the manager. Open positions are unaffected;
    /// exits never consult the hook.
    pub fn remove_entry(ctx: Context<RemoveEntry>) -> Result<()> {
        let l = &mut ctx.accounts.list;
        l.entries = l.entries.checked_sub(1).ok_or(AllowError::Overflow)?;
        emit_cpi!(EntryChanged {
            list: l.key(),
            wallet: ctx.accounts.entry.wallet,
            may_open: false,
            may_deposit: false,
            expires_ts: 0,
            removed: true,
        });
        Ok(())
    }

    /// Sets which points are gated and whether the list is frozen. A gate that is off admits everyone at that
    /// point; a frozen list admits no one at any gated point.
    pub fn set_gates(
        ctx: Context<ManageList>,
        gate_open: bool,
        gate_deposit: bool,
        frozen: bool,
    ) -> Result<()> {
        let l = &mut ctx.accounts.list;
        l.gate_open = gate_open;
        l.gate_deposit = gate_deposit;
        l.frozen = frozen;
        emit_cpi!(GatesChanged {
            list: l.key(),
            gate_open,
            gate_deposit,
            frozen
        });
        Ok(())
    }

    /// Proposes a new manager. The zero key clears a pending proposal.
    pub fn set_manager(ctx: Context<ManageList>, new_manager: Pubkey) -> Result<()> {
        let l = &mut ctx.accounts.list;
        require!(new_manager != l.manager, AllowError::SameManager);
        l.pending_manager = new_manager;
        emit_cpi!(ManagerProposed {
            list: l.key(),
            pending_manager: new_manager
        });
        Ok(())
    }

    /// The proposed manager accepts.
    pub fn accept_manager(ctx: Context<AcceptManager>) -> Result<()> {
        let l = &mut ctx.accounts.list;
        let previous = l.manager;
        l.manager = ctx.accounts.pending_manager.key();
        l.pending_manager = Pubkey::default();
        emit_cpi!(ManagerChanged {
            list: l.key(),
            previous,
            manager: l.manager
        });
        Ok(())
    }

    /// Hook entry. The AMM calls this program with data `[point][HookPayload]` rather than an Anchor
    /// instruction, so the call lands here. Veto points refuse with an error; observe points return at once.
    pub fn fallback<'info>(
        program_id: &Pubkey,
        accounts: &'info [AccountInfo<'info>],
        data: &[u8],
    ) -> Result<()> {
        hook::handle(program_id, accounts, data)
    }
}

fn check_entry_terms(a: &EntryTerms) -> Result<()> {
    require!(a.may_open || a.may_deposit, AllowError::NoPermission);
    Ok(())
}

/// The hook path: parses the AMM's call and checks the wallet against the list.
pub mod hook {
    use super::*;

    pub fn handle(program_id: &Pubkey, accounts: &[AccountInfo], data: &[u8]) -> Result<()> {
        let (point, rest) = data.split_first().ok_or(AllowError::BadCall)?;
        let payload = HookPayload::try_from_slice(rest).map_err(|_| AllowError::BadCall)?;
        match *point {
            POINT_AFTER_OPEN | POINT_AFTER_DEPOSIT => return Ok(()),
            POINT_BEFORE_OPEN | POINT_BEFORE_DEPOSIT => {}
            _ => return Err(AllowError::BadCall.into()),
        }
        // The pool is always the first account and must be the AMM's account named in the payload.
        let pool_info = accounts.first().ok_or(AllowError::BadCall)?;
        require!(pool_info.key() == payload.pool, AllowError::PoolMismatch);
        require!(*pool_info.owner == swap_amm::ID, AllowError::PoolMismatch);
        {
            let data = pool_info.try_borrow_data()?;
            let mut slice: &[u8] = &data;
            let pool =
                AmmPool::try_deserialize(&mut slice).map_err(|_| AllowError::PoolMismatch)?;
            require!(pool.hook_program == *program_id, AllowError::PoolNotHooked);
        }
        let (list_key, _) =
            Pubkey::find_program_address(&[List::SEED, payload.pool.as_ref()], program_id);
        let list_info = accounts
            .iter()
            .find(|a| a.key() == list_key)
            .ok_or(AllowError::ListMissing)?;
        require!(*list_info.owner == *program_id, AllowError::ListMissing);
        let list = {
            let data = list_info.try_borrow_data()?;
            let mut slice: &[u8] = &data;
            List::try_deserialize(&mut slice).map_err(|_| AllowError::ListMissing)?
        };
        require!(list.pool == payload.pool, AllowError::PoolMismatch);
        require!(!list.frozen, AllowError::Frozen);
        let gated = if *point == POINT_BEFORE_OPEN {
            list.gate_open
        } else {
            list.gate_deposit
        };
        if !gated {
            return Ok(());
        }
        let (entry_key, _) = Pubkey::find_program_address(
            &[Entry::SEED, payload.pool.as_ref(), payload.actor.as_ref()],
            program_id,
        );
        let entry_info = accounts
            .iter()
            .find(|a| a.key() == entry_key)
            .ok_or(AllowError::NotAllowed)?;
        require!(*entry_info.owner == *program_id, AllowError::NotAllowed);
        let entry = {
            let data = entry_info.try_borrow_data()?;
            let mut slice: &[u8] = &data;
            Entry::try_deserialize(&mut slice).map_err(|_| AllowError::NotAllowed)?
        };
        require!(entry.list == list_key, AllowError::NotAllowed);
        require!(entry.wallet == payload.actor, AllowError::NotAllowed);
        let allowed = if *point == POINT_BEFORE_OPEN {
            entry.may_open
        } else {
            entry.may_deposit
        };
        require!(allowed, AllowError::NotAllowed);
        if entry.expires_ts != 0 {
            let now = Clock::get()?.unix_timestamp;
            require!(now < entry.expires_ts, AllowError::Expired);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------- accounts

/// One list per permissioned pool.
#[account]
#[derive(InitSpace)]
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
impl List {
    pub const SEED: &'static [u8] = b"list";
}

/// One entry per wallet per pool.
#[account]
#[derive(InitSpace)]
pub struct Entry {
    pub version: u8,
    pub list: Pubkey,
    pub wallet: Pubkey,
    pub may_open: bool,
    pub may_deposit: bool,
    /// Unix time after which the entry no longer admits; `0` for no expiry.
    pub expires_ts: i64,
    pub added_ts: i64,
    pub bump: u8,
    pub reserved: [u8; 32],
}
impl Entry {
    pub const SEED: &'static [u8] = b"entry";
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy)]
pub struct CreateListArgs {
    pub manager: Pubkey,
    pub gate_open: bool,
    pub gate_deposit: bool,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy)]
pub struct EntryTerms {
    pub may_open: bool,
    pub may_deposit: bool,
    pub expires_ts: i64,
}

#[event_cpi]
#[derive(Accounts)]
pub struct CreateList<'info> {
    /// The AMM's global account; its `authority` is the only key that may create a list.
    #[account(seeds = [b"global"], bump = amm_global.bump, seeds::program = swap_amm::ID, has_one = authority @ AllowError::NotAuthority)]
    pub amm_global: Box<Account<'info, AmmGlobal>>,
    pub authority: Signer<'info>,
    /// The pool whose hook is this program.
    #[account(seeds = [b"pool", pool.benchmark.as_ref()], bump = pool.bump, seeds::program = swap_amm::ID)]
    pub pool: Box<Account<'info, AmmPool>>,
    #[account(init, payer = payer, space = 8 + List::INIT_SPACE, seeds = [List::SEED, pool.key().as_ref()], bump)]
    pub list: Box<Account<'info, List>>,
    #[account(mut)]
    pub payer: Signer<'info>,
    pub system_program: Program<'info, System>,
}

#[event_cpi]
#[derive(Accounts)]
pub struct AddEntry<'info> {
    #[account(mut, seeds = [List::SEED, list.pool.as_ref()], bump = list.bump, has_one = manager @ AllowError::NotManager)]
    pub list: Box<Account<'info, List>>,
    pub manager: Signer<'info>,
    /// CHECK: the wallet being admitted; any address.
    pub wallet: UncheckedAccount<'info>,
    #[account(init, payer = payer, space = 8 + Entry::INIT_SPACE, seeds = [Entry::SEED, list.pool.as_ref(), wallet.key().as_ref()], bump)]
    pub entry: Box<Account<'info, Entry>>,
    #[account(mut)]
    pub payer: Signer<'info>,
    pub system_program: Program<'info, System>,
}

#[event_cpi]
#[derive(Accounts)]
pub struct UpdateEntry<'info> {
    #[account(seeds = [List::SEED, list.pool.as_ref()], bump = list.bump, has_one = manager @ AllowError::NotManager)]
    pub list: Box<Account<'info, List>>,
    pub manager: Signer<'info>,
    #[account(mut, seeds = [Entry::SEED, list.pool.as_ref(), entry.wallet.as_ref()], bump = entry.bump, has_one = list)]
    pub entry: Box<Account<'info, Entry>>,
}

#[event_cpi]
#[derive(Accounts)]
pub struct RemoveEntry<'info> {
    #[account(mut, seeds = [List::SEED, list.pool.as_ref()], bump = list.bump, has_one = manager @ AllowError::NotManager)]
    pub list: Box<Account<'info, List>>,
    #[account(mut)]
    pub manager: Signer<'info>,
    #[account(mut, close = manager, seeds = [Entry::SEED, list.pool.as_ref(), entry.wallet.as_ref()], bump = entry.bump, has_one = list)]
    pub entry: Box<Account<'info, Entry>>,
}

#[event_cpi]
#[derive(Accounts)]
pub struct ManageList<'info> {
    #[account(mut, seeds = [List::SEED, list.pool.as_ref()], bump = list.bump, has_one = manager @ AllowError::NotManager)]
    pub list: Box<Account<'info, List>>,
    pub manager: Signer<'info>,
}

#[event_cpi]
#[derive(Accounts)]
pub struct AcceptManager<'info> {
    #[account(mut, seeds = [List::SEED, list.pool.as_ref()], bump = list.bump, has_one = pending_manager @ AllowError::NotPending)]
    pub list: Box<Account<'info, List>>,
    pub pending_manager: Signer<'info>,
}

// ---------------------------------------------------------------- events

#[event]
pub struct ListCreated {
    pub list: Pubkey,
    pub pool: Pubkey,
    pub manager: Pubkey,
    pub gate_open: bool,
    pub gate_deposit: bool,
}
#[event]
pub struct EntryChanged {
    pub list: Pubkey,
    pub wallet: Pubkey,
    pub may_open: bool,
    pub may_deposit: bool,
    pub expires_ts: i64,
    pub removed: bool,
}
#[event]
pub struct GatesChanged {
    pub list: Pubkey,
    pub gate_open: bool,
    pub gate_deposit: bool,
    pub frozen: bool,
}
#[event]
pub struct ManagerProposed {
    pub list: Pubkey,
    pub pending_manager: Pubkey,
}
#[event]
pub struct ManagerChanged {
    pub list: Pubkey,
    pub previous: Pubkey,
    pub manager: Pubkey,
}

// ---------------------------------------------------------------- errors

#[error_code]
pub enum AllowError {
    #[msg("Only the AMM's authority may create a list")]
    NotAuthority,
    #[msg("Only the list's manager may do this")]
    NotManager,
    #[msg("The pool's hook is not this program")]
    PoolNotHooked,
    #[msg("The zero key is not a valid manager")]
    ZeroKey,
    #[msg("An entry needs at least one permission")]
    NoPermission,
    #[msg("The expiry is in the past")]
    ExpiryInPast,
    #[msg("Arithmetic overflow")]
    Overflow,
    #[msg("The proposed manager is the current manager")]
    SameManager,
    #[msg("The signer is not the pending manager")]
    NotPending,
    #[msg("Malformed hook call")]
    BadCall,
    #[msg("The pool account does not match the hook payload")]
    PoolMismatch,
    #[msg("The pool's list account was not passed or does not exist")]
    ListMissing,
    #[msg("The list is frozen; no entries are admitted")]
    Frozen,
    #[msg("This wallet is not on the pool's allow-list for this action")]
    NotAllowed,
    #[msg("This wallet's entry has expired")]
    Expired,
}
