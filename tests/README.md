# Program tests

| Layer | Where | Runs with |
|---|---|---|
| Pure logic (pricing engine) | `vernier/src/lib.rs`: unit + proptest (11), Kani harnesses (`cargo kani -p vernier`), fuzz target (`vernier/fuzz`) | `cargo test -p vernier` |
| Pure logic (AMM maths, fee split, step limits, pending calibration) | `swap_amm/src/instructions/{math,fees,admin,swap}.rs` unit tests (12) | `cargo test -p swap_amm` |
| On-chain end-to-end | `tests/svm/tests/e2e.rs`: LiteSVM drives the compiled `.so` files through every instruction, asserting token balances, PDA state, error codes and the vault conservation invariant (6 scenarios) | `./build-sbf.sh && (cd tests/svm && cargo test)` |

The SVM harness is a separate Cargo workspace so that Agave/LiteSVM crate versions never constrain the programs' Anchor versions. Its only contract with the programs is the `.so` files under `target/deploy` and the mirrored account layouts in `tests/svm/src/lib.rs` (field order is asserted by discriminator and by the balance checks).
