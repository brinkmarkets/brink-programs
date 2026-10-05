#!/bin/bash
cd "$(dirname "$0")"
rm -f results/svm_adversarial.md
cargo test -p brink_sim_svm --test adversarial -- --nocapture --test-threads 1 > results/svm_adversarial.log 2>&1
echo "exit $?" >> results/svm_adversarial.log
touch results/svm_adversarial.done
