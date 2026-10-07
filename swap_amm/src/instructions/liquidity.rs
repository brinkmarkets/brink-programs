//! LP side of the AMM. Shares are an SPL mint priced by `tvl / supply`; the exit fee scales with the utilisation of
//! the book the leaver hands to the LPs who remain (`math::lp_exit_fee`).
//! The immediate withdrawal path serves a request only while, after it, total utilisation stays at or below
//! `queue::WITHDRAW_UTIL_BP` and each leg's at or below `queue::WITHDRAW_LEG_BP`, the pool is not in
//! `WithdrawOnly`, and the capacity left covers any queued epoch that is already eligible to be processed
//! (`queue::require_immediate_exit`); otherwise it fails with `UseWithdrawQueue` or `QueueHasPriority` and the LP
//! enqueues (`queue.rs`, ADR-009 as amended), so capacity is never first come, first served.
//! Pricing: both paths value LP capital through the open book and, on a pool with a reserve, the reserve's
//! unrealised yield (`reserve::pending_yield`), so a deposit or exit around a harvest moves no value between
//! LPs. A pool with a reserve is handed the reserve account set as the first `reserve::REMAINING_LEN` remaining
//! accounts of every LP instruction; a hook's extra accounts follow it.
use super::{
    fees,
    hooks::{self, HookPayload, Point},
    math::*,
};
use crate::{errors::BrinkError, state::*};
use anchor_lang::prelude::*;
use anchor_spl::token_interface::{
    burn, mint_to, transfer_checked, Burn, Mint, MintTo, TokenAccount, TokenInterface,
    TransferChecked,
};
use brink_index::Benchmark;

#[event_cpi]
#[derive(Accounts)]
pub struct LpDeposit<'info> {
    /// Read-only: deposits book no fee, so the shared account takes no write lock (ADR-001, review F-29).
    #[account(seeds = [b"global"], bump = global.bump, has_one = usdc_mint, has_one = token_program)]
    pub global: Box<Account<'info, Global>>,
    #[account(mut, seeds = [b"pool", pool.benchmark.as_ref()], bump = pool.bump, has_one = benchmark, has_one = share_mint, has_one = vault)]
    pub pool: Box<Account<'info, Pool>>,
    /// Read-only: prices the open book at the current accrual and published value (ADR-008, maths M-12).
    pub benchmark: Box<Account<'info, Benchmark>>,
    pub lp: Signer<'info>,
    #[account(mut, constraint = lp_usdc.owner == lp.key() @ BrinkError::TokenOwner, constraint = lp_usdc.mint == usdc_mint.key() @ BrinkError::SettlementMint)]
    pub lp_usdc: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, constraint = lp_shares.owner == lp.key() @ BrinkError::TokenOwner, constraint = lp_shares.mint == share_mint.key() @ BrinkError::TokenOwner)]
    pub lp_shares: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut)]
    pub vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut)]
    pub share_mint: Box<InterfaceAccount<'info, Mint>>,
    pub usdc_mint: Box<InterfaceAccount<'info, Mint>>,
    /// CHECK: validated against pool.hook_program in `hooks::call`.
    pub hook_program: Option<UncheckedAccount<'info>>,
    pub token_program: Interface<'info, TokenInterface>,
}

pub fn deposit<'info>(
    ctx: Context<'info, LpDeposit<'info>>,
    amount: u64,
    min_shares: u64,
) -> Result<()> {
    let g = &ctx.accounts.global;
    require!(g.mode != OperatingMode::Halted, BrinkError::Halted);
    require!(
        g.mode != OperatingMode::WithdrawOnly,
        BrinkError::WithdrawOnly
    );
    require!(amount > 0, BrinkError::NotionalTooSmall);
    let clock = Clock::get()?;
    super::swap::apply_pending(&mut ctx.accounts.pool, clock.slot);
    // A pool with a reserve is handed the reserve set first; a hook's extra accounts follow it.
    let reserve = super::reserve::required_remaining(&ctx.accounts.pool, ctx.remaining_accounts)?;
    let hook_extra = if reserve.is_some() {
        ctx.remaining_accounts
            .get(super::reserve::REMAINING_LEN..)
            .unwrap_or(&[])
    } else {
        ctx.remaining_accounts
    };
    let payload = HookPayload {
        pool: ctx.accounts.pool.key(),
        actor: ctx.accounts.lp.key(),
        amount,
        notional: 0,
        leg: None,
        tenor: 0,
    };
    hooks::call(
        &ctx.accounts.pool,
        g.mode,
        ctx.accounts.hook_program.as_ref(),
        Point::BeforeDeposit,
        &payload,
        hook_extra,
    )?;

    let supply = ctx
        .accounts
        .pool
        .reconcile_share_supply(ctx.accounts.share_mint.supply)?;
    let now = clock.unix_timestamp;
    // A matured swap awaiting settlement has a payoff the live mark cannot see; LP pricing waits for the crank (M-5).
    ctx.accounts.pool.require_no_matured_open(now)?;
    // Fair value: `tvl` less what the pool owes the open book (or plus what the book owes the pool), plus the
    // reserve's unrealised yield, so a depositor neither captures a settlement or a harvest that is already
    // visible nor pays for one that is not theirs.
    let b = &ctx.accounts.benchmark;
    let pending = super::reserve::pending_yield(reserve.as_ref(), &ctx.accounts.pool, now)?;
    let effective = ctx.accounts.pool.effective_tvl_for_deposit(
        super::swap::accrual_at(b, now)?,
        b.value_bp,
        now,
        pending,
    )?;
    let shares = shares_for(amount, effective, supply)?;
    require!(shares >= min_shares && shares > 0, BrinkError::Slippage);

    transfer_checked(
        CpiContext::new(
            ctx.accounts.token_program.key(),
            TransferChecked {
                from: ctx.accounts.lp_usdc.to_account_info(),
                mint: ctx.accounts.usdc_mint.to_account_info(),
                to: ctx.accounts.vault.to_account_info(),
                authority: ctx.accounts.lp.to_account_info(),
            },
        ),
        amount,
        ctx.accounts.usdc_mint.decimals,
    )?;
    let benchmark = ctx.accounts.pool.benchmark;
    let seeds: &[&[u8]] = &[b"pool", benchmark.as_ref(), &[ctx.accounts.pool.bump]];
    mint_to(
        CpiContext::new_with_signer(
            ctx.accounts.token_program.key(),
            MintTo {
                mint: ctx.accounts.share_mint.to_account_info(),
                to: ctx.accounts.lp_shares.to_account_info(),
                authority: ctx.accounts.pool.to_account_info(),
            },
            &[seeds],
        ),
        shares,
    )?;

    let pool = &mut ctx.accounts.pool;
    pool.tvl = pool.tvl.checked_add(amount).ok_or(BrinkError::Overflow)?;
    rebase_utilisation(pool)?;
    pool.event_seq = pool.event_seq.checked_add(1).ok_or(BrinkError::Sequence)?;
    let new_supply = supply.checked_add(shares).ok_or(BrinkError::Overflow)?;
    pool.share_supply = new_supply;
    emit_cpi!(LiquidityChanged {
        pool: pool.key(),
        lp: ctx.accounts.lp.key(),
        amount: i64::try_from(amount).map_err(|_| BrinkError::Overflow)?,
        shares: i64::try_from(shares).map_err(|_| BrinkError::Overflow)?,
        share_price_e6: share_price_e6(pool.tvl, new_supply)?,
        seq: pool.event_seq
    });
    hooks::call(
        &ctx.accounts.pool,
        ctx.accounts.global.mode,
        ctx.accounts.hook_program.as_ref(),
        Point::AfterDeposit,
        &payload,
        hook_extra,
    )?;
    ctx.accounts.vault.reload()?;
    // A deposit only adds capital and lowers utilisation; conservation is the check, so a pool that is over its
    // caps after trader gains can be recapitalised (maths finding M-4).
    ctx.accounts
        .pool
        .assert_conservation(ctx.accounts.vault.amount)
}

#[event_cpi]
#[derive(Accounts)]
pub struct LpWithdraw<'info> {
    /// Read-only: the exit fee accrues on the pool (review F-29, ADR-001).
    #[account(seeds = [b"global"], bump = global.bump, has_one = usdc_mint, has_one = token_program)]
    pub global: Box<Account<'info, Global>>,
    #[account(mut, seeds = [b"pool", pool.benchmark.as_ref()], bump = pool.bump, has_one = benchmark, has_one = share_mint, has_one = vault)]
    pub pool: Box<Account<'info, Pool>>,
    /// Read-only: prices the open book at the current accrual and published value (ADR-008, maths M-12).
    pub benchmark: Box<Account<'info, Benchmark>>,
    pub lp: Signer<'info>,
    #[account(mut, constraint = lp_usdc.owner == lp.key() @ BrinkError::TokenOwner, constraint = lp_usdc.mint == usdc_mint.key() @ BrinkError::SettlementMint)]
    pub lp_usdc: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, constraint = lp_shares.owner == lp.key() @ BrinkError::TokenOwner, constraint = lp_shares.mint == share_mint.key() @ BrinkError::TokenOwner)]
    pub lp_shares: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut)]
    pub vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut)]
    pub share_mint: Box<InterfaceAccount<'info, Mint>>,
    pub usdc_mint: Box<InterfaceAccount<'info, Mint>>,
    pub token_program: Interface<'info, TokenInterface>,
}

pub fn withdraw<'info>(
    ctx: Context<'info, LpWithdraw<'info>>,
    shares: u64,
    min_amount: u64,
) -> Result<()> {
    require!(
        ctx.accounts.global.mode != OperatingMode::Halted,
        BrinkError::Halted
    );
    require!(shares > 0, BrinkError::NotionalTooSmall);
    let clock = Clock::get()?;
    super::swap::apply_pending(&mut ctx.accounts.pool, clock.slot);
    let supply = ctx
        .accounts
        .pool
        .reconcile_share_supply(ctx.accounts.share_mint.supply)?;
    let now = clock.unix_timestamp;
    ctx.accounts.pool.require_no_matured_open(now)?;
    let mut reserve =
        super::reserve::required_remaining(&ctx.accounts.pool, ctx.remaining_accounts)?;
    // Conservative value: `tvl` less what the pool owes the open book, plus the reserve's unrealised yield;
    // unrealised trader losses stay with the LPs who remain until the swaps close (ADR-008). Every withdrawal,
    // the last LP's included, is priced through the virtual offsets: the former whole-balance override let a
    // donor recover an inflation donation at a later depositor's expense (external scan 1, M-7). Capital
    // attributable to the virtual shares stays in the pool.
    // `cap` bounds the gross at `tvl` once the reserve's yield has been realised: what remains above `tvl` then
    // is the rounding of a sub-unit of pending yield, and an exit cannot take more than the capital on the books.
    let quote = |pool: &Pool,
                 b: &Benchmark,
                 reserve: Option<&super::reserve::ReserveRemaining<'info>>,
                 cap: bool|
     -> Result<(u64, u64, u64, u64)> {
        let pending = super::reserve::pending_yield(reserve, pool, now)?;
        let effective = pool.effective_tvl_for_withdraw(
            super::swap::accrual_at(b, now)?,
            b.value_bp,
            now,
            pending,
        )?;
        let mut gross = amount_for(shares, effective, supply)?;
        if cap {
            gross = gross.min(pool.tvl);
        }
        let fee = lp_exit_fee(
            gross,
            pool.open_pay_notional,
            pool.open_rec_notional,
            pool.tvl,
        )?;
        let net = gross.checked_sub(fee).ok_or(BrinkError::Overflow)?;
        Ok((gross, fee, net, effective))
    };
    let (mut gross, mut fee, mut net, mut effective) = quote(
        &ctx.accounts.pool,
        &ctx.accounts.benchmark,
        reserve.as_ref(),
        false,
    )?;
    require!(net >= min_amount && net > 0, BrinkError::Slippage);
    let benchmark = ctx.accounts.pool.benchmark;
    let bump = ctx.accounts.pool.bump;
    let seeds: &[&[u8]] = &[b"pool", benchmark.as_ref(), &[bump]];
    // An exit priced on reserve yield not yet booked, which a large holder's is, realises that yield first so
    // `tvl` carries what the exit takes; the quote is taken again on the books as they then stand.
    if gross > ctx.accounts.pool.tvl {
        if let Some(r) = reserve.as_mut() {
            super::reserve::harvest_inline(
                r,
                &mut ctx.accounts.pool,
                &mut ctx.accounts.vault,
                &ctx.accounts.usdc_mint.to_account_info(),
                &ctx.accounts.token_program.to_account_info(),
                seeds,
                now,
            )?;
            (gross, fee, net, effective) = quote(
                &ctx.accounts.pool,
                &ctx.accounts.benchmark,
                reserve.as_ref(),
                true,
            )?;
            require!(net >= min_amount && net > 0, BrinkError::Slippage);
        }
    }
    // Immediate path only inside the withdraw limits and behind an eligible queued epoch; beyond them the queue
    // serves everyone pro rata (ADR-009, review F-31; external scan 2, findings 9, 11, 17). Checked here, before
    // any venue call, so a refusal is explicit and cheap, and again below on the final quote.
    super::queue::require_immediate_exit(
        &ctx.accounts.pool,
        ctx.accounts.global.mode,
        gross,
        effective,
        clock.slot,
    )?;
    // A pool with a reserve recalls what the working balance lacks inline, so an exit never waits for a crank.
    // The quote is taken again after a recall, since a recall can realise yield into LP capital.
    if ctx.accounts.vault.amount < net {
        if let Some(r) = reserve.as_mut() {
            let short = net
                .checked_sub(ctx.accounts.vault.amount)
                .ok_or(BrinkError::Overflow)?;
            super::reserve::recall_inline(
                r,
                &mut ctx.accounts.pool,
                &mut ctx.accounts.vault,
                &ctx.accounts.usdc_mint.to_account_info(),
                &ctx.accounts.token_program.to_account_info(),
                seeds,
                short,
            )?;
            (gross, fee, net, effective) = quote(
                &ctx.accounts.pool,
                &ctx.accounts.benchmark,
                reserve.as_ref(),
                true,
            )?;
            require!(net >= min_amount && net > 0, BrinkError::Slippage);
        }
    }
    ctx.accounts
        .pool
        .require_working(ctx.accounts.vault.amount, net)?;
    super::queue::require_immediate_exit(
        &ctx.accounts.pool,
        ctx.accounts.global.mode,
        gross,
        effective,
        clock.slot,
    )?;
    // No hook on an exit (review F-30, ADR-003).

    burn(
        CpiContext::new(
            ctx.accounts.token_program.key(),
            Burn {
                mint: ctx.accounts.share_mint.to_account_info(),
                from: ctx.accounts.lp_shares.to_account_info(),
                authority: ctx.accounts.lp.to_account_info(),
            },
        ),
        shares,
    )?;
    let decimals = ctx.accounts.usdc_mint.decimals;
    transfer_checked(
        CpiContext::new_with_signer(
            ctx.accounts.token_program.key(),
            TransferChecked {
                from: ctx.accounts.vault.to_account_info(),
                mint: ctx.accounts.usdc_mint.to_account_info(),
                to: ctx.accounts.lp_usdc.to_account_info(),
                authority: ctx.accounts.pool.to_account_info(),
            },
            &[seeds],
        ),
        net,
        decimals,
    )?;

    let pool = &mut ctx.accounts.pool;
    pool.tvl = pool.tvl.checked_sub(gross).ok_or(BrinkError::Overflow)?;
    rebase_utilisation(pool)?;
    if fee > 0 {
        // The exit fee stays in the vault and is booked on the pool (review F-29).
        let (b, t) = fees::book(pool, fees::FeeKind::LpExit, fee)?;
        emit_cpi!(FeeCollected {
            pool: pool.key(),
            kind: fees::FeeKind::LpExit,
            fee,
            buyback: b,
            treasury: t,
            seq: pool.event_seq
        });
    }
    pool.event_seq = pool.event_seq.checked_add(1).ok_or(BrinkError::Sequence)?;
    let new_supply = supply.checked_sub(shares).ok_or(BrinkError::Overflow)?;
    pool.share_supply = new_supply;
    emit_cpi!(LiquidityChanged {
        pool: pool.key(),
        lp: ctx.accounts.lp.key(),
        amount: i64::try_from(gross)
            .map_err(|_| BrinkError::Overflow)?
            .wrapping_neg(),
        shares: i64::try_from(shares)
            .map_err(|_| BrinkError::Overflow)?
            .wrapping_neg(),
        share_price_e6: share_price_e6(pool.tvl, new_supply)?,
        seq: pool.event_seq
    });
    ctx.accounts.vault.reload()?;
    // Caps are enforced here as the hard bound behind the withdraw limit.
    ctx.accounts
        .pool
        .assert_invariants(ctx.accounts.vault.amount)
}

/// Permissionless: folds any USDC sent directly to the vault (donations, airdrops, dust attacks) into LP capital so
/// the conservation invariant stays exact and the surplus benefits LPs rather than sitting unaccounted. Unswept
/// platform fees are accounted for and never folded.
#[event_cpi]
#[derive(Accounts)]
pub struct SyncVault<'info> {
    #[account(mut, seeds = [b"pool", pool.benchmark.as_ref()], bump = pool.bump, has_one = vault)]
    pub pool: Box<Account<'info, Pool>>,
    pub vault: Box<InterfaceAccount<'info, TokenAccount>>,
}

pub fn sync_vault(ctx: Context<SyncVault>) -> Result<()> {
    let pool = &mut ctx.accounts.pool;
    let accounted = pool.accounted()?;
    // Capital at the reserve venue counts as held (`Pool::assert_conservation`).
    let surplus = ctx
        .accounts
        .vault
        .amount
        .checked_add(pool.reserve_placed)
        .ok_or(BrinkError::Overflow)?
        .checked_sub(accounted)
        .ok_or(BrinkError::Conservation)?;
    require!(surplus > 0, BrinkError::NothingToSweep);
    pool.tvl = pool.tvl.checked_add(surplus).ok_or(BrinkError::Overflow)?;
    rebase_utilisation(pool)?;
    pool.event_seq = pool.event_seq.checked_add(1).ok_or(BrinkError::Sequence)?;
    emit_cpi!(VaultSynced {
        pool: pool.key(),
        surplus,
        tvl: pool.tvl,
        seq: pool.event_seq
    });
    pool.assert_conservation(ctx.accounts.vault.amount)
}

#[event]
pub struct VaultSynced {
    pub pool: Pubkey,
    pub surplus: u64,
    pub tvl: u64,
    pub seq: u64,
}
