# Slow layer: differential replay, horizon 60 days

| Seed | Transitions attempted | Accepted | Chain state checks | Mismatches | Invariant failures (model) |
|---:|---:|---:|---:|---:|---|
| 1 | 1185 | 958 | 5925 | 0 | none |
| 2 | 1686 | 1669 | 8430 | 0 | none |
| 3 | 6171 | 360 | 30855 | 0 | none |
| 4 | 1979 | 779 | 9895 | 0 | none |
| 5 | 3111 | 1347 | 15555 | 0 | none |
| 6 | 3677 | 2117 | 18385 | 0 | none |

## Compute units (scenario replay)

| Instruction | Outcome | Count | Min CU | Median CU | Max CU |
|---|---|---:|---:|---:|---:|
| brink_index::create_benchmark | ok | 6 | 13489 | 13489 | 13489 |
| brink_index::initialise | ok | 6 | 6571 | 6571 | 6571 |
| brink_index::publish | ok | 4296 | 9518 | 9979 | 9979 |
| swap_amm::admin_create_pool | ok | 6 | 34125 | 34125 | 34125 |
| swap_amm::admin_initialise_global | ok | 6 | 18339 | 18339 | 18339 |
| swap_amm::admin_queue_calibration | ok | 1 | 12687 | 12687 | 12687 |
| swap_amm::admin_set_mode | rejected | 8 | 5355 | 5983 | 5983 |
| swap_amm::admin_set_mode | ok | 13 | 6714 | 6714 | 6720 |
| swap_amm::crank_liquidate_swap | rejected | 7715 | 7386 | 20063 | 21383 |
| swap_amm::crank_settle_swap | ok | 172 | 26351 | 30791 | 34252 |
| swap_amm::lp_deposit | rejected | 32 | 15744 | 15839 | 15839 |
| swap_amm::lp_deposit | ok | 264 | 24489 | 24994 | 25195 |
| swap_amm::lp_withdraw | rejected | 481 | 17370 | 30212 | 30413 |
| swap_amm::lp_withdraw | ok | 220 | 27258 | 32034 | 32236 |
| swap_amm::sweep_fees | rejected | 5 | 11329 | 11329 | 11329 |
| swap_amm::sweep_fees | ok | 63 | 16300 | 16300 | 16300 |
| swap_amm::sync_vault | ok | 1 | 10470 | 10470 | 10470 |
| swap_amm::trader_cancel_swap | rejected | 33 | 17199 | 18933 | 18962 |
| swap_amm::trader_cancel_swap | ok | 452 | 25294 | 27422 | 32110 |
| swap_amm::trader_open_swap | rejected | 2305 | 21013 | 22515 | 35884 |
| swap_amm::trader_open_swap | ok | 1747 | 36074 | 37604 | 60112 |

