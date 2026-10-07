//! Hook CPI. A pool may name one hook program; the pool stores which points are enabled.
//! The hook is invoked with read-only `pool` and (where relevant) `swap` accounts, no signer privileges and a
//! fixed payload, so a hook can observe and veto an entry (by returning an error) but never move funds.
//!
//! Interface surface (review F-30, ADR-003): entries only. `BeforeOpen` and `BeforeDeposit` may veto;
//! `AfterOpen` and `AfterDeposit` are notifications made after the pool account has been serialised, so the hook
//! reads post-state. No exit path (withdraw, cancel, settle, liquidate) calls a hook, in any mode: exits are
//! unconditional. The veto points run in every mode that still admits the entry (`Normal` and `Limited`): a
//! permissioned pool's participant rules hold while the protocol is merely limited (external scan 2, finding
//! 25). The notification points run in `Normal` mode only, so governance can quieten a misbehaving hook by
//! limiting the protocol, and `WithdrawOnly` and `Halted` admit no entry and make no hook CPI. Closes are
//! observable through the `SwapClosed` and `LiquidityChanged` events.
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

/// Whether a hook point runs in `mode`: veto points wherever an entry is admitted, notifications in `Normal`.
#[must_use]
pub fn runs_in(point: Point, mode: OperatingMode) -> bool {
    match mode {
        OperatingMode::Normal => true,
        OperatingMode::Limited => !point.is_after(),
        OperatingMode::WithdrawOnly | OperatingMode::Halted => false,
    }
}

/// Invokes the pool's hook at `point` if enabled and the point runs in the current mode (`runs_in`). `extra`
/// are additional read-only accounts (e.g. the swap). For `after_*` points the pool account is serialised first
/// so the hook sees the booked state rather than the pre-instruction bytes.
pub fn call<'info>(
    pool: &Account<'info, Pool>,
    mode: OperatingMode,
    hook: Option<&UncheckedAccount<'info>>,
    point: Point,
    payload: &HookPayload,
    extra: &[AccountInfo<'info>],
) -> Result<()> {
    if !runs_in(point, mode) || !enabled(pool.hooks, point) {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// External scan 2, finding 25: veto points hold in Limited mode; notifications are Normal-only; the
    /// non-admitting modes call nothing.
    #[test]
    fn veto_points_run_wherever_an_entry_is_admitted() {
        for p in [Point::BeforeOpen, Point::BeforeDeposit] {
            assert!(runs_in(p, OperatingMode::Normal));
            assert!(runs_in(p, OperatingMode::Limited));
            assert!(!runs_in(p, OperatingMode::WithdrawOnly));
            assert!(!runs_in(p, OperatingMode::Halted));
        }
        for p in [Point::AfterOpen, Point::AfterDeposit] {
            assert!(runs_in(p, OperatingMode::Normal));
            assert!(!runs_in(p, OperatingMode::Limited));
            assert!(!runs_in(p, OperatingMode::WithdrawOnly));
            assert!(!runs_in(p, OperatingMode::Halted));
        }
    }
}
