## A-1 re-entrancy via hook CPI

- Environment A (BeforeOpen re-enters `sync_vault` with the accounts exactly as received): the re-entry cannot even be addressed: `swap_amm` does not forward its own program account to the hook, so the runtime reports the target program as missing and the outer open is aborted (`InstructionError(0, MissingAccount)`); pool unchanged. A failed CPI cannot be caught by the caller on Solana, so a re-entering hook can only deny service, never mutate (and a hook given the program account would still meet the runtime's re-entrancy rule).
- Environment B (AfterOpen writes one byte into the read-only pool): transaction rejected (`InstructionError(0, ReadonlyDataModified)`), pool unchanged.
- Environment C (BeforeWithdraw returns an error): withdraw rejected; the hook's own error code surfaces unchanged (`InstructionError(0, Custom(20058))`), so a hook can veto LP exits, which is the documented trust assumption on the hook program.
- Environment D (BeforeCancel re-enters `trader_cancel_swap` with the pool and swap marked writable although received read-only): rejected by the runtime before the callee runs (`InstructionError(0, PrivilegeEscalation)`); swap still open.


## A-2 duplicate, wrong-owner and wrong-mint accounts

- lp_deposit / lp_withdraw: foreign owner and wrong-mint token accounts rejected (TokenOwner, SettlementMint); the same account passed twice as two mutable arguments is rejected by Anchor before any typed check (ConstraintDuplicateMutableAccount).
- trader_open_swap: vault as trader account and vault or fee vault duplicated (ConstraintDuplicateMutableAccount), wrong-mint ATA (SettlementMint).
- trader_open_swap with a different benchmark account than the pool's: ConstraintHasOne.
- close paths: cranker destination equal to the vault or to the trader account (ConstraintDuplicateMutableAccount) or owned by a third party (TokenOwner), trader account or trader USDC replaced (ConstraintHasOne, TokenOwner), stranger cancel (ConstraintSigner); sweep_fees with treasury and buyback swapped and sync_vault with the fee vault: ConstraintHasOne.
- Trader settles their own swap: naming their own USDC account as cranker destination is a duplicate mutable account (rejected); without a cranker account the settlement succeeds, payout 118926028, no bounty (signer == trader).


## A-3 stale benchmark, band, ceiling and publisher rules

- 2,001 slots after the last publish (guard 2,000): open, cancel, liquidate rejected with BenchmarkStale.
- OutOfBand at ema + band + 1, RateCeiling above 30,000 bp, non-publisher signer rejected (ConstraintHasOne). A publisher sitting at the band edge once an hour for 24 hours moved the EMA from 684 to 4107 bp and the spot to 4265 bp with the band at 300 and a half-life of 10,000 slots: the band bounds each step, not the drift.
- Settlement 28 days after the last publish succeeded: `crank_settle_swap` is deliberately not staleness-guarded (liveness), the held rate accrues meanwhile.
- min_interval 150 slots: a publish after 100 slots is TooFrequent, after 150 accepted.


## A-4 settlement and liquidation rules

- Settle before maturity: NotMatured. Liquidate a healthy swap: NotLiquidatable. Near-maturity (3 h) liquidation of an in-the-money swap by a stranger: NotLiquidatable (the binary carries ADR-007, exhaustion is the only trigger).
- Settlement at maturity paid 36747602739 on collateral 33000000000 (gain clamped to collateral less the 10 pct income fee, less the 2 bp crank bounty).
- Double settle: second attempt fails on the closed account. Liquidate after maturity: AlreadySettled.
- Exhaustion probe: receive-fixed 180 d at 864 bp, spot walked to 2193 bp (ema 2035): liquidation accepted (residual to trader 0 on collateral 60000000000).


## A-5 withdraw beyond share and zero amounts

- Withdraw more shares than held (own or empty account): SPL insufficient funds. Zero shares or zero amount: NotionalTooSmall. min_amount or min_shares above the result: Slippage.
- Withdrawing 10 pct with a swap open returned 99500000000 (50 bp exit fee applied; the fee is booked to the fee vault).


## A-6 unauthorised mode changes and admin paths

- admin_set_mode: stranger ConstraintSigner; missing authority signature rejected by the runtime; guardian to Halted, guardian loosening, guardian same-mode: GuardianScope; authority restores Normal.
- admin_queue_calibration: stranger and guardian ConstraintHasOne; zero collateral and 182 (above 1.5x + 1 of 120) CalibrationStep; 181 accepted; re-queue within the delay replaces the pending set and restarts the delay (pending_effective_slot 433000).
- admin_set_authority by a stranger co-signed by the would-be authority: ConstraintHasOne.


## A-7 timelock bypass attempts

- initialise: delay below 432,000 or above 6,480,000 DelayOutOfRange; proposer equal to guardian RolesMustDiffer (proposer may equal executor, a two-key deployment); re-initialise on the existing PDA rejected by the system program.
- queue: stranger and executor ConstraintHasOne; a reused nonce fails the operation PDA seeds check (the nonce is the timelock's own counter); SetDelay below the floor DelayOutOfRange at queue time.
- execute: before eta TooEarly (also at eta minus one slot); stranger and guardian Unauthorised; unknown nonce fails on the account; cancelled or executed operation NotQueued or closed; past eta plus grace Expired.
- cancel: stranger and executor Unauthorised; guardian cancels; roles unchanged afterwards.


## S-1 settlement blocked by the utilisation cap after a gain

Reproduced on chain. Pool 1,000,000 USDC; pay leg at 4,800 bp (480,000 USDC, 180 d); receive-fixed 100,000 USDC 28 d at 672 bp; rate published at 400 bp. At maturity `crank_settle_swap` and `trader_cancel_swap` both fail with PoolInvariant because the gain (1200000000 clamped to collateral) lowers tvl and pushes pay utilisation above the cap. First error line: `InstructionError(0, Custom(6006))`. After a 2,000 USDC deposit the settlement succeeds, paying 1387791781; pay utilisation afterwards 4791 bp.

## S-2 Limited mode cap is per swap

With limited_mode_cap 100,000 USDC a 150,000 USDC open is rejected (LimitedModeCap) but three 100,000 USDC opens by the same trader in the same slot are accepted: open pay notional 300000 USDC. The cap bounds a single swap, not aggregate exposure.

## S-3 cancel after maturity

Pay-fixed 100,000 USDC 28 d at 698 bp against a flat 684 bp index; nobody cranks for 28 days after maturity, the publisher republishes 684, the trader cancels (not settles): payout 1662575342 on collateral 1200000000 (GAIN paid out of LP capital: F-12 reachable through cancel as well as settle). A fair settlement is a small loss.

## S-4 first-depositor inflation via sync_vault

First LP deposits 1 unit, donates 1,000 USDC to the vault and calls sync_vault (tvl 1,000.000001 USDC on 1 share). Second LP deposits 1,999 USDC and receives 1 share(s) (pool tvl 2999000001 units, supply 2); withdrawing immediately returns 1499500000 units, a loss of 499 USDC to the first depositor. A min_shares floor on the client side or a minimum initial deposit on chain removes the exposure (review F-16).

## S-5 low-rate regime

Index at 10 bp: a receive-fixed 90 d quote would be 10 - 14 - demand - 7 < 0 and the open fails with `InstructionError(0, Custom(6026))` (the error name Overflow is misleading for a quotable-but-negative rate; the current source renames it QuoteBelowZero); a pay-fixed 90 d swap opens at 53 bp; the trader's cancel one day later SUCCEEDED: the binary floors the unwind quote at zero (maths finding M-7), the model in this harness still reports Overflow for this case and needs the same floor.

## Compute units, canonical path

| Instruction | Outcome | Count | Min CU | Median CU | Max CU |
|---|---|---:|---:|---:|---:|
| brink_index::create_benchmark | ok | 1 | 13489 | 13489 | 13489 |
| brink_index::initialise | ok | 1 | 6571 | 6571 | 6571 |
| brink_index::publish | ok | 2 | 9518 | 9979 | 9979 |
| swap_amm::admin_create_pool | ok | 1 | 34125 | 34125 | 34125 |
| swap_amm::admin_initialise_global | ok | 1 | 18339 | 18339 | 18339 |
| swap_amm::admin_queue_calibration | ok | 1 | 12687 | 12687 | 12687 |
| swap_amm::admin_set_mode | ok | 2 | 6714 | 6714 | 6714 |
| swap_amm::crank_settle_swap | ok | 1 | 29726 | 29726 | 29726 |
| swap_amm::lp_deposit | ok | 1 | 24489 | 24489 | 24489 |
| swap_amm::lp_withdraw | ok | 1 | 31922 | 31922 | 31922 |
| swap_amm::sweep_fees | ok | 1 | 16300 | 16300 | 16300 |
| swap_amm::sync_vault | ok | 1 | 10432 | 10432 | 10432 |
| swap_amm::trader_cancel_swap | ok | 1 | 25987 | 25987 | 25987 |
| swap_amm::trader_open_swap | ok | 2 | 36076 | 42107 | 42107 |


