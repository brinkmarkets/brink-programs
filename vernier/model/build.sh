#!/bin/bash
cd "$(dirname "$0")" && cargo build --release --bin diff 2>&1 | tail -40 && echo BUILD_DONE && time ./target/release/diff --cases 200000 --seed 1 2>&1 | tail -30
