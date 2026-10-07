# Program tests

| Layer | Where | Runs with |
|---|---|---|
| Pricing library | `vernier/src/lib.rs`: unit and property tests, Kani harnesses (`cargo kani -p vernier`), fuzz target (`vernier/fuzz`) | `cargo test -p vernier` |
| Program logic | unit tests in the program crates, among them the AMM maths, fee split, step limits and pending calibration | `cargo test --workspace` |
| On-chain | `tests/svm/tests/`: LiteSVM loads the compiled `.so` files and drives the programs end to end (pools, swaps, forwards, basis, the reserve, permissioned pools, the venue adapter, fixed vaults, yield splitting and the sale), with adversarial and red-team cases and success-path coverage of every instruction the harness can reach, asserting token balances, account state, error codes and the vault conservation invariant | `./build-sbf.sh && (cd tests/svm && cargo test)` |
| Simulation | `tests/sim/`: deterministic scenario generator, pool-state model and invariant checker; the fast layer runs the model alone (`run_mc_batch.sh`), the slow layer runs it next to LiteSVM and the compiled programs (`run_svm_tests.sh`, `run_adversarial.sh`) | the scripts in `tests/sim` |

The SVM harness is a separate Cargo workspace so that Agave and LiteSVM crate versions never constrain the programs'
Anchor versions. Its only contract with the programs is the `.so` files under `target/deploy` and the mirrored account
layouts in `tests/svm/src/lib.rs` (field order is asserted by discriminator and by the balance checks).

To test the exact objects that are on chain, build them with the pinned image (`../VERIFY.md`) and place them in
`target/deploy` before running the suite.
