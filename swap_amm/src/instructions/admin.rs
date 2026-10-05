//! Admin instructions. `authority` is the timelock PDA; `guardian` is the fast path and may only tighten the mode
//! as far as `WithdrawOnly`. A pause blocks entries, never exits: only the timelocked authority may reach `Halted`.
use crate::{errors::BrinkError, state::*};
use anchor_lang::prelude::*;
use anchor_lang::solana_program::bpf_loader_upgradeable;
use anchor_spl::token_interface::{Mint, TokenAccount, TokenInterface};

#[derive(AnchorSerialize, AnchorDeserialize, Clone)]
pub struct InitialiseGlobalArgs {
    pub guardian: Pubkey,
    pub param_delay_slots: u64,
    pub limited_mode_cap: u64,
}

#[derive(Accounts)]
pub struct AdminInitialiseGlobal<'info> {
    #[account(init, payer = payer, space = 8 + Global::INIT_SPACE, seeds = [b"global"], bump)]
    pub global: Box<Account<'info, Global>>,
    pub authority: Signer<'info>,
    /// The deployer: must be this program's upgrade authority (review finding F-14), so the singleton cannot be
    /// created by whoever lands the first transaction after deployment.
    #[account(mut)]
    pub payer: Signer<'info>,
    /// This program's own `ProgramData` (loader-owned, address derived from the program id).
    #[account(
        address = bpf_loader_upgradeable::get_program_data_address(&crate::ID) @ BrinkError::NotUpgradeAuthority,
        constraint = program_data.upgrade_authority_address == Some(payer.key()) @ BrinkError::NotUpgradeAuthority,
    )]
    pub program_data: Account<'info, ProgramData>,
    /// Canonical USDC (mainnet or devnet); checked by address in the handler. Decimals must be 6.
    #[account(constraint = usdc_mint.decimals == 6 @ BrinkError::SettlementMint)]
    pub usdc_mint: Box<InterfaceAccount<'info, Mint>>,
    /// Unswept fees. Owned by the global PDA.
    #[account(init, payer = payer, seeds = [b"fee_vault"], bump, token::mint = usdc_mint, token::authority = global, token::token_program = token_program)]
    pub fee_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(constraint = treasury.mint == usdc_mint.key() @ BrinkError::SettlementMint)]
    pub treasury: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(constraint = buyback_escrow.mint == usdc_mint.key() @ BrinkError::SettlementMint)]
    pub buyback_escrow: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(constraint = token_program.key() == *usdc_mint.to_account_info().owner @ BrinkError::TokenProgram, constraint = token_program.key() == anchor_spl::token::ID @ BrinkError::TokenProgram)]
    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

pub fn initialise_global(
    ctx: Context<AdminInitialiseGlobal>,
    a: InitialiseGlobalArgs,
) -> Result<()> {
    require!(
        crate::instructions::swap::is_usdc_mint(&ctx.accounts.usdc_mint.key()),
        BrinkError::SettlementMint
    );
    // A zero guardian would leave no fast path to pause (external scan 1, L-25).
    require!(
        a.guardian != ctx.accounts.authority.key() && a.guardian != Pubkey::default(),
        BrinkError::GuardianScope
    );
    let g = &mut ctx.accounts.global;
    g.authority = ctx.accounts.authority.key();
    g.guardian = a.guardian;
    g.usdc_mint = ctx.accounts.usdc_mint.key();
    g.token_program = ctx.accounts.token_program.key();
    g.treasury = ctx.accounts.treasury.key();
    g.buyback_escrow = ctx.accounts.buyback_escrow.key();
    g.fee_vault = ctx.accounts.fee_vault.key();
    g.buyback_accrued = 0;
    g.treasury_accrued = 0;
    g.buyback_lifetime = 0;
    g.treasury_lifetime = 0;
    g.mode = OperatingMode::Normal;
    g.param_delay_slots = a.param_delay_slots;
    g.limited_mode_cap = a.limited_mode_cap;
    g.pool_count = 0;
    g.bump = ctx.bumps.global;
    Ok(())
}

#[event_cpi]
#[derive(Accounts)]
pub struct AdminSetMode<'info> {
    #[account(mut, seeds = [b"global"], bump = global.bump)]
    pub global: Box<Account<'info, Global>>,
    /// Authority may set any mode; guardian may only tighten, and never past WithdrawOnly (checked in handler).
    pub signer: Signer<'info>,
}

fn severity(m: OperatingMode) -> u8 {
    match m {
        OperatingMode::Normal => 0,
        OperatingMode::Limited => 1,
        OperatingMode::WithdrawOnly => 2,
        OperatingMode::Halted => 3,
    }
}

pub fn set_mode(ctx: Context<AdminSetMode>, mode: OperatingMode) -> Result<()> {
    let g = &mut ctx.accounts.global;
    let s = ctx.accounts.signer.key();
    if s == g.authority {
        g.mode = mode;
    } else if s == g.guardian {
        require!(severity(mode) > severity(g.mode), BrinkError::GuardianScope);
        require!(
            severity(mode) <= severity(OperatingMode::WithdrawOnly),
            BrinkError::GuardianScope
        );
        g.mode = mode;
    } else {
        return Err(ErrorCode::ConstraintSigner.into());
    }
    emit_cpi!(ModeChanged { mode, by: s });
    Ok(())
}

#[derive(Accounts)]
pub struct AdminSetAuthority<'info> {
    #[account(mut, seeds = [b"global"], bump = global.bump, has_one = authority)]
    pub global: Box<Account<'info, Global>>,
    pub authority: Signer<'info>,
    /// The incoming authority co-signs, so a mistyped key cannot take governance (review finding F-17).
    pub new_authority: Signer<'info>,
}

/// Hands the protocol to a new authority (bootstrap → governance). Both the current and the new authority sign.
pub fn set_authority(ctx: Context<AdminSetAuthority>, guardian: Pubkey) -> Result<()> {
    let new_authority = ctx.accounts.new_authority.key();
    require!(
        new_authority != guardian
            && new_authority != Pubkey::default()
            && guardian != Pubkey::default(),
        BrinkError::GuardianScope
    );
    let g = &mut ctx.accounts.global;
    g.authority = new_authority;
    g.guardian = guardian;
    Ok(())
}

#[event_cpi]
#[derive(Accounts)]
pub struct AdminQueueCalibration<'info> {
    #[account(seeds = [b"global"], bump = global.bump, has_one = authority)]
    pub global: Box<Account<'info, Global>>,
    #[account(mut, seeds = [b"pool", pool.benchmark.as_ref()], bump = pool.bump)]
    pub pool: Box<Account<'info, Pool>>,
    pub authority: Signer<'info>,
}

/// Each parameter may move at most 50 percent per queue, so a compromised key cannot reprice the book in one step.
/// Computed in `u32` so the bound is exact over the whole `u16` domain (maths finding M-13: the previous
/// `saturating_mul` rejected the identity above 21 845).
pub(crate) fn within_step(old: u16, new: u16) -> bool {
    let lo = u32::from(old) / 2;
    let hi = (u32::from(old).saturating_mul(3) / 2).saturating_add(1);
    u32::from(new) >= lo && u32::from(new) <= hi
}

/// Collateral floors per tenor, bp of notional. The aggregate book mark is exact only while no position's
/// unbounded value exceeds its collateral (external scan 1, M-4); a floor keeps the rate move needed to reach the
/// collateral at least about 6.5 percentage points annualised over the tenor, and the cap-out close in
/// `liquidate` handles the rest. Devnet calibration is 120, 230, 330, 600.
pub const MIN_COLLATERAL_BP: [u16; 4] = [50, 100, 150, 300];

/// Static bounds every calibration must satisfy, at creation and when queued (maths finding M-13).
/// Collateral is a fraction of notional, so at most 100 percent; the spreads and the demand slope are at least
/// 1 bp (zero would price a leg at the benchmark with no edge, or switch the demand term off); the tables are
/// non-decreasing in tenor (a longer swap is never cheaper to open or to collateralise than a shorter one).
pub(crate) fn check_calibration_bounds(p: &VernierParamsOnChain) -> Result<()> {
    require!(
        p.collateral_bp
            .iter()
            .zip(MIN_COLLATERAL_BP.iter())
            .all(|(c, floor)| *c >= *floor && *c <= 10_000),
        BrinkError::CalibrationBound
    );
    require!(
        p.model_pay_bp.iter().all(|x| *x > 0)
            && p.model_rec_bp.iter().all(|x| *x > 0)
            && p.term_bp.iter().all(|x| *x > 0)
            && p.demand_k_bp > 0
            && p.demand_cap_bp > 0,
        BrinkError::CalibrationBound
    );
    let non_decreasing = |t: &[u16; 4]| t.iter().zip(t.iter().skip(1)).all(|(a, b)| a <= b);
    require!(
        non_decreasing(&p.model_pay_bp)
            && non_decreasing(&p.model_rec_bp)
            && non_decreasing(&p.term_bp)
            && non_decreasing(&p.collateral_bp),
        BrinkError::CalibrationOrder
    );
    Ok(())
}

pub fn queue_calibration(
    ctx: Context<AdminQueueCalibration>,
    p: VernierParamsOnChain,
) -> Result<()> {
    let pool = &mut ctx.accounts.pool;
    let cur = pool.params;
    let tables = [
        (cur.model_pay_bp, p.model_pay_bp),
        (cur.model_rec_bp, p.model_rec_bp),
        (cur.term_bp, p.term_bp),
        (cur.collateral_bp, p.collateral_bp),
    ];
    for (old, new) in &tables {
        require!(
            old.iter().zip(new.iter()).all(|(o, n)| within_step(*o, *n)),
            BrinkError::CalibrationStep
        );
    }
    require!(
        within_step(cur.demand_k_bp, p.demand_k_bp)
            && within_step(cur.demand_cap_bp, p.demand_cap_bp),
        BrinkError::CalibrationStep
    );
    check_calibration_bounds(&p)?;
    let effective = Clock::get()?
        .slot
        .checked_add(ctx.accounts.global.param_delay_slots)
        .ok_or(BrinkError::Overflow)?;
    pool.pending_params = p;
    pool.pending_effective_slot = effective;
    emit_cpi!(CalibrationQueued {
        pool: pool.key(),
        effective_slot: effective
    });
    Ok(())
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone)]
pub struct CreatePoolArgs {
    pub params: VernierParamsOnChain,
    pub hook_program: Option<Pubkey>,
    pub hooks: HookFlags,
    pub pricer: Pricer,
    pub min_notional: u64,
    pub max_notional: u64,
}

#[event_cpi]
#[derive(Accounts)]
pub struct AdminCreatePool<'info> {
    #[account(mut, seeds = [b"global"], bump = global.bump, has_one = authority, has_one = usdc_mint, has_one = token_program)]
    pub global: Box<Account<'info, Global>>,
    pub authority: Signer<'info>,
    #[account(mut)]
    pub payer: Signer<'info>,
    /// CHECK: benchmark account owned by the index program; ownership checked by constraint.
    #[account(owner = crate::instructions::swap::INDEX_PROGRAM_ID @ BrinkError::BenchmarkNotPublished)]
    pub benchmark: UncheckedAccount<'info>,
    #[account(init, payer = payer, space = 8 + Pool::INIT_SPACE, seeds = [b"pool", benchmark.key().as_ref()], bump)]
    pub pool: Box<Account<'info, Pool>>,
    #[account(init, payer = payer, seeds = [b"shares", pool.key().as_ref()], bump, mint::decimals = crate::instructions::math::SHARE_DECIMALS, mint::authority = pool, mint::token_program = token_program)]
    pub share_mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(init, payer = payer, seeds = [b"vault", pool.key().as_ref()], bump, token::mint = usdc_mint, token::authority = pool, token::token_program = token_program)]
    pub vault: Box<InterfaceAccount<'info, TokenAccount>>,
    pub usdc_mint: Box<InterfaceAccount<'info, Mint>>,
    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

pub fn create_pool(ctx: Context<AdminCreatePool>, a: CreatePoolArgs) -> Result<()> {
    if let Pricer::External { program } = a.pricer {
        require!(
            program != crate::ID && program != Pubkey::default(),
            BrinkError::PricerMismatch
        );
    }
    if let Some(h) = a.hook_program {
        require!(
            h != crate::ID && h != Pubkey::default(),
            BrinkError::HookMismatch
        );
        // Exits never call hooks (review F-30, ADR-003); the retired flag slots must stay clear.
        require!(a.hooks.exit_points_clear(), BrinkError::HookExitPoint);
    }
    require!(
        a.min_notional > 0 && a.max_notional >= a.min_notional,
        BrinkError::NotionalTooSmall
    );
    check_calibration_bounds(&a.params)?;
    let pool = &mut ctx.accounts.pool;
    pool.benchmark = ctx.accounts.benchmark.key();
    pool.share_mint = ctx.accounts.share_mint.key();
    pool.vault = ctx.accounts.vault.key();
    pool.params = a.params;
    pool.pending_params = a.params;
    pool.pending_effective_slot = 0;
    pool.hook_program = a.hook_program.unwrap_or_default();
    pool.hooks = if a.hook_program.is_some() {
        a.hooks
    } else {
        HookFlags::default()
    };
    pool.pricer = a.pricer;
    pool.min_notional = a.min_notional;
    pool.max_notional = a.max_notional;
    pool.bump = ctx.bumps.pool;
    pool.tvl = 0;
    pool.collateral_held = 0;
    pool.util_pay_bp = 0;
    pool.util_rec_bp = 0;
    pool.open_pay_notional = 0;
    pool.open_rec_notional = 0;
    pool.open_swaps = 0;
    pool.fees_lifetime = 0;
    pool.event_seq = 0;
    pool.collateral_pay = 0;
    pool.collateral_rec = 0;
    pool.book_pay = BookSide::default();
    pool.book_rec = BookSide::default();
    pool.ladder = [0; LADDER_BYTES];
    pool.share_supply = 0;
    pool.queued_shares = 0;
    pool.queue_first_slot = 0;
    ctx.accounts.global.pool_count = ctx
        .accounts
        .global
        .pool_count
        .checked_add(1)
        .ok_or(BrinkError::Overflow)?;
    emit_cpi!(PoolCreated {
        pool: pool.key(),
        benchmark: pool.benchmark,
        share_mint: pool.share_mint,
        vault: pool.vault
    });
    pool.assert_invariants(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn step_is_half_to_one_and_a_half() {
        assert!(within_step(100, 50));
        assert!(!within_step(100, 49));
        assert!(within_step(100, 151));
        assert!(!within_step(100, 152));
        assert!(within_step(0, 1));
        assert!(!within_step(0, 2));
    }
    #[test]
    fn guardian_only_tightens() {
        assert!(severity(OperatingMode::Halted) > severity(OperatingMode::WithdrawOnly));
        assert!(severity(OperatingMode::WithdrawOnly) > severity(OperatingMode::Limited));
        // The fast path stops at WithdrawOnly: exits stay open unless the timelocked authority halts.
        assert!(severity(OperatingMode::Halted) > severity(OperatingMode::WithdrawOnly));
        assert!(severity(OperatingMode::Limited) > severity(OperatingMode::Normal));
    }

    fn devnet() -> VernierParamsOnChain {
        VernierParamsOnChain {
            model_pay_bp: [11, 22, 31, 54],
            model_rec_bp: [9, 12, 14, 19],
            term_bp: [3, 5, 7, 12],
            demand_k_bp: 45,
            demand_cap_bp: 60,
            collateral_bp: [120, 230, 330, 600],
        }
    }

    #[test]
    fn calibration_bounds_accept_devnet_and_reject_each_breach() {
        assert!(check_calibration_bounds(&devnet()).is_ok());
        let mut p = devnet();
        p.collateral_bp[3] = 10_001;
        assert!(check_calibration_bounds(&p).is_err());
        let mut p = devnet();
        p.demand_k_bp = 0;
        assert!(check_calibration_bounds(&p).is_err());
        let mut p = devnet();
        p.term_bp = [3, 5, 4, 12];
        assert!(check_calibration_bounds(&p).is_err());
        let mut p = devnet();
        p.collateral_bp = [600, 600, 600, 600];
        assert!(check_calibration_bounds(&p).is_ok());
    }

    #[test]
    fn step_bound_is_exact_at_the_top_of_u16() {
        assert!(within_step(u16::MAX, u16::MAX));
        assert!(within_step(21_846, 21_846));
        assert!(!within_step(1_000, 1_502));
        assert!(within_step(1_000, 1_501));
        assert!(within_step(1_000, 500));
        assert!(!within_step(1_000, 499));
    }
}
