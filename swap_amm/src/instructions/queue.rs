//! LP withdrawal queue (ADR-009, review F-31). A withdrawal that the immediate path cannot serve inside the
//! withdraw utilisation limit is queued and served pro rata as capacity frees, so capacity is never allocated
//! to whoever lands first.
//!
//! Shape. One `WithdrawQueue` PDA and one share escrow token account per pool, created permissionlessly by
//! `init_withdraw_queue`. `lp_enqueue_withdraw` moves shares into the escrow and records a `WithdrawRequest`
//! bound to the queue's open epoch. `crank_process_withdrawals` closes the open epoch once it has run for at
//! least `WITHDRAW_EPOCH_SLOTS` after its first request: it values the queued shares, computes the capacity the
//! utilisation caps leave, burns the filled fraction of the escrowed shares in one burn, moves the net value from
//! LP capital into `pool.withdraw_reserved`, and records `(queued, filled, net)` in a ring slot. `lp_claim_withdrawal`
//! then pays each request its pro rata share of the slot's net amount and returns its pro rata share of the
//! unfilled shares; it is permissionless and pays to any USDC and share token accounts owned by the request's LP
//! (an associated token account can be created for the LP by anyone), so a request whose recorded destinations
//! were closed cannot stall the ring (external scan 1, H-1). Requests are priced at processing, not at enqueue:
//! queued LPs share gains and losses with remaining LPs until they are served.
//!
//! The pool mirrors the open epoch (`queued_shares`, `queue_first_slot`) so that `trader_open_swap`, which does
//! not load the queue, can refuse exposure that would take the capacity an eligible epoch is waiting for (M-18).
//!
//! Every step is O(1) in the number of requests. The crank iterates nothing; a claim reads one ring slot.
use super::math::*;
use crate::{errors::BrinkError, state::*};
use anchor_lang::prelude::*;
use anchor_spl::token_interface::{
    burn, transfer_checked, Burn, Mint, TokenAccount, TokenInterface, TransferChecked,
};
use brink_index::Benchmark;

/// Total utilisation (bp of TVL) above which the immediate `lp_withdraw` path refuses and points at the queue.
/// Below the 8 000 bp open cap so a pool near its caps keeps a small immediate exit (ADR-009, decision 1).
/// In `WithdrawOnly` the limit is zero: every withdrawal goes through the queue (decision 6).
pub const WITHDRAW_UTIL_BP: u32 = 7_000;
/// Minimum length of a queue epoch, measured from the epoch's first request: about one day at 400 ms slots.
pub const WITHDRAW_EPOCH_SLOTS: u64 = 216_000;
/// Processed epochs retained for claims. A slot is reused only when every request in it has been claimed.
pub const EPOCH_RING: usize = 8;

/// Record of one processed epoch.
#[derive(
    AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq, Eq, Debug, Default, InitSpace,
)]
pub struct EpochFill {
    /// Epoch index; `u64::MAX` for a slot never used.
    pub epoch: u64,
    /// Shares queued when the epoch was processed.
    pub shares_queued: u64,
    /// Shares burned (filled) at processing.
    pub shares_filled: u64,
    /// Net USDC set aside for the epoch's requests (after the exit fee).
    pub net_amount: u64,
    /// Net USDC still unclaimed; dust is folded into LP capital when the last request claims.
    pub amount_left: u64,
    /// Unfilled shares still in escrow for this epoch's requests; dust is burned when the last request claims.
    pub shares_return_left: u64,
    /// Requests of this epoch not yet claimed.
    pub unclaimed: u32,
}

#[account]
#[derive(InitSpace)]
pub struct WithdrawQueue {
    pub pool: Pubkey,
    /// Pool-owned token account holding escrowed shares.
    pub escrow: Pubkey,
    /// Open epoch: new requests join it.
    pub epoch: u64,
    /// Slot of the first request in the open epoch; 0 when it has none.
    pub first_request_slot: u64,
    /// Shares escrowed for the open epoch.
    pub queued_shares: u64,
    /// Requests in the open epoch.
    pub request_count: u32,
    pub bump: u8,
    pub escrow_bump: u8,
    pub fills: [EpochFill; EPOCH_RING],
    pub _reserved: [u8; 64],
}

/// Ring index of an epoch.
fn ring_index(epoch: u64) -> usize {
    usize::try_from(epoch.rem_euclid(EPOCH_RING as u64)).unwrap_or(0)
}

impl WithdrawQueue {
    fn slot(&self, epoch: u64) -> Result<&EpochFill> {
        self.fills
            .get(ring_index(epoch))
            .ok_or(BrinkError::EpochEvicted.into())
    }
    fn slot_mut(&mut self, epoch: u64) -> Result<&mut EpochFill> {
        self.fills
            .get_mut(ring_index(epoch))
            .ok_or(BrinkError::EpochEvicted.into())
    }
}

#[account]
#[derive(InitSpace)]
pub struct WithdrawRequest {
    pub pool: Pubkey,
    pub lp: Pubkey,
    /// Destinations recorded at enqueue for clients; a claim accepts these or any other token accounts the LP
    /// owns (H-1).
    pub lp_usdc: Pubkey,
    pub lp_shares: Pubkey,
    pub shares: u64,
    pub epoch: u64,
    pub enqueued_slot: u64,
    pub bump: u8,
    pub _reserved: [u8; 32],
}

/// LP capital that must remain so that both utilisation caps hold for the open book, rounded up.
pub fn tvl_floor_for_caps(open_pay: u64, open_rec: u64) -> Result<u64> {
    let need = |n: u64, cap: u32| -> Result<u128> {
        Ok(u128::from(n)
            .checked_mul(BP)
            .ok_or(BrinkError::Overflow)?
            .div_ceil(u128::from(cap)))
    };
    let total = open_pay.checked_add(open_rec).ok_or(BrinkError::Overflow)?;
    let floor = need(open_pay, vernier::CAP_LEG_BP)?
        .max(need(open_rec, vernier::CAP_LEG_BP)?)
        .max(need(total, vernier::CAP_TOTAL_BP)?);
    u64::try_from(floor).map_err(|_| BrinkError::Overflow.into())
}

/// Total utilisation after a withdrawal of `gross` from `tvl`, rounded up and saturating at 10 000 bp, so that
/// any remaining exposure at all exceeds the zero limit of `WithdrawOnly` (external scan 1, M-9).
pub fn total_util_after(pool: &Pool, gross: u64) -> Result<u32> {
    let tvl = pool.tvl.checked_sub(gross).ok_or(BrinkError::Overflow)?;
    total_util_bp_ceil(pool.open_pay_notional, pool.open_rec_notional, tvl)
}

/// Refuses a new swap that would leave an eligible withdrawal epoch less capacity than it needs (M-18). An epoch
/// is eligible once it has run `WITHDRAW_EPOCH_SLOTS` from its first request; until it is processed, the capital
/// above the caps' floor is reserved for it, valued at the current share price (an upper bound of the
/// conservative price the crank will pay, so the test errs towards the queue).
pub fn require_queue_priority(
    pool: &Pool,
    leg: vernier::Leg,
    notional: u64,
    slot: u64,
) -> Result<()> {
    if pool.queued_shares == 0 || slot < pool.queue_first_slot.saturating_add(WITHDRAW_EPOCH_SLOTS)
    {
        return Ok(());
    }
    let (pay, rec) = match leg {
        vernier::Leg::Pay => (
            pool.open_pay_notional
                .checked_add(notional)
                .ok_or(BrinkError::Overflow)?,
            pool.open_rec_notional,
        ),
        vernier::Leg::Receive => (
            pool.open_pay_notional,
            pool.open_rec_notional
                .checked_add(notional)
                .ok_or(BrinkError::Overflow)?,
        ),
    };
    let floor = tvl_floor_for_caps(pay, rec)?;
    let capacity = pool.tvl.saturating_sub(floor);
    let owed = amount_for(pool.queued_shares, pool.tvl, pool.share_supply)?;
    require!(capacity >= owed, BrinkError::QueueHasPriority);
    Ok(())
}

/// Immediate-path limit for the mode (ADR-009, decisions 1 and 6).
#[must_use]
pub fn immediate_limit_bp(mode: OperatingMode) -> u32 {
    match mode {
        OperatingMode::WithdrawOnly => 0,
        _ => WITHDRAW_UTIL_BP,
    }
}

/// Shares filled when `queued` shares worth `value` meet `capacity` USDC: `min(queued, floor(queued · C / V))`.
pub fn filled_shares(queued: u64, value: u64, capacity: u64) -> Result<u64> {
    if value == 0 || capacity >= value {
        return Ok(queued);
    }
    u64::try_from(
        u128::from(queued)
            .checked_mul(u128::from(capacity))
            .ok_or(BrinkError::Overflow)?
            .checked_div(u128::from(value))
            .ok_or(BrinkError::Overflow)?,
    )
    .map_err(|_| BrinkError::Overflow.into())
}

/// `part · total / whole`, floored; zero when `whole` is zero.
pub fn pro_rata(part: u64, total: u64, whole: u64) -> Result<u64> {
    if whole == 0 {
        return Ok(0);
    }
    u64::try_from(
        u128::from(part)
            .checked_mul(u128::from(total))
            .ok_or(BrinkError::Overflow)?
            .checked_div(u128::from(whole))
            .ok_or(BrinkError::Overflow)?,
    )
    .map_err(|_| BrinkError::Overflow.into())
}

// ----- init -----

#[derive(Accounts)]
pub struct InitWithdrawQueue<'info> {
    #[account(seeds = [b"global"], bump = global.bump, has_one = token_program)]
    pub global: Box<Account<'info, Global>>,
    #[account(seeds = [b"pool", pool.benchmark.as_ref()], bump = pool.bump, has_one = share_mint)]
    pub pool: Box<Account<'info, Pool>>,
    #[account(init, payer = payer, space = 8 + WithdrawQueue::INIT_SPACE, seeds = [b"queue", pool.key().as_ref()], bump)]
    pub queue: Box<Account<'info, WithdrawQueue>>,
    #[account(init, payer = payer, seeds = [b"escrow", pool.key().as_ref()], bump, token::mint = share_mint, token::authority = pool, token::token_program = token_program)]
    pub escrow: Box<InterfaceAccount<'info, TokenAccount>>,
    pub share_mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(mut)]
    pub payer: Signer<'info>,
    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

/// Permissionless, once per pool. The pool account is read-only: the queue is a companion.
pub fn init(ctx: Context<InitWithdrawQueue>) -> Result<()> {
    let q = &mut ctx.accounts.queue;
    q.pool = ctx.accounts.pool.key();
    q.escrow = ctx.accounts.escrow.key();
    q.epoch = 0;
    q.first_request_slot = 0;
    q.queued_shares = 0;
    q.request_count = 0;
    q.bump = ctx.bumps.queue;
    q.escrow_bump = ctx.bumps.escrow;
    q.fills = [EpochFill {
        epoch: u64::MAX,
        ..EpochFill::default()
    }; EPOCH_RING];
    Ok(())
}

// ----- enqueue -----

#[event_cpi]
#[derive(Accounts)]
#[instruction(shares: u64, seed: u64)]
pub struct LpEnqueueWithdraw<'info> {
    #[account(seeds = [b"global"], bump = global.bump, has_one = token_program)]
    pub global: Box<Account<'info, Global>>,
    #[account(mut, seeds = [b"pool", pool.benchmark.as_ref()], bump = pool.bump, has_one = share_mint)]
    pub pool: Box<Account<'info, Pool>>,
    #[account(mut, seeds = [b"queue", pool.key().as_ref()], bump = queue.bump, has_one = pool, has_one = escrow)]
    pub queue: Box<Account<'info, WithdrawQueue>>,
    #[account(init, payer = lp, space = 8 + WithdrawRequest::INIT_SPACE, seeds = [b"request", pool.key().as_ref(), lp.key().as_ref(), &seed.to_le_bytes()], bump)]
    pub request: Box<Account<'info, WithdrawRequest>>,
    #[account(mut)]
    pub lp: Signer<'info>,
    #[account(mut, constraint = lp_shares.owner == lp.key() @ BrinkError::TokenOwner, constraint = lp_shares.mint == share_mint.key() @ BrinkError::TokenOwner)]
    pub lp_shares: Box<InterfaceAccount<'info, TokenAccount>>,
    /// Recorded as the claim destination; must belong to the LP and hold USDC.
    #[account(constraint = lp_usdc.owner == lp.key() @ BrinkError::TokenOwner, constraint = lp_usdc.mint == global.usdc_mint @ BrinkError::SettlementMint)]
    pub lp_usdc: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut)]
    pub escrow: Box<InterfaceAccount<'info, TokenAccount>>,
    pub share_mint: Box<InterfaceAccount<'info, Mint>>,
    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

pub fn enqueue(ctx: Context<LpEnqueueWithdraw>, shares: u64, seed: u64) -> Result<()> {
    require!(
        ctx.accounts.global.mode != OperatingMode::Halted,
        BrinkError::Halted
    );
    require!(shares > 0, BrinkError::NotionalTooSmall);
    let slot = Clock::get()?.slot;
    transfer_checked(
        CpiContext::new(
            ctx.accounts.token_program.key(),
            TransferChecked {
                from: ctx.accounts.lp_shares.to_account_info(),
                mint: ctx.accounts.share_mint.to_account_info(),
                to: ctx.accounts.escrow.to_account_info(),
                authority: ctx.accounts.lp.to_account_info(),
            },
        ),
        shares,
        ctx.accounts.share_mint.decimals,
    )?;
    let q = &mut ctx.accounts.queue;
    if q.request_count == 0 {
        q.first_request_slot = slot;
    }
    q.queued_shares = q
        .queued_shares
        .checked_add(shares)
        .ok_or(BrinkError::Overflow)?;
    q.request_count = q.request_count.checked_add(1).ok_or(BrinkError::Overflow)?;
    let pool = &mut ctx.accounts.pool;
    pool.queued_shares = q.queued_shares;
    pool.queue_first_slot = q.first_request_slot;
    let r = &mut ctx.accounts.request;
    r.pool = ctx.accounts.pool.key();
    r.lp = ctx.accounts.lp.key();
    r.lp_usdc = ctx.accounts.lp_usdc.key();
    r.lp_shares = ctx.accounts.lp_shares.key();
    r.shares = shares;
    r.epoch = q.epoch;
    r.enqueued_slot = slot;
    r.bump = ctx.bumps.request;
    emit_cpi!(WithdrawQueued {
        pool: ctx.accounts.pool.key(),
        request: r.key(),
        lp: r.lp,
        shares,
        epoch: r.epoch,
        seed,
    });
    Ok(())
}

// ----- dequeue (before processing) -----

#[event_cpi]
#[derive(Accounts)]
pub struct LpDequeueWithdraw<'info> {
    #[account(mut, seeds = [b"pool", pool.benchmark.as_ref()], bump = pool.bump, has_one = share_mint)]
    pub pool: Box<Account<'info, Pool>>,
    #[account(mut, seeds = [b"queue", pool.key().as_ref()], bump = queue.bump, has_one = pool, has_one = escrow)]
    pub queue: Box<Account<'info, WithdrawQueue>>,
    #[account(mut, close = lp, has_one = pool, has_one = lp, has_one = lp_shares)]
    pub request: Box<Account<'info, WithdrawRequest>>,
    #[account(mut)]
    pub lp: Signer<'info>,
    #[account(mut)]
    pub lp_shares: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut)]
    pub escrow: Box<InterfaceAccount<'info, TokenAccount>>,
    pub share_mint: Box<InterfaceAccount<'info, Mint>>,
    pub token_program: Interface<'info, TokenInterface>,
}

/// Returns escrowed shares while the request's epoch is still open. Allowed in every mode, including `Halted`:
/// it only hands an LP back their own shares.
pub fn dequeue(ctx: Context<LpDequeueWithdraw>) -> Result<()> {
    let r = &ctx.accounts.request;
    let q = &mut ctx.accounts.queue;
    require!(r.epoch == q.epoch, BrinkError::RequestProcessed);
    q.queued_shares = q
        .queued_shares
        .checked_sub(r.shares)
        .ok_or(BrinkError::Overflow)?;
    q.request_count = q.request_count.checked_sub(1).ok_or(BrinkError::Overflow)?;
    if q.request_count == 0 {
        q.first_request_slot = 0;
    }
    {
        let pool = &mut ctx.accounts.pool;
        pool.queued_shares = q.queued_shares;
        pool.queue_first_slot = q.first_request_slot;
    }
    let benchmark = ctx.accounts.pool.benchmark;
    let seeds: &[&[u8]] = &[b"pool", benchmark.as_ref(), &[ctx.accounts.pool.bump]];
    transfer_checked(
        CpiContext::new_with_signer(
            ctx.accounts.token_program.key(),
            TransferChecked {
                from: ctx.accounts.escrow.to_account_info(),
                mint: ctx.accounts.share_mint.to_account_info(),
                to: ctx.accounts.lp_shares.to_account_info(),
                authority: ctx.accounts.pool.to_account_info(),
            },
            &[seeds],
        ),
        r.shares,
        ctx.accounts.share_mint.decimals,
    )?;
    emit_cpi!(WithdrawDequeued {
        pool: ctx.accounts.pool.key(),
        request: r.key(),
        lp: r.lp,
        shares: r.shares,
    });
    Ok(())
}

// ----- process (permissionless crank) -----

#[event_cpi]
#[derive(Accounts)]
pub struct CrankProcessWithdrawals<'info> {
    #[account(seeds = [b"global"], bump = global.bump, has_one = token_program)]
    pub global: Box<Account<'info, Global>>,
    #[account(mut, seeds = [b"pool", pool.benchmark.as_ref()], bump = pool.bump, has_one = benchmark, has_one = share_mint, has_one = vault)]
    pub pool: Box<Account<'info, Pool>>,
    /// Prices the queued shares conservatively against the open book (ADR-008): the same value an immediate
    /// `lp_withdraw` would receive, so the queue never pays more than the exit path.
    pub benchmark: Box<Account<'info, Benchmark>>,
    #[account(mut, seeds = [b"queue", pool.key().as_ref()], bump = queue.bump, has_one = pool, has_one = escrow)]
    pub queue: Box<Account<'info, WithdrawQueue>>,
    #[account(mut)]
    pub escrow: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut)]
    pub share_mint: Box<InterfaceAccount<'info, Mint>>,
    pub vault: Box<InterfaceAccount<'info, TokenAccount>>,
    pub token_program: Interface<'info, TokenInterface>,
}

pub fn process(ctx: Context<CrankProcessWithdrawals>) -> Result<()> {
    require!(
        ctx.accounts.global.mode != OperatingMode::Halted,
        BrinkError::Halted
    );
    let clock = Clock::get()?;
    let slot = clock.slot;
    super::swap::apply_pending(&mut ctx.accounts.pool, slot);
    let q = &ctx.accounts.queue;
    require!(q.queued_shares > 0, BrinkError::NothingQueued);
    require!(
        slot >= q.first_request_slot.saturating_add(WITHDRAW_EPOCH_SLOTS),
        BrinkError::EpochNotElapsed
    );
    let epoch = q.epoch;
    // The ring slot about to be reused must have no unclaimed requests; claims are permissionless.
    require!(q.slot(epoch)?.unclaimed == 0, BrinkError::EpochUnclaimed);

    let pool = &ctx.accounts.pool;
    let supply = ctx.accounts.share_mint.supply;
    pool.assert_share_supply(supply)?;
    let queued = q.queued_shares;
    let now = clock.unix_timestamp;
    pool.require_no_matured_open(now)?;
    // Conservative value, as in `lp_withdraw`: capital less what the pool owes the open book (ADR-008), through
    // the virtual offsets for every fill including a full-supply one (external scan 1, L-21, L-24): the queue pays
    // exactly what the immediate path would for the same shares, and capital attributable to the virtual shares
    // stays in the pool rather than being left for the next depositor.
    let b = &ctx.accounts.benchmark;
    let effective =
        pool.effective_tvl_for_withdraw(super::swap::accrual_at(b, now)?, b.value_bp, now)?;
    let value = amount_for(queued, effective, supply)?;
    let floor = tvl_floor_for_caps(pool.open_pay_notional, pool.open_rec_notional)?;
    let capacity = pool.tvl.saturating_sub(floor);
    let filled = filled_shares(queued, value, capacity)?;
    require!(filled > 0, BrinkError::NoCapacity);
    let gross = amount_for(filled, effective, supply)?;
    let fee = lp_exit_fee(
        gross,
        pool.open_pay_notional,
        pool.open_rec_notional,
        pool.tvl,
    )?;
    let net = gross.checked_sub(fee).ok_or(BrinkError::Overflow)?;

    let benchmark = pool.benchmark;
    let bump = pool.bump;
    let seeds: &[&[u8]] = &[b"pool", benchmark.as_ref(), &[bump]];
    burn(
        CpiContext::new_with_signer(
            ctx.accounts.token_program.key(),
            Burn {
                mint: ctx.accounts.share_mint.to_account_info(),
                from: ctx.accounts.escrow.to_account_info(),
                authority: ctx.accounts.pool.to_account_info(),
            },
            &[seeds],
        ),
        filled,
    )?;

    let pool = &mut ctx.accounts.pool;
    pool.tvl = pool.tvl.checked_sub(gross).ok_or(BrinkError::Overflow)?;
    pool.withdraw_reserved = pool
        .withdraw_reserved
        .checked_add(net)
        .ok_or(BrinkError::Overflow)?;
    rebase_utilisation(pool)?;
    if fee > 0 {
        let (b, t) = super::fees::book(pool, super::fees::FeeKind::LpExit, fee)?;
        emit_cpi!(FeeCollected {
            pool: pool.key(),
            kind: super::fees::FeeKind::LpExit,
            fee,
            buyback: b,
            treasury: t,
            seq: pool.event_seq
        });
    }
    pool.event_seq = pool.event_seq.checked_add(1).ok_or(BrinkError::Sequence)?;
    let new_supply = supply.checked_sub(filled).ok_or(BrinkError::Overflow)?;
    pool.share_supply = new_supply;
    pool.queued_shares = 0;
    pool.queue_first_slot = 0;
    emit_cpi!(LiquidityChanged {
        pool: pool.key(),
        lp: ctx.accounts.queue.key(),
        amount: i64::try_from(gross)
            .map_err(|_| BrinkError::Overflow)?
            .wrapping_neg(),
        shares: i64::try_from(filled)
            .map_err(|_| BrinkError::Overflow)?
            .wrapping_neg(),
        share_price_e6: share_price_e6(pool.tvl, new_supply)?,
        seq: pool.event_seq
    });

    let q = &mut ctx.accounts.queue;
    let count = q.request_count;
    *q.slot_mut(epoch)? = EpochFill {
        epoch,
        shares_queued: queued,
        shares_filled: filled,
        net_amount: net,
        amount_left: net,
        shares_return_left: queued.checked_sub(filled).ok_or(BrinkError::Overflow)?,
        unclaimed: count,
    };
    q.epoch = epoch.checked_add(1).ok_or(BrinkError::Overflow)?;
    q.first_request_slot = 0;
    q.queued_shares = 0;
    q.request_count = 0;
    emit_cpi!(WithdrawEpochProcessed {
        pool: pool.key(),
        epoch,
        shares_queued: queued,
        shares_filled: filled,
        net_amount: net,
        capacity,
        requests: count,
    });
    // Caps hold by construction (gross <= capacity); conservation is asserted like every other instruction.
    pool.assert_caps()?;
    pool.assert_conservation(ctx.accounts.vault.amount)
}

// ----- claim (permissionless, pays the recorded destinations) -----

#[event_cpi]
#[derive(Accounts)]
pub struct LpClaimWithdrawal<'info> {
    #[account(seeds = [b"global"], bump = global.bump, has_one = usdc_mint, has_one = token_program)]
    pub global: Box<Account<'info, Global>>,
    #[account(mut, seeds = [b"pool", pool.benchmark.as_ref()], bump = pool.bump, has_one = share_mint, has_one = vault)]
    pub pool: Box<Account<'info, Pool>>,
    #[account(mut, seeds = [b"queue", pool.key().as_ref()], bump = queue.bump, has_one = pool, has_one = escrow)]
    pub queue: Box<Account<'info, WithdrawQueue>>,
    #[account(mut, close = lp, has_one = pool, has_one = lp)]
    pub request: Box<Account<'info, WithdrawRequest>>,
    /// CHECK: rent destination; equality with `request.lp` enforced by `has_one`. Anyone may sign the claim.
    #[account(mut)]
    pub lp: UncheckedAccount<'info>,
    pub signer: Signer<'info>,
    /// Any USDC account owned by the LP (the one recorded at enqueue, or a fresh associated token account
    /// that anyone may create for them), so a closed destination can never leave a request unclaimable (H-1).
    #[account(mut, constraint = lp_usdc.owner == lp.key() @ BrinkError::TokenOwner, constraint = lp_usdc.mint == usdc_mint.key() @ BrinkError::SettlementMint)]
    pub lp_usdc: Box<InterfaceAccount<'info, TokenAccount>>,
    /// Any share account owned by the LP, likewise.
    #[account(mut, constraint = lp_shares.owner == lp.key() @ BrinkError::TokenOwner, constraint = lp_shares.mint == share_mint.key() @ BrinkError::TokenOwner)]
    pub lp_shares: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut)]
    pub vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut)]
    pub escrow: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut)]
    pub share_mint: Box<InterfaceAccount<'info, Mint>>,
    pub usdc_mint: Box<InterfaceAccount<'info, Mint>>,
    pub token_program: Interface<'info, TokenInterface>,
}

/// Pays a processed request. Allowed in every mode including `Halted`: the money was set aside at processing
/// and belongs to the LP; paying it moves nothing that backs a swap.
pub fn claim(ctx: Context<LpClaimWithdrawal>) -> Result<()> {
    let r = &ctx.accounts.request;
    let q = &mut ctx.accounts.queue;
    require!(r.epoch < q.epoch, BrinkError::RequestNotProcessed);
    let slot = q.slot_mut(r.epoch)?;
    require!(slot.epoch == r.epoch, BrinkError::EpochEvicted);
    let paid = pro_rata(r.shares, slot.net_amount, slot.shares_queued)?.min(slot.amount_left);
    let unfilled = slot
        .shares_queued
        .checked_sub(slot.shares_filled)
        .ok_or(BrinkError::Overflow)?;
    let returned = pro_rata(r.shares, unfilled, slot.shares_queued)?.min(slot.shares_return_left);
    slot.amount_left = slot
        .amount_left
        .checked_sub(paid)
        .ok_or(BrinkError::Overflow)?;
    slot.shares_return_left = slot
        .shares_return_left
        .checked_sub(returned)
        .ok_or(BrinkError::Overflow)?;
    slot.unclaimed = slot.unclaimed.checked_sub(1).ok_or(BrinkError::Overflow)?;
    // Last claim of the epoch: rounding dust in USDC goes back to LP capital, dust shares are burned.
    let (dust_amount, dust_shares) = if slot.unclaimed == 0 {
        let d = (slot.amount_left, slot.shares_return_left);
        slot.amount_left = 0;
        slot.shares_return_left = 0;
        d
    } else {
        (0, 0)
    };

    let pool = &mut ctx.accounts.pool;
    pool.assert_share_supply(ctx.accounts.share_mint.supply)?;
    pool.share_supply = pool
        .share_supply
        .checked_sub(dust_shares)
        .ok_or(BrinkError::Overflow)?;
    pool.withdraw_reserved = pool
        .withdraw_reserved
        .checked_sub(paid)
        .and_then(|x| x.checked_sub(dust_amount))
        .ok_or(BrinkError::Overflow)?;
    pool.tvl = pool
        .tvl
        .checked_add(dust_amount)
        .ok_or(BrinkError::Overflow)?;
    rebase_utilisation(pool)?;
    pool.event_seq = pool.event_seq.checked_add(1).ok_or(BrinkError::Sequence)?;

    let benchmark = pool.benchmark;
    let bump = pool.bump;
    let seeds: &[&[u8]] = &[b"pool", benchmark.as_ref(), &[bump]];
    if paid > 0 {
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
            paid,
            ctx.accounts.usdc_mint.decimals,
        )?;
    }
    if returned > 0 {
        transfer_checked(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.key(),
                TransferChecked {
                    from: ctx.accounts.escrow.to_account_info(),
                    mint: ctx.accounts.share_mint.to_account_info(),
                    to: ctx.accounts.lp_shares.to_account_info(),
                    authority: ctx.accounts.pool.to_account_info(),
                },
                &[seeds],
            ),
            returned,
            ctx.accounts.share_mint.decimals,
        )?;
    }
    if dust_shares > 0 {
        burn(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.key(),
                Burn {
                    mint: ctx.accounts.share_mint.to_account_info(),
                    from: ctx.accounts.escrow.to_account_info(),
                    authority: ctx.accounts.pool.to_account_info(),
                },
                &[seeds],
            ),
            dust_shares,
        )?;
    }
    emit_cpi!(WithdrawClaimed {
        pool: ctx.accounts.pool.key(),
        request: ctx.accounts.request.key(),
        lp: ctx.accounts.request.lp,
        epoch: ctx.accounts.request.epoch,
        paid,
        shares_returned: returned,
        seq: ctx.accounts.pool.event_seq,
    });
    ctx.accounts.vault.reload()?;
    ctx.accounts
        .pool
        .assert_conservation(ctx.accounts.vault.amount)
}

#[event]
pub struct WithdrawQueued {
    pub pool: Pubkey,
    pub request: Pubkey,
    pub lp: Pubkey,
    pub shares: u64,
    pub epoch: u64,
    pub seed: u64,
}
#[event]
pub struct WithdrawDequeued {
    pub pool: Pubkey,
    pub request: Pubkey,
    pub lp: Pubkey,
    pub shares: u64,
}
#[event]
pub struct WithdrawEpochProcessed {
    pub pool: Pubkey,
    pub epoch: u64,
    pub shares_queued: u64,
    pub shares_filled: u64,
    pub net_amount: u64,
    pub capacity: u64,
    pub requests: u32,
}
#[event]
pub struct WithdrawClaimed {
    pub pool: Pubkey,
    pub request: Pubkey,
    pub lp: Pubkey,
    pub epoch: u64,
    pub paid: u64,
    pub shares_returned: u64,
    pub seq: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn capital_floor_covers_both_caps() {
        // 480 000 on one leg needs 1 000 000 of capital (48 percent); 400 000 + 400 000 needs 1 000 000 (80 percent).
        assert_eq!(tvl_floor_for_caps(480_000, 0).unwrap(), 1_000_000);
        assert_eq!(tvl_floor_for_caps(400_000, 400_000).unwrap(), 1_000_000);
        assert_eq!(tvl_floor_for_caps(480_000, 480_000).unwrap(), 1_200_000);
        assert_eq!(tvl_floor_for_caps(0, 0).unwrap(), 0);
        // Rounds up so the caps hold after the withdrawal.
        assert_eq!(tvl_floor_for_caps(1, 0).unwrap(), 3);
    }
    #[test]
    fn fills_are_pro_rata_and_never_exceed_capacity() {
        // 2 000 000 queued shares worth 2 000 000 USDC against 200 000 of capacity: ten percent filled.
        assert_eq!(
            filled_shares(2_000_000, 2_000_000, 200_000).unwrap(),
            200_000
        );
        assert_eq!(
            filled_shares(2_000_000, 2_000_000, 2_000_000).unwrap(),
            2_000_000
        );
        assert_eq!(filled_shares(2_000_000, 2_000_000, 0).unwrap(), 0);
        assert_eq!(filled_shares(7, 0, 0).unwrap(), 7);
        // Two equal requests in one epoch receive equal amounts: no first-mover prize.
        let (net, queued) = (199_000u64, 2_000_000u64);
        let a = pro_rata(1_000_000, net, queued).unwrap();
        let b = pro_rata(1_000_000, net, queued).unwrap();
        assert_eq!(a, b);
        assert!(a + b <= net);
        // Three unequal requests: floors sum to at most the total, so the reserve never goes negative.
        let parts = [1u64, 999_999, 1_000_000];
        let sum: u64 = parts
            .iter()
            .map(|p| pro_rata(*p, net, queued).unwrap())
            .sum();
        assert!(sum <= net && net - sum < parts.len() as u64);
    }
    #[test]
    fn immediate_limit_is_zero_in_withdraw_only() {
        assert_eq!(immediate_limit_bp(OperatingMode::WithdrawOnly), 0);
        assert_eq!(immediate_limit_bp(OperatingMode::Normal), WITHDRAW_UTIL_BP);
        assert_eq!(immediate_limit_bp(OperatingMode::Limited), WITHDRAW_UTIL_BP);
    }
}
