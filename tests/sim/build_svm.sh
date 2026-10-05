#!/bin/bash
# Builds the slow layer (LiteSVM) crate and its tests without running them.
cd "$(dirname "$0")"
cargo build -p brink_sim_svm --tests > results/build_svm.log 2>&1
echo "exit $?" >> results/build_svm.log
touch results/build_svm.done
