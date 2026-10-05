//! LP side of the AMM. Shares are an SPL mint priced by `tvl / supply`; the exit fee scales with the utilisation of
//! the book the leaver hands to the LPs who remain (`math::lp_exit_fee`).
//! The immediate withdrawal path serves a request only while total utilisation after it stays at or below the
//! withdraw limit (`queue::WITHDRAW_UTIL_BP`; zero in `WithdrawOnly`); otherwise it fails with
//! `UseWithdrawQueue` and the LP enqueues (`queue.rs`, ADR-009), so capacity is never first come, first served.
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

pub fn deposit(ctx: Context<LpDeposit>, amount: u64, min_shares: u64) -> Result<()> {
    let g = &ctx.accounts.global;
    require!(g.mode != OperatingMode::Halted, BrinkError::Halted);
    require!(
        g.mode != OperatingMode::WithdrawOnly,
        BrinkError::WithdrawOnly
    );
    require!(amount > 0, BrinkError::NotionalTooSmall);
    let clock = Clock::get()?;
    super::swap::apply_pending(&mut ctx.accounts.pool, clock.slot);
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
        &[],
    )?;

    let supply = ctx.accounts.share_mint.supply;
    ctx.accounts.pool.assert_share_supply(supply)?;
    let now = clock.unix_timestamp;
    // A matured swap awaiting settlement has a payoff the live mark cannot see; LP pricing waits for the crank (M-5).
    ctx.accounts.pool.require_no_matured_open(now)?;
    // Fair value: `tvl` less what the pool owes the open book (or plus what the book owes the pool), so a
    // depositor neither captures a settlement that is already visible nor pays for one that is not theirs.
    let b = &ctx.accounts.benchmark;
    let effective = ctx.accounts.pool.effective_tvl_for_deposit(
        super::swap::accrual_at(b, now)?,
        b.value_bp,
        now,
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
        &[],
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

pub fn withdraw(ctx: Context<LpWithdraw>, shares: u64, min_amount: u64) -> Result<()> {
    require!(
        ctx.accounts.global.mode != OperatingMode::Halted,
        BrinkError::Halted
    );
    require!(shares > 0, BrinkError::NotionalTooSmall);
    let clock = Clock::get()?;
    super::swap::apply_pending(&mut ctx.accounts.pool, clock.slot);
    let supply = ctx.accounts.share_mint.supply;
    ctx.accounts.pool.assert_share_supply(supply)?;
    let now = clock.unix_timestamp;
    ctx.accounts.pool.require_no_matured_open(now)?;
    // Conservative value: `tvl` less what the pool owes the open book; unrealised trader losses stay with the
    // LPs who remain until the swaps close (ADR-008). Every withdrawal, the last LP's included, is priced through
    // the virtual offsets: the former whole-balance override let a donor recover an inflation donation at a later
    // depositor's expense (external scan 1, M-7). Capital attributable to the virtual shares stays in the pool.
    let b = &ctx.accounts.benchmark;
    let effective = ctx.accounts.pool.effective_tvl_for_withdraw(
        super::swap::accrual_at(b, now)?,
        b.value_bp,
        now,
    )?;
    let gross = amount_for(shares, effective, supply)?;
    let p = &ctx.accounts.pool;
    let fee = lp_exit_fee(gross, p.open_pay_notional, p.open_rec_notional, p.tvl)?;
    let net = gross.checked_sub(fee).ok_or(BrinkError::Overflow)?;
    require!(net >= min_amount && net > 0, BrinkError::Slippage);
    // Immediate path only inside the withdraw utilisation limit; beyond it the queue serves everyone pro rata
    // (ADR-009, review F-31). Checked before any transfer so the failure is explicit and cheap.
    require!(
        super::queue::total_util_after(&ctx.accounts.pool, gross)?
            <= super::queue::immediate_limit_bp(ctx.accounts.global.mode),
        BrinkError::UseWithdrawQueue
    );
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
    let benchmark = ctx.accounts.pool.benchmark;
    let bump = ctx.accounts.pool.bump;
    let seeds: &[&[u8]] = &[b"pool", benchmark.as_ref(), &[bump]];
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
    let surplus = ctx
        .accounts
        .vault
        .amount
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
