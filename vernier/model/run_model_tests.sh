#!/bin/bash
cd "$(dirname "$0")"
echo "== model crate tests (PROPTEST_CASES=100000)"; time cargo test --release 2>&1 | tail -60
echo MODEL_TESTS_DONE
cd "$(dirname "$0")/../.."
echo "== vernier crate incl. tests/model_diff.rs"; time cargo test -p vernier --release 2>&1 | tail -40
echo VERNIER_TESTS_DONE
