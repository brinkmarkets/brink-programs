//! Brink upgrade timelock.
//!
//! Every Brink program is deployed with the BPF upgradeable loader and its upgrade authority is set to this
//! program's `["authority"]` PDA. Nobody holds a key that can upgrade a program directly. Instead:
//!
//! ```text
//! proposer ──queue──▶ Operation{eta = now + delay} ──(delay elapses)──▶ executor ──execute──▶ loader CPI
//!                              │
//!                   proposer or guardian ──cancel──▶ Cancelled (any time before execution)
//! ```
//!
//! * `delay_slots` is at least `MIN_DELAY_SLOTS` (48 h) and can only be changed through the timelock itself.
//! * An operation expires `GRACE_SLOTS` (7 days) after its eta; expired operations cannot be executed, and anyone
//!   may cancel one to return its rent to the proposer. Executed operations are closed in the same transaction.
//! * The guardian is a separate key (hardware wallet or security council) with exactly one power: cancel.
//!   It cannot queue, execute or change the delay, so a compromised guardian can only delay, never attack.
//! * `Upgrade` operations are queued through `queue_upgrade`, which reads the buffer on chain: the buffer's
//!   authority must already be the `["authority"]` PDA, so nothing can rewrite it once queued, and the program
//!   takes the content hash itself. The hash is checked again at execution. A cancelled upgrade's buffer is
//!   closed back to the proposer through `cancel_upgrade`.
//! * `SetUpgradeAuthority` (handing a program to a new timelock or governance account) is itself timelocked, as is
//!   `SetDelay` and `SetRoles`. There is no path that bypasses the delay.
//! * `Invoke` is the generic delayed call, queued through `queue_invoke` with its full account list and
//!   instruction data as arguments: the program hashes both on chain and publishes them in the queue event, so
//!   the contents of every delayed call are disclosed the moment it is queued (external scan 2, finding 18). At
//!   execution the executor supplies the accounts and data, both hashes are recomputed and compared, and the
//!   `["authority"]` PDA signs the CPI in place of whichever account in the list is the PDA. This is how the PDA
//!   exercises `Global.authority` and `Registry.authority` (review finding F-13, ADR-011): every mode relaxation,
//!   parameter change, publisher set change, pool creation and authority transfer is visible for the full delay
//!   before it can execute.
//! * The generic `queue` accepts only the kinds that are fully specified by their arguments (`SetDelay`,
//!   `SetRoles`, `SetUpgradeAuthority`); an `Upgrade` or `Invoke` presented to it is refused.
//! * Emergency response is intentionally outside this program: the swap AMM's guardian can `Halt` the protocol
//!   instantly without a delay, which is the correct split (pausing is cheap and reversible; upgrading is neither).
//!
//! Status: deployed on devnet; audit in progress.
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
use anchor_lang::solana_program::{
    bpf_loader_upgradeable,
    instruction::{AccountMeta, Instruction},
    program::invoke_signed,
};
use solana_sha256_hasher::{hash, hashv};

declare_id!("CeQz4x7Ad7Hg715Tn4PHtvtAxYin2PHv4KGaD3KS3kM5");

/// 48 hours at ~400 ms per slot.
pub const MIN_DELAY_SLOTS: u64 = 432_000;
/// Upper bound so a mistaken delay cannot brick governance: 30 days.
pub const MAX_DELAY_SLOTS: u64 = 6_480_000;
/// Window after eta in which an operation may execute: 7 days.
pub const GRACE_SLOTS: u64 = 1_512_000;

#[program]
pub mod brink_timelock {
    use super::*;

    /// Creates the singleton timelock. Only the program's current upgrade authority (the deployer) may call it,
    /// so the first transaction after deployment cannot be front-run (review finding F-14). The deployer then
    /// sets every program's upgrade authority to the `["authority"]` PDA and hands `proposer` to governance.
    pub fn initialise(
        ctx: Context<Initialise>,
        proposer: Pubkey,
        executor: Pubkey,
        guardian: Pubkey,
        delay_slots: u64,
    ) -> Result<()> {
        require!(
            (MIN_DELAY_SLOTS..=MAX_DELAY_SLOTS).contains(&delay_slots),
            TimelockError::DelayOutOfRange
        );
        validate_roles(proposer, executor, guardian)?;
        let t = &mut ctx.accounts.timelock;
        t.proposer = proposer;
        t.executor = executor;
        t.guardian = guardian;
        t.delay_slots = delay_slots;
        t.nonce = 0;
        t.bump = ctx.bumps.timelock;
        t.authority_bump = ctx.bumps.authority;
        Ok(())
    }

    /// Queues a `SetDelay`, `SetRoles` or `SetUpgradeAuthority` operation. Only the proposer. The eta is fixed at
    /// queue time from the current delay. Upgrades and invokes, whose contents are not carried by their kind,
    /// go through `queue_upgrade` and `queue_invoke`.
    pub fn queue(ctx: Context<Queue>, kind: OperationKind) -> Result<()> {
        require!(
            !matches!(
                kind,
                OperationKind::Upgrade { .. } | OperationKind::Invoke { .. }
            ),
            TimelockError::Undisclosed
        );
        validate_kind(&kind)?;
        record(
            &mut ctx.accounts.timelock,
            &mut ctx.accounts.operation,
            ctx.bumps.operation,
            ctx.accounts.proposer.key(),
            kind,
        )?;
        let op = &ctx.accounts.operation;
        emit_cpi!(Queued {
            operation: op.key(),
            nonce: op.nonce,
            eta_slot: op.eta_slot,
            kind
        });
        Ok(())
    }

    /// Queues an `Upgrade` with the buffer disclosed: the buffer is read here, its authority must be the
    /// `["authority"]` PDA so that it cannot be rewritten after review, and its content hash is taken by the
    /// program rather than supplied by the proposer. Only the proposer.
    pub fn queue_upgrade(ctx: Context<QueueUpgrade>) -> Result<()> {
        let program = ctx.accounts.target_program.key();
        let buffer = ctx.accounts.buffer.key();
        let buffer_hash = {
            let data = ctx.accounts.buffer.try_borrow_data()?;
            require!(
                buffer_authority(&data)? == ctx.accounts.authority.key(),
                TimelockError::BufferAuthority
            );
            let body = data
                .get(bpf_loader_upgradeable::UpgradeableLoaderState::size_of_buffer_metadata()..)
                .ok_or(TimelockError::BufferMalformed)?;
            require!(!body.is_empty(), TimelockError::BufferMalformed);
            hash(body).to_bytes()
        };
        let kind = OperationKind::Upgrade {
            program,
            buffer,
            buffer_hash,
        };
        validate_kind(&kind)?;
        record(
            &mut ctx.accounts.timelock,
            &mut ctx.accounts.operation,
            ctx.bumps.operation,
            ctx.accounts.proposer.key(),
            kind,
        )?;
        let op = &ctx.accounts.operation;
        emit_cpi!(Queued {
            operation: op.key(),
            nonce: op.nonce,
            eta_slot: op.eta_slot,
            kind
        });
        Ok(())
    }

    /// Queues an `Invoke` with its contents disclosed: the full account list and the instruction data are
    /// arguments, hashed on chain and published in the queue event. Only the proposer. The `["authority"]` PDA
    /// is recorded as a signer wherever it appears, as the executor will present it.
    pub fn queue_invoke(
        ctx: Context<QueueInvoke>,
        metas: Vec<InvokeMeta>,
        data: Vec<u8>,
    ) -> Result<()> {
        let program = ctx.accounts.target_program.key();
        require!(
            ctx.accounts.target_program.executable,
            TimelockError::AccountMismatch
        );
        let pda = ctx.accounts.authority.key();
        let metas: Vec<InvokeMeta> = metas
            .into_iter()
            .map(|m| InvokeMeta {
                pubkey: m.pubkey,
                is_signer: m.is_signer || m.pubkey == pda,
                is_writable: m.is_writable,
            })
            .collect();
        for m in &metas {
            require!(
                m.pubkey != bpf_loader_upgradeable::ID,
                TimelockError::InvokeTargetForbidden
            );
        }
        let account_metas: Vec<AccountMeta> = metas
            .iter()
            .map(|m| AccountMeta {
                pubkey: m.pubkey,
                is_signer: m.is_signer,
                is_writable: m.is_writable,
            })
            .collect();
        let kind = OperationKind::Invoke {
            program,
            accounts_hash: invoke_accounts_hash(&program, &account_metas),
            data_hash: invoke_data_hash(&data),
        };
        validate_kind(&kind)?;
        record(
            &mut ctx.accounts.timelock,
            &mut ctx.accounts.operation,
            ctx.bumps.operation,
            ctx.accounts.proposer.key(),
            kind,
        )?;
        let op = &ctx.accounts.operation;
        emit_cpi!(Queued {
            operation: op.key(),
            nonce: op.nonce,
            eta_slot: op.eta_slot,
            kind
        });
        emit_cpi!(InvokeDisclosed {
            operation: op.key(),
            program,
            metas,
            data,
        });
        Ok(())
    }

    /// Cancels a queued `Upgrade` and closes its buffer, whose authority is the PDA, back to the proposer that
    /// paid for it. Same permission as `cancel`.
    pub fn cancel_upgrade(ctx: Context<CancelUpgrade>) -> Result<()> {
        let t = &ctx.accounts.timelock;
        let s = ctx.accounts.signer.key();
        let expired =
            Clock::get()?.slot > ctx.accounts.operation.eta_slot.saturating_add(GRACE_SLOTS);
        require!(
            s == t.proposer || s == t.guardian || expired,
            TimelockError::Unauthorised
        );
        let OperationKind::Upgrade { buffer, .. } = ctx.accounts.operation.kind else {
            return Err(TimelockError::KindMismatch.into());
        };
        require_keys_eq!(
            ctx.accounts.buffer.key(),
            buffer,
            TimelockError::AccountMismatch
        );
        let ix = bpf_loader_upgradeable::close(
            &buffer,
            &ctx.accounts.payer.key(),
            &ctx.accounts.authority.key(),
        );
        let seeds: &[&[u8]] = &[b"authority", &[t.authority_bump]];
        invoke_signed(
            &ix,
            &[
                ctx.accounts.buffer.to_account_info(),
                ctx.accounts.payer.to_account_info(),
                ctx.accounts.authority.to_account_info(),
            ],
            &[seeds],
        )?;
        emit_cpi!(Cancelled {
            operation: ctx.accounts.operation.key(),
            by: s
        });
        Ok(())
    }

    /// Cancels a queued operation. Proposer or guardian at any time; anyone once the operation has expired
    /// (`eta + GRACE_SLOTS` passed). The account is closed and rent returned to the proposer.
    pub fn cancel(ctx: Context<Cancel>) -> Result<()> {
        let t = &ctx.accounts.timelock;
        let s = ctx.accounts.signer.key();
        // An upgrade pins a buffer whose authority is the timelock's PDA; closing the operation without the
        // buffer would strand the buffer's lamports, so an upgrade is cancelled through `cancel_upgrade`
        // (review of external scan 2, A-5).
        require!(
            !matches!(ctx.accounts.operation.kind, OperationKind::Upgrade { .. }),
            TimelockError::KindMismatch
        );
        let expired =
            Clock::get()?.slot > ctx.accounts.operation.eta_slot.saturating_add(GRACE_SLOTS);
        require!(
            s == t.proposer || s == t.guardian || expired,
            TimelockError::Unauthorised
        );
        emit_cpi!(Cancelled {
            operation: ctx.accounts.operation.key(),
            by: s
        });
        Ok(())
    }

    /// Executes an `Upgrade` operation: CPI to the loader with the PDA as upgrade authority.
    pub fn execute_upgrade(ctx: Context<ExecuteUpgrade>) -> Result<()> {
        let op = &mut ctx.accounts.operation;
        ready(op, &ctx.accounts.timelock, ctx.accounts.executor.key())?;
        let OperationKind::Upgrade {
            program,
            buffer,
            buffer_hash,
        } = op.kind
        else {
            return Err(TimelockError::KindMismatch.into());
        };
        require_keys_eq!(
            ctx.accounts.target_program.key(),
            program,
            TimelockError::AccountMismatch
        );
        require_keys_eq!(
            ctx.accounts.buffer.key(),
            buffer,
            TimelockError::AccountMismatch
        );
        require_keys_eq!(
            ctx.accounts.program_data.key(),
            bpf_loader_upgradeable::get_program_data_address(&program),
            TimelockError::AccountMismatch
        );
        // The buffer reviewed at queue time must be the buffer executed: hash the ELF bytes past the header.
        let data = ctx.accounts.buffer.try_borrow_data()?;
        let body = data
            .get(bpf_loader_upgradeable::UpgradeableLoaderState::size_of_buffer_metadata()..)
            .ok_or(TimelockError::BufferMalformed)?;
        require!(
            hash(body).to_bytes() == buffer_hash,
            TimelockError::BufferHashMismatch
        );
        drop(data);
        let ix = bpf_loader_upgradeable::upgrade(
            &program,
            &buffer,
            &ctx.accounts.authority.key(),
            &ctx.accounts.spill.key(),
        );
        let seeds: &[&[u8]] = &[b"authority", &[ctx.accounts.timelock.authority_bump]];
        invoke_signed(
            &ix,
            &[
                ctx.accounts.program_data.to_account_info(),
                ctx.accounts.target_program.to_account_info(),
                ctx.accounts.buffer.to_account_info(),
                ctx.accounts.spill.to_account_info(),
                ctx.accounts.rent.to_account_info(),
                ctx.accounts.clock.to_account_info(),
                ctx.accounts.authority.to_account_info(),
            ],
            &[seeds],
        )?;
        op.state = OperationState::Executed;
        emit_cpi!(Executed {
            operation: op.key(),
            kind: op.kind
        });
        Ok(())
    }

    /// Executes a `SetUpgradeAuthority` operation (migration to a new timelock or governance account).
    pub fn execute_set_upgrade_authority(ctx: Context<ExecuteSetAuthority>) -> Result<()> {
        let op = &mut ctx.accounts.operation;
        ready(op, &ctx.accounts.timelock, ctx.accounts.executor.key())?;
        let OperationKind::SetUpgradeAuthority {
            program,
            new_authority,
        } = op.kind
        else {
            return Err(TimelockError::KindMismatch.into());
        };
        require_keys_eq!(
            ctx.accounts.program_data.key(),
            bpf_loader_upgradeable::get_program_data_address(&program),
            TimelockError::AccountMismatch
        );
        require_keys_eq!(
            ctx.accounts.new_authority.key(),
            new_authority,
            TimelockError::AccountMismatch
        );
        // `_checked` requires the new authority to co-sign, so a typo cannot send a program into the void.
        let ix = bpf_loader_upgradeable::set_upgrade_authority_checked(
            &program,
            &ctx.accounts.authority.key(),
            &new_authority,
        );
        let seeds: &[&[u8]] = &[b"authority", &[ctx.accounts.timelock.authority_bump]];
        invoke_signed(
            &ix,
            &[
                ctx.accounts.program_data.to_account_info(),
                ctx.accounts.authority.to_account_info(),
                ctx.accounts.new_authority.to_account_info(),
            ],
            &[seeds],
        )?;
        op.state = OperationState::Executed;
        emit_cpi!(Executed {
            operation: op.key(),
            kind: op.kind
        });
        Ok(())
    }

    /// Executes an `Invoke` operation: a CPI into `target_program` with the account list given as remaining
    /// accounts and `data` as instruction data. Both must hash to the values pinned at queue time. The
    /// `["authority"]` PDA is marked as a signer wherever it appears in the list and signs the CPI; every other
    /// signer in the list must sign the outer transaction (the runtime refuses privilege escalation), which is how
    /// co-signed transfers such as `admin_set_authority` work when the PDA is the incoming authority.
    pub fn execute_invoke<'info>(
        ctx: Context<'info, ExecuteInvoke<'info>>,
        data: Vec<u8>,
    ) -> Result<()> {
        let op = &mut ctx.accounts.operation;
        ready(op, &ctx.accounts.timelock, ctx.accounts.executor.key())?;
        let OperationKind::Invoke {
            program,
            accounts_hash,
            data_hash,
        } = op.kind
        else {
            return Err(TimelockError::KindMismatch.into());
        };
        require_keys_eq!(
            ctx.accounts.target_program.key(),
            program,
            TimelockError::AccountMismatch
        );
        require!(
            ctx.accounts.target_program.executable,
            TimelockError::AccountMismatch
        );
        let pda = ctx.accounts.authority.key();
        // The PDA's signature must not reach the loader through a relay: no account in the list may be the
        // loader itself, and no loader-owned account (program, program data, buffer) may be writable. The
        // read-only, loader-owned `program` account of an `#[event_cpi]` target stays allowed (M-2).
        for a in ctx.remaining_accounts.iter() {
            require!(
                a.key() != bpf_loader_upgradeable::ID,
                TimelockError::InvokeTargetForbidden
            );
            require!(
                !(a.is_writable && *a.owner == bpf_loader_upgradeable::ID),
                TimelockError::InvokeTargetForbidden
            );
        }
        let metas: Vec<AccountMeta> = ctx
            .remaining_accounts
            .iter()
            .map(|a| AccountMeta {
                pubkey: a.key(),
                is_signer: a.is_signer || a.key() == pda,
                is_writable: a.is_writable,
            })
            .collect();
        require!(
            invoke_accounts_hash(&program, &metas) == accounts_hash,
            TimelockError::InvokeAccountsMismatch
        );
        require!(
            hash(&data).to_bytes() == data_hash,
            TimelockError::InvokeDataMismatch
        );
        let ix = Instruction {
            program_id: program,
            accounts: metas,
            data,
        };
        let mut infos: Vec<AccountInfo<'info>> = ctx.remaining_accounts.to_vec();
        infos.push(ctx.accounts.target_program.to_account_info());
        let seeds: &[&[u8]] = &[b"authority", &[ctx.accounts.timelock.authority_bump]];
        invoke_signed(&ix, &infos, &[seeds])?;
        op.state = OperationState::Executed;
        emit_cpi!(Executed {
            operation: op.key(),
            kind: op.kind
        });
        Ok(())
    }

    /// Executes a `SetDelay` or `SetRoles` operation on the timelock itself.
    pub fn execute_config(ctx: Context<ExecuteConfig>) -> Result<()> {
        let op = &mut ctx.accounts.operation;
        ready(op, &ctx.accounts.timelock, ctx.accounts.executor.key())?;
        let t = &mut ctx.accounts.timelock;
        match op.kind {
            OperationKind::SetDelay { delay_slots } => t.delay_slots = delay_slots,
            OperationKind::SetRoles {
                proposer,
                executor,
                guardian,
            } => {
                t.proposer = proposer;
                t.executor = executor;
                t.guardian = guardian;
            }
            _ => return Err(TimelockError::KindMismatch.into()),
        }
        op.state = OperationState::Executed;
        emit_cpi!(Executed {
            operation: op.key(),
            kind: op.kind
        });
        Ok(())
    }
}

/// Roles must be real keys and the guardian must be independent of both acting roles: a default or guardian
/// proposer or executor would strand governance behind a delay nobody can satisfy (M-10, L-27).
fn validate_roles(proposer: Pubkey, executor: Pubkey, guardian: Pubkey) -> Result<()> {
    require!(
        proposer != Pubkey::default()
            && executor != Pubkey::default()
            && guardian != Pubkey::default(),
        TimelockError::RolesMustDiffer
    );
    require!(
        proposer != guardian && executor != guardian,
        TimelockError::RolesMustDiffer
    );
    Ok(())
}

/// Writes a queued operation and advances the nonce.
fn record(
    t: &mut Account<Timelock>,
    op: &mut Account<Operation>,
    bump: u8,
    payer: Pubkey,
    kind: OperationKind,
) -> Result<()> {
    let clock = Clock::get()?;
    op.timelock = t.key();
    op.nonce = t.nonce;
    op.kind = kind;
    op.queued_slot = clock.slot;
    op.eta_slot = clock
        .slot
        .checked_add(t.delay_slots)
        .ok_or(TimelockError::Overflow)?;
    op.state = OperationState::Queued;
    op.bump = bump;
    op.payer = payer;
    t.nonce = t.nonce.checked_add(1).ok_or(TimelockError::Overflow)?;
    Ok(())
}

/// The authority of a loader `Buffer` account from its serialised state: a `u32` tag of 1 followed by an
/// `Option<Pubkey>`. A buffer without an authority is immutable and cannot be used for an upgrade.
pub fn buffer_authority(data: &[u8]) -> Result<Pubkey> {
    let tag = data.get(0..4).ok_or(TimelockError::BufferMalformed)?;
    require!(tag == [1, 0, 0, 0], TimelockError::BufferMalformed);
    let some = data.get(4).ok_or(TimelockError::BufferMalformed)?;
    require!(*some == 1, TimelockError::BufferAuthority);
    let key = data.get(5..37).ok_or(TimelockError::BufferMalformed)?;
    let bytes: [u8; 32] = key.try_into().map_err(|_| TimelockError::BufferMalformed)?;
    Ok(Pubkey::new_from_array(bytes))
}

fn validate_kind(k: &OperationKind) -> Result<()> {
    match *k {
        OperationKind::SetDelay { delay_slots } => {
            require!(
                (MIN_DELAY_SLOTS..=MAX_DELAY_SLOTS).contains(&delay_slots),
                TimelockError::DelayOutOfRange
            )
        }
        OperationKind::SetRoles {
            proposer,
            executor,
            guardian,
        } => validate_roles(proposer, executor, guardian)?,
        OperationKind::Upgrade {
            program, buffer, ..
        } => require!(
            program != buffer && program != Pubkey::default() && buffer != Pubkey::default(),
            TimelockError::AccountMismatch
        ),
        OperationKind::SetUpgradeAuthority { new_authority, .. } => {
            require!(
                new_authority != Pubkey::default(),
                TimelockError::AccountMismatch
            )
        }
        // Upgrades go through `Upgrade` (buffer hash); the timelock never calls itself (the runtime forbids the
        // reentrancy anyway) and never the system program on its own behalf.
        OperationKind::Invoke { program, .. } => require!(
            program != Pubkey::default()
                && program != crate::ID
                && program != bpf_loader_upgradeable::ID
                && program != anchor_lang::system_program::ID,
            TimelockError::InvokeTargetForbidden
        ),
    }
    Ok(())
}

/// Hash of an `Invoke` account list: the target program followed by, for each account, its key and the two
/// privilege flags. Clients compute the same hash at queue time (`accounts_hash`); the executor cannot add,
/// drop, reorder or re-flag an account without changing it.
#[must_use]
pub fn invoke_accounts_hash(program: &Pubkey, metas: &[AccountMeta]) -> [u8; 32] {
    let mut parts: Vec<Vec<u8>> = Vec::with_capacity(metas.len().saturating_add(1));
    parts.push(program.to_bytes().to_vec());
    for m in metas {
        let mut b = Vec::with_capacity(34);
        b.extend_from_slice(m.pubkey.as_ref());
        b.push(u8::from(m.is_signer));
        b.push(u8::from(m.is_writable));
        parts.push(b);
    }
    let refs: Vec<&[u8]> = parts.iter().map(Vec::as_slice).collect();
    hashv(&refs).to_bytes()
}

/// Hash of `Invoke` instruction data (`data_hash`).
#[must_use]
pub fn invoke_data_hash(data: &[u8]) -> [u8; 32] {
    hash(data).to_bytes()
}

fn ready(op: &Operation, t: &Timelock, executor: Pubkey) -> Result<()> {
    require!(
        executor == t.executor || executor == t.proposer,
        TimelockError::Unauthorised
    );
    require!(op.state == OperationState::Queued, TimelockError::NotQueued);
    let slot = Clock::get()?.slot;
    require!(slot >= op.eta_slot, TimelockError::TooEarly);
    require!(
        slot <= op.eta_slot.saturating_add(GRACE_SLOTS),
        TimelockError::Expired
    );
    Ok(())
}

/// What a queued operation will do. Fully specified at queue time; nothing is decided at execution.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq, Eq, Debug, InitSpace)]
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
    /// Generic delayed CPI signed by the `["authority"]` PDA. The full account list and data are published off
    /// chain at queue time; only their hashes live on chain (same discipline as `buffer_hash`).
    Invoke {
        program: Pubkey,
        accounts_hash: [u8; 32],
        data_hash: [u8; 32],
    },
}

/// One account of a disclosed `Invoke`: the key and the privilege flags the executor will present.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub struct InvokeMeta {
    pub pubkey: Pubkey,
    pub is_signer: bool,
    pub is_writable: bool,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq, Eq, Debug, InitSpace)]
/// `Executed` and `Cancelled` are transient: the account is closed in the same transaction, so a stored
/// operation is always `Queued`. Kept for the event payloads and clients.
pub enum OperationState {
    Queued,
    Executed,
    Cancelled,
}

#[account]
#[derive(InitSpace)]
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

#[account]
#[derive(InitSpace)]
pub struct Operation {
    pub timelock: Pubkey,
    pub nonce: u64,
    pub kind: OperationKind,
    pub queued_slot: u64,
    pub eta_slot: u64,
    pub state: OperationState,
    pub bump: u8,
    /// The proposer that paid for this account; its rent returns here whatever the roles are by then (L-26).
    pub payer: Pubkey,
}

#[derive(Accounts)]
pub struct Initialise<'info> {
    #[account(init, payer = payer, space = 8 + Timelock::INIT_SPACE, seeds = [b"timelock"], bump)]
    pub timelock: Account<'info, Timelock>,
    /// CHECK: PDA that becomes the upgrade authority of every Brink program. Holds no data.
    #[account(seeds = [b"authority"], bump)]
    pub authority: UncheckedAccount<'info>,
    /// The deployer: must be this program's upgrade authority (F-14).
    #[account(mut)]
    pub payer: Signer<'info>,
    /// This program's own `ProgramData` (loader-owned, address derived from the program id); binds `payer` to
    /// the upgrade authority so nobody else can create the singleton.
    #[account(
        address = bpf_loader_upgradeable::get_program_data_address(&crate::ID) @ TimelockError::AccountMismatch,
        constraint = program_data.upgrade_authority_address == Some(payer.key()) @ TimelockError::NotUpgradeAuthority,
    )]
    pub program_data: Account<'info, ProgramData>,
    pub system_program: Program<'info, System>,
}

#[event_cpi]
#[derive(Accounts)]
pub struct Queue<'info> {
    #[account(mut, seeds = [b"timelock"], bump = timelock.bump, has_one = proposer)]
    pub timelock: Account<'info, Timelock>,
    #[account(mut)]
    pub proposer: Signer<'info>,
    #[account(init, payer = proposer, space = 8 + Operation::INIT_SPACE, seeds = [b"op", timelock.nonce.to_le_bytes().as_ref()], bump)]
    pub operation: Account<'info, Operation>,
    pub system_program: Program<'info, System>,
}

#[event_cpi]
#[derive(Accounts)]
pub struct QueueUpgrade<'info> {
    #[account(mut, seeds = [b"timelock"], bump = timelock.bump, has_one = proposer)]
    pub timelock: Account<'info, Timelock>,
    #[account(mut)]
    pub proposer: Signer<'info>,
    #[account(init, payer = proposer, space = 8 + Operation::INIT_SPACE, seeds = [b"op", timelock.nonce.to_le_bytes().as_ref()], bump)]
    pub operation: Account<'info, Operation>,
    /// CHECK: the PDA that must already be the buffer's authority.
    #[account(seeds = [b"authority"], bump = timelock.authority_bump)]
    pub authority: UncheckedAccount<'info>,
    /// CHECK: the program to upgrade; loader-owned and executable.
    #[account(executable, owner = bpf_loader_upgradeable::ID @ TimelockError::AccountMismatch)]
    pub target_program: UncheckedAccount<'info>,
    /// CHECK: loader buffer; read for its authority and content hash.
    #[account(owner = bpf_loader_upgradeable::ID @ TimelockError::AccountMismatch)]
    pub buffer: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

#[event_cpi]
#[derive(Accounts)]
pub struct QueueInvoke<'info> {
    #[account(mut, seeds = [b"timelock"], bump = timelock.bump, has_one = proposer)]
    pub timelock: Account<'info, Timelock>,
    #[account(mut)]
    pub proposer: Signer<'info>,
    #[account(init, payer = proposer, space = 8 + Operation::INIT_SPACE, seeds = [b"op", timelock.nonce.to_le_bytes().as_ref()], bump)]
    pub operation: Account<'info, Operation>,
    /// CHECK: the PDA that signs the call at execution.
    #[account(seeds = [b"authority"], bump = timelock.authority_bump)]
    pub authority: UncheckedAccount<'info>,
    /// CHECK: the program the call targets; must be executable.
    pub target_program: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

#[event_cpi]
#[derive(Accounts)]
pub struct CancelUpgrade<'info> {
    #[account(seeds = [b"timelock"], bump = timelock.bump)]
    pub timelock: Account<'info, Timelock>,
    pub signer: Signer<'info>,
    /// CHECK: rent destination for the operation and the buffer; the account that paid for the operation.
    #[account(mut, address = operation.payer @ TimelockError::AccountMismatch)]
    pub payer: UncheckedAccount<'info>,
    #[account(mut, close = payer, has_one = timelock, constraint = operation.state == OperationState::Queued @ TimelockError::NotQueued)]
    pub operation: Account<'info, Operation>,
    /// CHECK: the buffer's authority.
    #[account(seeds = [b"authority"], bump = timelock.authority_bump)]
    pub authority: UncheckedAccount<'info>,
    /// CHECK: loader buffer pinned in the operation; closed to `payer`.
    #[account(mut)]
    pub buffer: UncheckedAccount<'info>,
    /// CHECK: the upgradeable loader.
    #[account(address = bpf_loader_upgradeable::ID)]
    pub loader: UncheckedAccount<'info>,
}

#[event_cpi]
#[derive(Accounts)]
pub struct Cancel<'info> {
    #[account(seeds = [b"timelock"], bump = timelock.bump)]
    pub timelock: Account<'info, Timelock>,
    pub signer: Signer<'info>,
    /// CHECK: rent destination; the account that paid for the operation (L-26).
    #[account(mut, address = operation.payer @ TimelockError::AccountMismatch)]
    pub payer: UncheckedAccount<'info>,
    #[account(mut, close = payer, has_one = timelock, constraint = operation.state == OperationState::Queued @ TimelockError::NotQueued)]
    pub operation: Account<'info, Operation>,
}

#[event_cpi]
#[derive(Accounts)]
pub struct ExecuteUpgrade<'info> {
    #[account(seeds = [b"timelock"], bump = timelock.bump)]
    pub timelock: Account<'info, Timelock>,
    pub executor: Signer<'info>,
    /// CHECK: rent destination; the account that paid for the operation (L-26).
    #[account(mut, address = operation.payer @ TimelockError::AccountMismatch)]
    pub payer: UncheckedAccount<'info>,
    #[account(mut, has_one = timelock, close = payer)]
    pub operation: Account<'info, Operation>,
    /// CHECK: PDA upgrade authority; signs the loader CPI.
    #[account(seeds = [b"authority"], bump = timelock.authority_bump)]
    pub authority: UncheckedAccount<'info>,
    /// CHECK: ProgramData of the target, derived and compared in the handler.
    #[account(mut)]
    pub program_data: UncheckedAccount<'info>,
    /// CHECK: target program; address pinned in the operation.
    #[account(mut)]
    pub target_program: UncheckedAccount<'info>,
    /// CHECK: loader buffer; address and content hash pinned in the operation.
    #[account(mut)]
    pub buffer: UncheckedAccount<'info>,
    /// CHECK: receives the buffer's lamports after the upgrade: always the proposer that queued the upgrade,
    /// so an executor cannot redirect them (L-20).
    #[account(mut, address = operation.payer @ TimelockError::AccountMismatch)]
    pub spill: UncheckedAccount<'info>,
    pub rent: Sysvar<'info, Rent>,
    pub clock: Sysvar<'info, Clock>,
    /// CHECK: the upgradeable loader itself.
    #[account(address = bpf_loader_upgradeable::ID)]
    pub loader: UncheckedAccount<'info>,
}

#[event_cpi]
#[derive(Accounts)]
pub struct ExecuteSetAuthority<'info> {
    #[account(seeds = [b"timelock"], bump = timelock.bump)]
    pub timelock: Account<'info, Timelock>,
    pub executor: Signer<'info>,
    /// CHECK: rent destination; the account that paid for the operation (L-26).
    #[account(mut, address = operation.payer @ TimelockError::AccountMismatch)]
    pub payer: UncheckedAccount<'info>,
    #[account(mut, has_one = timelock, close = payer)]
    pub operation: Account<'info, Operation>,
    /// CHECK: PDA upgrade authority.
    #[account(seeds = [b"authority"], bump = timelock.authority_bump)]
    pub authority: UncheckedAccount<'info>,
    /// CHECK: ProgramData, derived and compared in the handler.
    #[account(mut)]
    pub program_data: UncheckedAccount<'info>,
    /// The incoming authority must co-sign (loader `_checked` variant).
    pub new_authority: Signer<'info>,
    /// CHECK: the upgradeable loader itself.
    #[account(address = bpf_loader_upgradeable::ID)]
    pub loader: UncheckedAccount<'info>,
}

#[event_cpi]
#[derive(Accounts)]
pub struct ExecuteInvoke<'info> {
    #[account(seeds = [b"timelock"], bump = timelock.bump)]
    pub timelock: Account<'info, Timelock>,
    pub executor: Signer<'info>,
    /// CHECK: rent destination; the account that paid for the operation (L-26).
    #[account(mut, address = operation.payer @ TimelockError::AccountMismatch)]
    pub payer: UncheckedAccount<'info>,
    #[account(mut, has_one = timelock, close = payer)]
    pub operation: Account<'info, Operation>,
    /// CHECK: PDA that signs the CPI; must also appear in the remaining accounts wherever the target expects it.
    #[account(seeds = [b"authority"], bump = timelock.authority_bump)]
    pub authority: UncheckedAccount<'info>,
    /// CHECK: target program; address pinned in the operation, executable checked in the handler.
    pub target_program: UncheckedAccount<'info>,
    // remaining_accounts: the account list of the inner instruction, in order, with the flags it needs.
}

#[event_cpi]
#[derive(Accounts)]
pub struct ExecuteConfig<'info> {
    #[account(mut, seeds = [b"timelock"], bump = timelock.bump)]
    pub timelock: Account<'info, Timelock>,
    pub executor: Signer<'info>,
    /// CHECK: rent destination; the account that paid for the operation (L-26).
    #[account(mut, address = operation.payer @ TimelockError::AccountMismatch)]
    pub payer: UncheckedAccount<'info>,
    #[account(mut, has_one = timelock, close = payer)]
    pub operation: Account<'info, Operation>,
}

#[event]
pub struct Queued {
    pub operation: Pubkey,
    pub nonce: u64,
    pub eta_slot: u64,
    pub kind: OperationKind,
}
#[event]
pub struct Cancelled {
    pub operation: Pubkey,
    pub by: Pubkey,
}
/// The full contents of a queued `Invoke`, published when it is queued.
#[event]
pub struct InvokeDisclosed {
    pub operation: Pubkey,
    pub program: Pubkey,
    pub metas: Vec<InvokeMeta>,
    pub data: Vec<u8>,
}
#[event]
pub struct Executed {
    pub operation: Pubkey,
    pub kind: OperationKind,
}

#[error_code]
pub enum TimelockError {
    #[msg("delay outside [48h, 30d]")]
    DelayOutOfRange,
    #[msg("roles must be non-zero keys and the guardian must differ from the proposer and the executor")]
    RolesMustDiffer,
    #[msg("signer is not permitted to do this")]
    Unauthorised,
    #[msg("operation is not in Queued state")]
    NotQueued,
    #[msg("eta not reached")]
    TooEarly,
    #[msg("operation expired (grace window passed)")]
    Expired,
    #[msg("operation kind does not match this instruction")]
    KindMismatch,
    #[msg("account does not match the operation")]
    AccountMismatch,
    #[msg("buffer account malformed")]
    BufferMalformed,
    #[msg("buffer content changed since it was queued")]
    BufferHashMismatch,
    #[msg("arithmetic overflow")]
    Overflow,
    #[msg("invoke may not target or write loader state: not the timelock, the loader, the system program, nor any writable loader-owned account")]
    InvokeTargetForbidden,
    #[msg("account list does not hash to the queued accounts_hash")]
    InvokeAccountsMismatch,
    #[msg("instruction data does not hash to the queued data_hash")]
    InvokeDataMismatch,
    #[msg("only the program's upgrade authority may initialise")]
    NotUpgradeAuthority,
    #[msg("upgrades and invokes are queued through queue_upgrade and queue_invoke, which disclose their contents")]
    Undisclosed,
    #[msg("the buffer's authority must be the timelock's authority PDA before it is queued")]
    BufferAuthority,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(k: u8, s: bool, w: bool) -> AccountMeta {
        AccountMeta {
            pubkey: Pubkey::new_from_array([k; 32]),
            is_signer: s,
            is_writable: w,
        }
    }

    #[test]
    fn accounts_hash_binds_program_order_and_flags() {
        let prog = Pubkey::new_from_array([9; 32]);
        let base = vec![meta(1, false, true), meta(2, true, false)];
        let h = invoke_accounts_hash(&prog, &base);
        assert_ne!(
            h,
            invoke_accounts_hash(&Pubkey::new_from_array([8; 32]), &base),
            "program id"
        );
        let reordered = vec![meta(2, true, false), meta(1, false, true)];
        assert_ne!(h, invoke_accounts_hash(&prog, &reordered), "order");
        let reflagged = vec![meta(1, false, false), meta(2, true, false)];
        assert_ne!(h, invoke_accounts_hash(&prog, &reflagged), "writable flag");
        let unsigned = vec![meta(1, false, true), meta(2, false, false)];
        assert_ne!(h, invoke_accounts_hash(&prog, &unsigned), "signer flag");
        let extra = vec![
            meta(1, false, true),
            meta(2, true, false),
            meta(3, false, false),
        ];
        assert_ne!(h, invoke_accounts_hash(&prog, &extra), "extra account");
        assert_eq!(h, invoke_accounts_hash(&prog, &base), "deterministic");
    }

    #[test]
    fn roles_must_be_real_and_the_guardian_independent() {
        let a = Pubkey::new_from_array([1; 32]);
        let b = Pubkey::new_from_array([2; 32]);
        let g = Pubkey::new_from_array([3; 32]);
        let z = Pubkey::default();
        assert!(validate_roles(a, b, g).is_ok());
        assert!(validate_roles(a, a, g).is_ok(), "proposer may also execute");
        // M-10: a zero proposer or executor strands governance behind a delay nobody can satisfy.
        assert!(validate_roles(z, b, g).is_err());
        assert!(validate_roles(a, z, g).is_err());
        assert!(validate_roles(a, b, z).is_err());
        // L-27: the guardian must be independent of both acting roles.
        assert!(validate_roles(g, b, g).is_err());
        assert!(validate_roles(a, g, g).is_err());
        let kind = OperationKind::SetRoles {
            proposer: a,
            executor: z,
            guardian: g,
        };
        assert!(
            validate_kind(&kind).is_err(),
            "SetRoles is held to the same rule"
        );
    }

    #[test]
    fn buffer_authority_is_read_from_the_loader_header() {
        let pda = Pubkey::new_from_array([7; 32]);
        let mut data = vec![0u8; 40];
        data[0] = 1;
        data[4] = 1;
        data[5..37].copy_from_slice(pda.as_ref());
        assert_eq!(buffer_authority(&data).unwrap(), pda);
        let mut none = data.clone();
        none[4] = 0;
        assert!(buffer_authority(&none).is_err(), "immutable buffer");
        let mut program = data.clone();
        program[0] = 2;
        assert!(buffer_authority(&program).is_err(), "not a buffer");
        assert!(buffer_authority(&data[..20]).is_err(), "short");
    }

    #[test]
    fn invoke_targets_are_restricted() {
        let ok = OperationKind::Invoke {
            program: Pubkey::new_from_array([3; 32]),
            accounts_hash: [0; 32],
            data_hash: [0; 32],
        };
        assert!(validate_kind(&ok).is_ok());
        for bad in [
            crate::ID,
            bpf_loader_upgradeable::ID,
            anchor_lang::system_program::ID,
            Pubkey::default(),
        ] {
            let k = OperationKind::Invoke {
                program: bad,
                accounts_hash: [0; 32],
                data_hash: [0; 32],
            };
            assert!(validate_kind(&k).is_err(), "{bad}");
        }
    }
}
