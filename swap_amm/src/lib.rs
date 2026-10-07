//! Brink swap AMM: a singleton program for fixed-for-floating interest-rate swaps on Solana.
//!
//! Architecture (Uniswap v4 parity, expressed in Solana's account model):
//!
//! | v4 concept            | Brink equivalent                                                                 |
//! |-----------------------|----------------------------------------------------------------------------------|
//! | Singleton PoolManager | One program, one `Global` account, every pool a PDA `["pool", benchmark]`        |
//! | Hooks                 | Optional hook program per pool invoked by CPI on entries only (observe / veto)   |
//! | Dynamic fees          | Vernier: demand spread, model spread and term are all timelocked parameters      |
//! | Custom curves         | `Pool.pricer` = this program's Vernier, or an external pricer (reserved, v1.1)   |
//! | ERC-6909 claims       | Pool shares are an SPL mint owned by the pool PDA; swaps are PDAs closed on exit |
//! | Flash accounting      | Designed (transaction-scoped ledger); not in the v1 devnet build                 |
//! | Native ETH            | USDC is the single settlement asset; no wrapped-asset hop anywhere               |
//! | Subscribers           | `emit_cpi!` events with a per-pool sequence number for indexers                  |
//!
//! Money rules. Conservation, `vault >= tvl + collateral_held + fees_held`, is asserted at the end of every instruction
//! (`Pool::assert_conservation`). The utilisation caps, per leg ≤ 48 percent and total ≤ 80 percent, are entry
//! constraints asserted only where exposure is added or LP capital removed, `trader_open_swap` and `lp_withdraw`
//! (`Pool::assert_caps`); a close is never refused by a cap.
//!
//! Status: deployed on devnet; audit in progress. Program IDs are listed in `Anchor.toml`.
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

pub mod errors;
pub mod instructions;
pub mod state;

use instructions::*;

declare_id!("EbD862QySygYKCMd8RHM2JaHqnduLQXB8Y632U3pQtce");

#[program]
pub mod swap_amm {
    use super::*;

    // ----- admin (timelock PDA as authority; guardian can only tighten) -----

    /// Creates the global account and the fee vault. Called once by the deployer, then the authority is handed
    /// to the timelock PDA with `admin_set_authority`.
    pub fn admin_initialise_global(
        ctx: Context<AdminInitialiseGlobal>,
        args: InitialiseGlobalArgs,
    ) -> Result<()> {
        instructions::admin::initialise_global(ctx, args)
    }
    /// Sets the operating mode: Normal, Limited, WithdrawOnly, Halted. The guardian may only tighten.
    pub fn admin_set_mode(ctx: Context<AdminSetMode>, mode: state::OperatingMode) -> Result<()> {
        instructions::admin::set_mode(ctx, mode)
    }
    /// Hands authority (and guardian) to new keys.
    pub fn admin_set_authority(ctx: Context<AdminSetAuthority>, guardian: Pubkey) -> Result<()> {
        instructions::admin::set_authority(ctx, guardian)
    }
    /// Publishes a new Vernier calibration that becomes effective after `Global.param_delay_slots`.
    pub fn admin_queue_calibration(
        ctx: Context<AdminQueueCalibration>,
        params: state::VernierParamsOnChain,
    ) -> Result<()> {
        instructions::admin::queue_calibration(ctx, params)
    }
    /// Withdraws a queued calibration that has not yet taken effect; a due one is installed instead.
    pub fn admin_cancel_calibration(ctx: Context<AdminQueueCalibration>) -> Result<()> {
        instructions::admin::cancel_calibration(ctx)
    }
    /// Creates a pool for a benchmark, with its share mint, vault and optional hook set.
    pub fn admin_create_pool(ctx: Context<AdminCreatePool>, args: CreatePoolArgs) -> Result<()> {
        instructions::admin::create_pool(ctx, args)
    }

    // ----- liquidity -----

    /// LP deposits USDC and receives pool shares at the current share price.
    pub fn lp_deposit<'info>(
        ctx: Context<'info, LpDeposit<'info>>,
        amount: u64,
        min_shares: u64,
    ) -> Result<()> {
        instructions::liquidity::deposit(ctx, amount, min_shares)
    }
    /// LP burns shares for USDC, paying the exit fee while swaps are open. Serves only inside the withdraw
    /// utilisation limit; beyond it fails with `UseWithdrawQueue`.
    pub fn lp_withdraw<'info>(
        ctx: Context<'info, LpWithdraw<'info>>,
        shares: u64,
        min_amount: u64,
    ) -> Result<()> {
        instructions::liquidity::withdraw(ctx, shares, min_amount)
    }

    // ----- withdrawal queue (ADR-009) -----

    /// Permissionless, once per pool: creates the withdrawal queue and the share escrow.
    pub fn init_withdraw_queue(ctx: Context<InitWithdrawQueue>) -> Result<()> {
        instructions::queue::init(ctx)
    }
    /// LP escrows shares and joins the open epoch. Priced at processing, not now.
    pub fn lp_enqueue_withdraw(
        ctx: Context<LpEnqueueWithdraw>,
        shares: u64,
        seed: u64,
    ) -> Result<()> {
        instructions::queue::enqueue(ctx, shares, seed)
    }
    /// LP takes escrowed shares back while the request's epoch is still open.
    pub fn lp_dequeue_withdraw(ctx: Context<LpDequeueWithdraw>) -> Result<()> {
        instructions::queue::dequeue(ctx)
    }
    /// Permissionless crank: closes the open epoch, fills it pro rata up to the capacity the caps leave.
    pub fn crank_process_withdrawals<'info>(
        ctx: Context<'info, CrankProcessWithdrawals<'info>>,
    ) -> Result<()> {
        instructions::queue::process(ctx)
    }
    /// Permissionless: pays a processed request to the destinations recorded at enqueue and returns its
    /// unfilled shares.
    pub fn lp_claim_withdrawal<'info>(ctx: Context<'info, LpClaimWithdrawal<'info>>) -> Result<()> {
        instructions::queue::claim(ctx)
    }

    // ----- swaps -----

    /// Trader opens a fixed-for-floating swap. The fixed rate is quoted in this slot and must be within
    /// `limit_rate_bp` or the instruction fails (the AMM equivalent of slippage protection).
    pub fn trader_open_swap<'info>(
        ctx: Context<'info, TraderOpenSwap<'info>>,
        args: OpenSwapArgs,
    ) -> Result<()> {
        instructions::swap::open(ctx, args)
    }
    /// Trader cancels early at the then-current opposite-leg quote; the difference settles against collateral.
    pub fn trader_cancel_swap(ctx: Context<CloseSwap>, min_payout: u64) -> Result<()> {
        instructions::swap::cancel(ctx, min_payout)
    }
    /// Permissionless crank: settles a matured swap against the average index and closes it.
    pub fn crank_settle_swap(ctx: Context<CloseSwap>) -> Result<()> {
        instructions::swap::settle(ctx)
    }
    /// Permissionless crank: liquidates a swap whose collateral is exhausted or within six hours of maturity.
    pub fn crank_liquidate_swap(ctx: Context<CloseSwap>) -> Result<()> {
        instructions::swap::liquidate(ctx)
    }

    // ----- basis swaps -----

    /// Governance enables a basis pair: pay fixed on `pool_a` against receive fixed on `pool_b`, with the
    /// correlation offset applied to both legs' demand charge. Absent the pair, basis swaps are refused.
    pub fn admin_set_basis_pair(
        ctx: Context<AdminSetBasisPair>,
        args: SetBasisPairArgs,
    ) -> Result<()> {
        instructions::basis::set_basis_pair(ctx, args)
    }
    /// Governance changes an existing pair's correlation offset.
    pub fn admin_update_basis_pair(
        ctx: Context<AdminUpdateBasisPair>,
        args: SetBasisPairArgs,
    ) -> Result<()> {
        instructions::basis::update_basis_pair(ctx, args)
    }
    /// Trader opens a basis swap: two linked legs, pay-fixed on pool A and receive-fixed on pool B, same tenor
    /// and notional, each collateralised in its own pool, in one transaction.
    pub fn trader_open_basis_swap<'info>(
        ctx: Context<'info, TraderOpenBasisSwap<'info>>,
        args: OpenBasisSwapArgs,
    ) -> Result<()> {
        instructions::basis::open_basis_swap(ctx, args)
    }
    /// Trader closes both legs of a basis swap early, with one floor on the net payout.
    pub fn trader_cancel_basis_swap(
        ctx: Context<TraderCancelBasisSwap>,
        min_payout: u64,
    ) -> Result<()> {
        instructions::basis::cancel_basis_swap(ctx, min_payout)
    }
    /// Trader opens a forward-starting swap: a fixed rate agreed now on the Vernier forward curve for a swap
    /// whose floating accrual begins 28, 60 or 90 days ahead.
    pub fn trader_open_forward_swap<'info>(
        ctx: Context<'info, TraderOpenForwardSwap<'info>>,
        args: OpenForwardSwapArgs,
    ) -> Result<()> {
        instructions::forward::open_forward_swap(ctx, args)
    }
    /// Permissionless: records a forward's start reading once its start has passed.
    pub fn crank_start_forward(ctx: Context<CrankStartForward>) -> Result<()> {
        instructions::forward::start_forward(ctx)
    }

    /// Permissionless: folds USDC sent directly to a pool vault into LP capital.
    pub fn sync_vault(ctx: Context<SyncVault>) -> Result<()> {
        instructions::liquidity::sync_vault(ctx)
    }

    // ----- fees -----

    /// Permissionless, per pool: moves the pool's accrued platform fees 50/50 from its vault to the buyback
    /// escrow and the treasury.
    pub fn sweep_fees(ctx: Context<SweepFees>) -> Result<()> {
        instructions::fees::sweep(ctx)
    }

    // ----- Stacked Treasuries reserve: idle LP capital at a Treasury-benchmark venue -----

    /// Gives a pool a reserve: a venue on the pool's USDC whose receipts the pool PDA holds. Authority only.
    pub fn admin_enable_reserve(
        ctx: Context<AdminEnableReserve>,
        args: EnableReserveArgs,
    ) -> Result<()> {
        instructions::reserve::enable_reserve(ctx, args)
    }
    /// Recalibrates or pauses a pool's reserve. Pausing stops placements; recalls and harvests continue.
    pub fn admin_set_reserve(ctx: Context<AdminSetReserve>, args: SetReserveArgs) -> Result<()> {
        instructions::reserve::set_reserve(ctx, args)
    }
    /// Permissionless: realises accrued yield into LP capital, then places the excess over the working band
    /// or recalls the shortfall under it. The bounty comes from harvested yield only.
    pub fn crank_rebalance_reserve(ctx: Context<CrankRebalanceReserve>) -> Result<()> {
        instructions::reserve::rebalance_reserve(ctx)
    }
}
