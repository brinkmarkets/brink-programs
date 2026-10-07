#!/usr/bin/env bash
# Builds each deployable program on its own so feature unification cannot strip an entrypoint
# (swap_amm depends on brink_index with the `cpi` feature, which implies `no-entrypoint`).
# Pinned to platform-tools v1.57, SBPF v0: the devnet programs were built with it, and v1.52 emits 4 KB stack
# frame overflows in every Anchor `try_accounts` (runtime access violations on LiteSVM) for the same source.
set -euo pipefail; cd "$(dirname "$0")"
TOOLS="${BRINK_SBF_TOOLS:-v1.57}"
for p in ${BRINK_SBF_PROGRAMS:-brink_index brink_timelock swap_amm brink_sale brink_allowlist brink_venue brink_vaults brink_split}; do
  echo "== $p"
  cargo build-sbf --manifest-path "$p/Cargo.toml" --tools-version "$TOOLS" --arch v0 2>&1 | grep -E "Stack offset|overflows the maximum|error|Finished" | sed -E 's/^.*Function (_ZN[0-9]+_)?/  stack: /' | cut -c1-200 || true
  if [ "${PIPESTATUS[0]}" -ne 0 ]; then echo "build of $p FAILED"; exit 1; fi
done
ls -la target/deploy/*.so
