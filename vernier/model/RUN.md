# Vernier reference model and differential tests

Everything here is new and self-contained; nothing under `programs/*/src` was modified. The crate is a standalone
workspace (`[workspace]` in `Cargo.toml`) that depends on `vernier` by path, so it does not change the programs'
workspace or lock file. It builds into its own `target/` inside this directory.

## Layout

| Path | Purpose |
|---|---|
| `src/reference.rs` | Exact rational reference (`Q{n, d}` over i128) written from `docs/audit/maths/SPEC.md`, one function per formula |
| `src/transcribed.rs` | Line-for-line transcription of the `swap_amm` and `brink_index` integer maths (not available as crates) |
| `src/lib.rs` | Input generator (`gen`: SplitMix64, structured boundaries, off-by-one around every threshold) and the bridge that runs the crate and the reference on a case |
| `src/bin/diff.rs` | In-process differential runner; also emits the case stream for the external checkers |
| `ref_model.py` | Independent Python `fractions.Fraction` reference and stream checker |
| `ts_check.mjs` | Bundles `packages/vernier/src/index.ts` with the repository's esbuild and checks the stream against it |
| `tests/invariants.rs` | proptest invariants I-1 to I-14, exhaustive sweeps and the counterexamples for findings M-1 to M-9 |
| `../tests/model_diff.rs` | The one integration test added to the `vernier` crate: seeded sweep and proptest against the reference |

## Commands

```
cd programs/vernier/model
cargo build --release --bin diff
./target/release/diff --cases 10000000 --seed 7            # in-process crate vs reference, exit 1 on mismatch
./target/release/diff --cases 5000000 --seed 23 --valid-only
./target/release/diff --cases 10000000 --emit 10000000 --seed 7 2>/dev/null | python3 ref_model.py
./target/release/diff --cases 10000000 --emit 10000000 --seed 7 2>/dev/null | node ts_check.mjs
cargo test --release                                       # invariants; PROPTEST_CASES=100000 default

cd programs
cargo test -p vernier --release                            # existing suite plus tests/model_diff.rs
                                                           # MODEL_DIFF_CASES (default 2 000 000), PROPTEST_CASES (default 200 000)
```

Runs longer than a minute were launched from the scripts in this directory (`run_rust_diff.sh`,
`run_model_tests2.sh`, `run_streams.sh`) with `setsid nohup`, logs alongside (`rust_diff.log`, `model_tests2.log`,
`streams.log`, `ts_mismatch_examples.log`).

## Stream format

One case per line, fields separated by spaces, `|` between inputs and outputs:

```
spot ema tenor_ix leg notional tvl util_pay util_rec mp0 mp1 mp2 mp3 mr0 mr1 mr2 mr3 t0 t1 t2 t3 k cap c0 c1 c2 c3 | status fixed ref model demand term before after reduces collateral capacity
```

`leg` is 0 for pay-fixed and 1 for receive-fixed; `status` is `ok`, `empty_pool`, `malformed_util` or `overflow`;
the numeric outputs are omitted when `status` is not `ok`.

## Domain covered by the generator

Rates 0 to 65 535 bp (with a bias to 0 to 30 000 and to the thresholds), notionals 1 to 2^64 − 1 (structured
around `tvl / 10^4`, the leg and total caps, `i32::MAX · tvl / 10^4` and powers of two), tvl 1 to 2^64 − 1,
utilisation 0 to 65 535 with boundaries at 4 800, 8 000 and 10 000, all four tenors, both legs, the default
parameter table and random tables including zeros and `u16::MAX`. `--valid-only` restricts to pools the program
can reach (utilisation within the caps, tvl > 0).
