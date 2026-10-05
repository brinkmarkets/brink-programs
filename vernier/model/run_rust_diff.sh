#!/bin/bash
# In-process differential: vernier crate vs exact reference model.
cd "$(dirname "$0")"
for s in 7 11; do ./target/release/diff --cases 10000000 --seed $s; done
./target/release/diff --cases 5000000 --seed 23 --valid-only
echo RUST_DIFF_DONE
