#!/bin/bash
# Runs the slow layer: differential replay then adversarial tests. Done-file results/svm_tests.done.
cd "$(dirname "$0")"
: "${BRINK_SIM_SEEDS:=1,2,3,4,5,6}"
: "${BRINK_SIM_HORIZON_DAYS:=60}"
export BRINK_SIM_SEEDS BRINK_SIM_HORIZON_DAYS
rm -f results/svm_adversarial.md
{
  echo "== adversarial"
  cargo test -p brink_sim_svm --test adversarial -- --nocapture --test-threads 1
  echo "adversarial exit $?"
  echo "== scenarios seeds=$BRINK_SIM_SEEDS horizon=$BRINK_SIM_HORIZON_DAYS"
  cargo test -p brink_sim_svm --test scenarios -- --nocapture
  echo "scenarios exit $?"
} > results/svm_tests.log 2>&1
touch results/svm_tests.done
