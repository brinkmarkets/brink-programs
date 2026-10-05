#!/bin/bash
cd "$(dirname "$0")"
cargo build --release -p brink_sim_fast > results/build_fast.log 2>&1
echo "exit $?" >> results/build_fast.log
touch results/build_fast.done
