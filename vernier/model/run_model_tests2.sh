#!/bin/bash
cd "$(dirname "$0")"
echo "== model crate tests (PROPTEST_CASES=100000)"; time cargo test --release 2>&1 | rg -v Compiling | tail -70
echo MODEL_TESTS_DONE
