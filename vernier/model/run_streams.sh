#!/bin/bash
# External checkers over the emitted stream: Python Fractions reference, then the TypeScript mirror via node.
cd "$(dirname "$0")"
echo "== python reference, 10M cases, seed 7"
time (./target/release/diff --cases 10000000 --emit 10000000 --seed 7 2>/dev/null | python3 ref_model.py)
echo PY_DONE
echo "== typescript mirror, 10M cases, seed 7"
time (./target/release/diff --cases 10000000 --emit 10000000 --seed 7 2>/dev/null | node ts_check.mjs 2> ts_mismatch_examples.log)
echo TS_DONE
echo "== typescript mirror, 2M cases restricted to notional and tvl below 2^53"
