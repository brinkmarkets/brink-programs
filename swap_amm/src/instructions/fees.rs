//! Platform fee routing. Every platform fee (opening, income, LP exit) is split in the collecting instruction:
//! 50 percent to the BRINK buyback escrow, 50 percent to the treasury. The split is a program constant so it cannot
//! be changed by a parameter update; changing it means a program upgrade through the timelock.
//!
//! Fees stay in the collecting pool's vault and accrue on the `Pool` account (review F-29, ADR-001, ADR-013), so
//! the hot path takes no write lock on the singleton `Global` or on a shared fee account and two pools'
//! transactions never serialise on each other. `sweep_fees` is permissionless and per pool: it moves the two
//! accrued halves from the pool vault to their destinations, so no key has to be online for fees to be collected
//! and no key can redirect them. The conservation invariant counts unswept fees:
//! `vault >= tvl + collateral_held + fees_held`.
use crate::{errors::BrinkError, state::*};
use anchor_lang::prelude::*;
use anchor_spl::token_interface::{
    transfer_checked, Mint, TokenAccount, TokenInterface, TransferChecked,
};

/// Buyback share in bp of every platform fee. `TREASURY_BP = 10_000 − BUYBACK_BP`.
pub const BUYBACK_BP: u64 = 5_000;
pub const TREASURY_BP: u64 = 10_000 - BUYBACK_BP;

/// Fee kinds, for events and the conservation invariant.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum FeeKind {
    Opening,
    Income,
    LpExit,
}

/// Pure split. The buyback share rounds down; the treasury receives the remainder, so `buyback + treasury == fee` always.
#[must_use]
pub fn split(fee: u64) -> (u64, u64) {
    let buyback =
        u64::try_from(u128::from(fee).saturating_mul(u128::from(BUYBACK_BP)) / 10_000).unwrap_or(0);
    (buyback, fee.saturating_sub(buyback))
}

/// Books a collected fee against the pool's accruals and lifetime counter. The caller has already ensured `fee`
/// USDC is in the pool vault (paid in by the trader on open; retained from the payout on close and LP exit).
pub fn book(pool: &mut Pool, kind: FeeKind, fee: u64) -> Result<(u64, u64)> {
    let (buyback, treasury) = split(fee);
    pool.fees_buyback_accrued = pool
        .fees_buyback_accrued
        .checked_add(buyback)
        .ok_or(BrinkError::Overflow)?;
    pool.fees_treasury_accrued = pool
        .fees_treasury_accrued
        .checked_add(treasury)
        .ok_or(BrinkError::Overflow)?;
    pool.fees_lifetime = pool
        .fees_lifetime
        .checked_add(fee)
        .ok_or(BrinkError::Overflow)?;
    pool.event_seq = pool.event_seq.checked_add(1).ok_or(BrinkError::Sequence)?;
    let _ = kind;
    Ok((buyback, treasury))
}

#[event_cpi]
#[derive(Accounts)]
pub struct SweepFees<'info> {
    /// Written only here (lifetime counters); never on the hot path.
    #[account(mut, seeds = [b"global"], bump = global.bump, has_one = treasury, has_one = buyback_escrow, has_one = usdc_mint, has_one = token_program)]
    pub global: Box<Account<'info, Global>>,
    #[account(mut, seeds = [b"pool", pool.benchmark.as_ref()], bump = pool.bump, has_one = vault)]
    pub pool: Box<Account<'info, Pool>>,
    #[account(mut)]
    pub vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut)]
    pub treasury: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut)]
    pub buyback_escrow: Box<InterfaceAccount<'info, TokenAccount>>,
    pub usdc_mint: Box<InterfaceAccount<'info, Mint>>,
    pub token_program: Interface<'info, TokenInterface>,
}

/// Permissionless. Moves one pool's accrued halves from its vault to the treasury and the buyback escrow.
pub fn sweep(ctx: Context<SweepFees>) -> Result<()> {
    let pool = &mut ctx.accounts.pool;
    let (b, t) = (pool.fees_buyback_accrued, pool.fees_treasury_accrued);
    require!(b > 0 || t > 0, BrinkError::NothingToSweep);
    pool.fees_buyback_accrued = 0;
    pool.fees_treasury_accrued = 0;
    let benchmark = pool.benchmark;
    let bump = pool.bump;
    let seeds: &[&[u8]] = &[b"pool", benchmark.as_ref(), &[bump]];
    let decimals = ctx.accounts.usdc_mint.decimals;
    for (amount, to) in [
        (b, ctx.accounts.buyback_escrow.to_account_info()),
        (t, ctx.accounts.treasury.to_account_info()),
    ] {
        if amount == 0 {
            continue;
        }
        transfer_checked(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.key(),
                TransferChecked {
                    from: ctx.accounts.vault.to_account_info(),
                    mint: ctx.accounts.usdc_mint.to_account_info(),
                    to,
                    authority: ctx.accounts.pool.to_account_info(),
                },
                &[seeds],
            ),
            amount,
            decimals,
        )?;
    }
    let g = &mut ctx.accounts.global;
    g.buyback_lifetime = g
        .buyback_lifetime
        .checked_add(b)
        .ok_or(BrinkError::Overflow)?;
    g.treasury_lifetime = g
        .treasury_lifetime
        .checked_add(t)
        .ok_or(BrinkError::Overflow)?;
    let pool = &mut ctx.accounts.pool;
    pool.event_seq = pool.event_seq.checked_add(1).ok_or(BrinkError::Sequence)?;
    emit_cpi!(FeesSwept {
        pool: pool.key(),
        buyback: b,
        treasury: t
    });
    ctx.accounts.vault.reload()?;
    ctx.accounts
        .pool
        .assert_conservation(ctx.accounts.vault.amount)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn split_conserves_and_is_half() {
        for fee in [0u64, 1, 2, 3, 999, 1_000_000, u64::MAX / 3] {
            let (b, t) = split(fee);
            assert_eq!(b + t, fee);
            assert!(b <= t && t - b <= 1);
        }
    }
}
