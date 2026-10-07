# Brink programs

The Solana programs behind Brink fixed-rate markets: Anchor programs (`anchor-lang` 1.2.0 as locked in `Cargo.lock`)
for Solana 3.1.10.

Status: deployed on devnet; `brink_sale` also runs on mainnet-beta. Audit in progress.

## Programs

| Crate | Program id | Purpose |
|---|---|---|
| `swap_amm` | `EbD862QySygYKCMd8RHM2JaHqnduLQXB8Y632U3pQtce` | Fixed-for-floating interest-rate swap AMM: pools, swaps, forwards, basis swaps, LP capital and the reserve |
| `brink_index` | `J2udZ8xzSsETsrSnbW3DeLFBTVooKRup3P7LWKSRwuvS` | Benchmark index: registered publishers post floating-rate observations, aggregated into guarded values |
| `brink_timelock` | `CeQz4x7Ad7Hg715Tn4PHtvtAxYin2PHv4KGaD3KS3kM5` | Upgrade timelock: delayed, cancellable governance operations, program upgrades included |
| `brink_sale` | `GYCJoDiZCh7mbqyWRjYJmpPUt3nPLs6887tGm58rmK3v` | Fixed-price BRINK sale rounds with escrow, finalisation and vesting |
| `brink_allowlist` | `FaNttHSrjcQruS8qBNFDdmAnKq9Z4ZGu2y1UkMYXe1UR` | Allow-list hook for permissioned pools |
| `brink_venue` | `Boj38zJFfsN72DBL2nrJEfk1ewskn11EbdikgHMBX7v2` | Venue adapter: the floating leg of a fixed vault |
| `brink_vaults` | `HhDTKdT36W1vE1DBgKnXDp7gFtxpxhRxovmfanuhaQiy` | Fixed vaults: a benchmark's floating rate turned into a fixed APY for a fixed term |
| `brink_split` | `ASYzAAxpLwQW5XwdL1GJbpQ15onLR6uM1HBSCRTza9sz` | Yield splitting into principal and yield tokens that share one maturity |
| `vernier` | library | Vernier, the pricing library: pure, integer and deterministic |

The ids are fixed; `brink_sale` uses the same id on devnet and mainnet-beta. `idl/` holds the Anchor IDL of each program.

## Build

`./build-sbf.sh` builds each program on its own into `target/deploy` (platform tools v1.57, SBPF v0). Deployed
objects come only from the pinned verifiable-build image; `VERIFY.md` gives the parameters, the commands and the
executable hash of every deployed program, so anyone can rebuild a program and compare it with the chain.

## Test

`cargo test --workspace` runs the library and program unit tests. The on-chain suite and the simulation harness are
described in `tests/README.md`.
