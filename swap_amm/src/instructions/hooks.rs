//! Hook CPI. A pool may name one hook program; the pool stores which points are enabled.
//! The hook is invoked with read-only `pool` and (where relevant) `swap` accounts, no signer privileges and a
//! fixed payload, so a hook can observe and veto an entry (by returning an error) but never move funds.
//!
//! Interface surface (review F-30, ADR-003): entries only. `BeforeOpen` and `BeforeDeposit` may veto;
//! `AfterOpen` and `AfterDeposit` are notifications made after the pool account has been serialised, so the hook
//! reads post-state. No exit path (withdraw, cancel, settle, liquidate) calls a hook, in any mode: exits are
//! unconditional. Hooks are called in `Normal` mode only; `Limited`, `WithdrawOnly` and `Halted` make no hook
//! CPI, so governance can switch a misbehaving hook off without touching the pool. Closes are observable through
//! the `SwapClosed` and `LiquidityChanged` events.
use crate::{errors::BrinkError, state::*};
use anchor_lang::prelude::*;
use anchor_lang::solana_program::{
    instruction::{AccountMeta, Instruction},
    program::invoke,
};

/// Hook points. The discriminator byte is stable: the retired exit points (2, 3, 6, 7) are never sent.
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Point {
    BeforeOpen = 0,
    AfterOpen = 1,
    BeforeDeposit = 4,
    AfterDeposit = 5,
}

impl Point {
    /// Notification points run after the pool state is booked and serialised.
    #[must_use]
    pub fn is_after(self) -> bool {
        matches!(self, Point::AfterOpen | Point::AfterDeposit)
    }
}

/// Payload every hook receives after the one-byte point discriminator.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy)]
pub struct HookPayload {
    pub pool: Pubkey,
    pub actor: Pubkey,
    pub amount: u64,
    pub notional: u64,
    pub leg: Option<LegKind>,
    pub tenor: u8,
}

fn enabled(f: HookFlags, p: Point) -> bool {
    match p {
        Point::BeforeOpen => f.before_open,
        Point::AfterOpen => f.after_open,
        Point::BeforeDeposit => f.before_deposit,
        Point::AfterDeposit => f.after_deposit,
    }
}

/// Invokes the pool's hook at `point` if enabled and the protocol is in `Normal` mode. `extra` are additional
/// read-only accounts (e.g. the swap). For `after_*` points the pool account is serialised first so the hook
/// sees the booked state rather than the pre-instruction bytes.
pub fn call<'info>(
    pool: &Account<'info, Pool>,
    mode: OperatingMode,
    hook: Option<&UncheckedAccount<'info>>,
    point: Point,
    payload: &HookPayload,
    extra: &[AccountInfo<'info>],
) -> Result<()> {
    if mode != OperatingMode::Normal || !enabled(pool.hooks, point) {
        return Ok(());
    }
    let h = hook.ok_or(BrinkError::HookMismatch)?;
    require!(
        pool.hook_program != Pubkey::default() && h.key() == pool.hook_program,
        BrinkError::HookMismatch
    );
    require!(h.executable, BrinkError::HookMismatch);
    if point.is_after() {
        pool.exit(&crate::ID)?;
    }
    let mut data = Vec::with_capacity(1 + 32 + 32 + 8 + 8 + 2 + 1);
    data.push(point as u8);
    payload.serialize(&mut data)?;
    let mut metas = vec![AccountMeta::new_readonly(pool.key(), false)];
    let mut infos = vec![pool.to_account_info()];
    for a in extra {
        metas.push(AccountMeta::new_readonly(a.key(), false));
        infos.push(a.clone());
    }
    infos.push(h.to_account_info());
    invoke(
        &Instruction {
            program_id: h.key(),
            accounts: metas,
            data,
        },
        &infos,
    )
    .map_err(|_| BrinkError::HookRejected.into())
}
