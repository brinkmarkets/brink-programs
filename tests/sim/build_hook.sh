#!/bin/bash
# Builds the re-entrancy probe hook program with the shared target directory; output in hook/deploy.
cd "$(dirname "$0")"
export PATH="$HOME/.local/share/solana/install/active_release/bin:$PATH"
cargo build-sbf --manifest-path hook/Cargo.toml --sbf-out-dir hook/deploy > results/build_hook.log 2>&1
echo "exit $?" >> results/build_hook.log
ls -la hook/deploy >> results/build_hook.log 2>&1
touch results/build_hook.done
# cargo-build-sbf looks for the artefact under hook/target; with the shared target directory it lands here instead:
cp -f ../../target/sbpfv3-solana-solana/release/brink_sim_hook.so hook/deploy/ 2>/dev/null || true
ls -la hook/deploy >> results/build_hook.log 2>&1
