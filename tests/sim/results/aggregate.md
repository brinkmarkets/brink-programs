| Batch file | Seeds | Elapsed s | Transitions attempted | Accepted | Quotes |
|---|---:|---:|---:|---:|---:|
| mc_1000000_1001241_365d | 1241 | 60 | 32,458,847 | 8,031,342 | 24,138,500 |
| mc_100000_131258_365d | 31258 | 1200 | 809,763,404 | 196,202,327 | 606,927,686 |
| mc_1_21839_365d | 21838 | 1200 | 567,912,467 | 138,065,775 | 425,239,084 |
| mc_200000_229874_365d | 29874 | 1200 | 780,271,179 | 188,438,297 | 584,982,326 |
| **Total** | 84,211 | 3660 | 2,190,405,897 | 530,737,741 | 1,641,287,596 |

| Invariant | Pass | Fail |
|---|---:|---:|
| I1 | 1,397,078,101 | 0 |
| I2 | 1,268,426,101 | 0 |
| I3 | 1,268,426,101 | 0 |
| I4 | 1,268,426,101 | 0 |
| I5 | 1,268,426,101 | 0 |
| I6 | 1,328,570,978 | 0 |
| I7 | 1,268,426,101 | 0 |
| I8 | 25,962,404 | 374,506 |
| I9 | 55,098,780 | 0 |
| I10 | 55,098,780 | 0 |
| I11 | 148,709,005 | 0 |
| I12 | 148,811,649 | 0 |
| I13 | 19,500,789 | 0 |
| I14 | 1,659,668,156 | 0 |
| I15 | 64,694,877 | 0 |

| Instruction | Attempted | Accepted |
|---|---:|---:|
| admin_queue_calibration | 462,907 | 346,910 |
| admin_set_mode | 538,421 | 329,618 |
| crank_liquidate_swap | 1,333,714,979 | 936,698 |
| crank_settle_swap | 26,336,910 | 25,962,404 |
| donate | 102,644 | 102,644 |
| lp_deposit | 22,119,267 | 19,500,789 |
| lp_withdraw | 101,954,032 | 9,414,559 |
| publish | 391,350,024 | 376,100,823 |
| sweep_fees | 6,151,452 | 5,046,097 |
| sync_vault | 102,644 | 102,644 |
| trader_cancel_swap | 30,874,760 | 28,199,678 |
| trader_open_swap | 276,697,857 | 64,694,877 |

| Rejection (instruction:error) | Count |
|---|---:|
| admin_queue_calibration:CalibrationStep | 115,997 |
| admin_set_mode:ConstraintSigner | 76,755 |
| admin_set_mode:GuardianScope | 132,048 |
| crank_liquidate_swap:AccountNotInitialized | 5,290,484 |
| crank_liquidate_swap:AlreadySettled | 14,452,468 |
| crank_liquidate_swap:BenchmarkStale | 114,925,395 |
| crank_liquidate_swap:NotLiquidatable | 1,198,109,934 |
| crank_settle_swap:PoolInvariant | 374,506 |
| lp_deposit:Halted | 11,541 |
| lp_deposit:WithdrawOnly | 2,606,937 |
| lp_withdraw:Halted | 31,690 |
| lp_withdraw:PoolInvariant | 90,394,833 |
| lp_withdraw:Slippage | 2,112,950 |
| publish:OutOfBand | 15,249,201 |
| sweep_fees:NothingToSweep | 1,105,355 |
| trader_cancel_swap:BenchmarkStale | 2,004,959 |
| trader_cancel_swap:ConstraintSigner | 616,618 |
| trader_cancel_swap:Halted | 12,163 |
| trader_cancel_swap:PoolInvariant | 41,342 |
| trader_open_swap:BenchmarkStale | 21,306,103 |
| trader_open_swap:Halted | 142,881 |
| trader_open_swap:LegCap | 132,473,192 |
| trader_open_swap:LimitRate | 10,884,867 |
| trader_open_swap:LimitedModeCap | 9,444,832 |
| trader_open_swap:NotionalTooLarge | 662,146 |
| trader_open_swap:Overflow | 4,243,322 |
| trader_open_swap:PoolInvariant | 140,727 |
| trader_open_swap:WithdrawOnly | 32,704,910 |

| Metric | Value |
|---|---:|
| Ticks (hours) simulated | 737,688,360 |
| Closes / clamped at collateral | 55,098,780 / 1,289,537 |
| Settlements / booked differs from fair | 25,962,404 / 10,195,934 |
| Early closes | 29,136,376 |
| Gains paid from LP capital (USDC) | 48,005,270,213 |
| Losses booked to LP capital (USDC) | 126,309,674,216 |
| Fees (USDC) | 128,538,534,381 |
| Seeds where tvl hit zero with shares outstanding | 0 |
| Seeds with drawdown >= 10 pct / >= 50 pct | 52 / 5 |
| Stale-benchmark rejections / withdrawals blocked by caps | 138,236,457 / 90,394,833 |
| Clamped on trader loss (LP absorbs excess): count / USDC | 352,280 / 2,519,800,319 |
| Clamped on trader gain (trader forgoes excess): count / USDC | 103,501 / 1,191,471,906 |
| Deposits capturing dead capital: count / USDC | 245 / 167,185 |

| Extreme | Value | Seed | Batch |
|---|---:|---:|---|
| min_share_price_e6 | 891,519 | 1555 | mc_1_21839_365d.json |
| max_collateral_shortfall_units | 8,140,964,537,351 | 210164 | mc_200000_229874_365d.json |
| max_single_gain_bp_of_tvl | 554 | 13721 | mc_1_21839_365d.json |
| settle_transfer_abs_max_units | 727,348,090,221 | 6843 | mc_1_21839_365d.json |
| early_close_transfer_abs_max_units | 482,735,240,817 | 222116 | mc_200000_229874_365d.json |
