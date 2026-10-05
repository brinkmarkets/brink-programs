#!/bin/bash
# Fast-layer batch. Usage: run_mc_batch.sh SEED_START BUDGET_SECS [HORIZON_DAYS]
# Writes results to results/mc_<start>_<end>_<h>d.{json,md} and touches results/batch_<start>.done when finished.
cd "$(dirname "$0")"
START=${1:-1}
BUDGET=${2:-1200}
H=${3:-365}
./target/release/brink_sim_fast mc --seed-start "$START" --horizon-days "$H" --time-budget-secs "$BUDGET" --threads 2 --out results > "results/batch_${START}.log" 2>&1
touch "results/batch_${START}.done"
