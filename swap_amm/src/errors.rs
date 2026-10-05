//! One error per distinct failure point, so every failure path has a test and an auditor can map findings to code.
use anchor_lang::prelude::*;

#[error_code]
pub enum BrinkError {
    #[msg("protocol is halted")]
    Halted,
    #[msg("protocol is in withdraw-only mode")]
    WithdrawOnly,
    #[msg("protocol is in limited mode: notional above the limited-mode cap")]
    LimitedModeCap,
    #[msg("benchmark is not published")]
    BenchmarkNotPublished,
    #[msg("benchmark publication is stale")]
    BenchmarkStale,
    #[msg("benchmark value outside sanity band")]
    BenchmarkOutOfBand,
    #[msg("pool invariant violated after instruction")]
    PoolInvariant,
    #[msg("vault balance does not equal tvl + collateral held")]
    Conservation,
    #[msg("per-leg utilisation cap exceeded")]
    LegCap,
    #[msg("quote outside the caller's limit rate")]
    LimitRate,
    #[msg("output below the caller's minimum")]
    Slippage,
    #[msg("notional below minimum")]
    NotionalTooSmall,
    #[msg("notional above maximum for this pool")]
    NotionalTooLarge,
    #[msg("invalid tenor index")]
    Tenor,
    #[msg("swap has not matured")]
    NotMatured,
    #[msg("swap already settled")]
    AlreadySettled,
    #[msg("swap is not liquidatable")]
    NotLiquidatable,
    #[msg("calibration change outside allowed step")]
    CalibrationStep,
    #[msg("hook program rejected the action")]
    HookRejected,
    #[msg("hook program id does not match the pool's hook set")]
    HookMismatch,
    #[msg("pricer program id does not match the pool")]
    PricerMismatch,
    #[msg("settlement mint must be USDC")]
    SettlementMint,
    #[msg("token program must own the settlement mint")]
    TokenProgram,
    #[msg("token account owner mismatch")]
    TokenOwner,
    #[msg("guardian may only tighten the operating mode")]
    GuardianScope,
    #[msg("nothing to sweep")]
    NothingToSweep,
    #[msg("arithmetic overflow")]
    Overflow,
    #[msg("benchmark history before the most recent accrual segment is not available")]
    BenchmarkHistoryUnavailable,
    #[msg("fixed rate would be negative; no quotes below zero")]
    QuoteBelowZero,
    #[msg("fixed rate above the u16 ceiling")]
    QuoteCeiling,
    #[msg("event sequence overflow")]
    Sequence,
    #[msg("only the program's upgrade authority may initialise")]
    NotUpgradeAuthority,
    #[msg("hooks cannot be set on exit points; exits are unconditional")]
    HookExitPoint,
    #[msg("withdrawal exceeds the immediate limit; enqueue it")]
    UseWithdrawQueue,
    #[msg("nothing queued")]
    NothingQueued,
    #[msg("the queue epoch has not run its minimum length since its first request")]
    EpochNotElapsed,
    #[msg("an older processed epoch still has unclaimed requests; claim them first")]
    EpochUnclaimed,
    #[msg("the caps leave no capacity to fill the queue in this epoch")]
    NoCapacity,
    #[msg("request epoch not processed yet")]
    RequestNotProcessed,
    #[msg("request epoch already processed; claim instead")]
    RequestProcessed,
    #[msg("request epoch record no longer available")]
    EpochEvicted,
    #[msg("calibration parameter outside its static bounds")]
    CalibrationBound,
    #[msg("calibration tables must be non-decreasing in tenor")]
    CalibrationOrder,
    #[msg("a matured swap awaits settlement; settle it (anyone may) before LP pricing resumes")]
    MaturedUnsettled,
    #[msg("an eligible withdrawal epoch has priority over new exposure for this capacity")]
    QueueHasPriority,
    #[msg("pool share supply mirror disagrees with the mint")]
    ShareSupplyMismatch,
    #[msg("swap is one leg of a basis swap; close both legs with trader_cancel_basis_swap")]
    LinkedLeg,
    #[msg("the two swaps are not the two legs of one basis swap")]
    LinkNotPaired,
    #[msg("a basis swap needs two different pools")]
    BasisSamePool,
    #[msg("correlation offset above MAX_CORRELATION_BP")]
    CorrelationBound,
    #[msg("a forward's start plus tenor may not run past 180 days")]
    ForwardHorizon,
    #[msg("the forward has not reached its start yet")]
    ForwardNotDue,
    #[msg("the forward has already started")]
    ForwardAlreadyStarted,
    #[msg("swap is not a forward-starting swap")]
    NotForward,
}

impl From<vernier::VernierError> for BrinkError {
    fn from(e: vernier::VernierError) -> Self {
        match e {
            vernier::VernierError::EmptyPool | vernier::VernierError::MalformedUtilisation => {
                BrinkError::PoolInvariant
            }
            vernier::VernierError::Overflow => BrinkError::Overflow,
            vernier::VernierError::CorrelationOutOfRange => BrinkError::CorrelationBound,
            vernier::VernierError::ForwardHorizon => BrinkError::ForwardHorizon,
        }
    }
}
