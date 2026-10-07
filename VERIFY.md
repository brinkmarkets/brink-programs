# Verified builds

Every Brink program on chain is built by the pinned verifiable-build image from a commit in the public programs
repository, `https://github.com/brinkmarkets/brink-programs`, and is verified on chain straight after it is deployed.
Nothing is deployed or upgraded any other way.

## The rule

1. Build only with the pinned image, from a commit that is already public. Host builds are for development and tests.
2. Test those exact objects: the LiteSVM suite loads the image-built `.so` files before anything is deployed.
3. Deploy by buffer write and upgrade, signed by the upgrade authority.
4. Verify in the same session as the deployment:
   - the on-chain program hash equals the executable hash of the image build;
   - the verification record is written by the upgrade authority, naming the repository, the commit and the build
     arguments (an upgrade clears the previous verification, so the record is rewritten after every upgrade);
   - on mainnet-beta the remote verification job is submitted and its result is recorded (remote verification covers
     mainnet-beta only; on devnet the record and the reproducible hash are the verification).
5. Record the commit, executable hash, slot and signatures in the deployment record.

## Build parameters

| Parameter | Value |
|---|---|
| Image | `solanafoundation/solana-verifiable-build:3.1.10`, selected by `[workspace.metadata.cli] solana = "3.1.10"` in `Cargo.toml` |
| Image digest | `sha256:f71be5ca7620b7e40933b7f1294fa44e01d08c1fc5ba1f375a2478f5a01580d3` |
| Platform tools | `v1.57`, passed as `--cargo-build-sbf-args="--tools-version v1.57"` |
| Architecture | SBPF v0, `--arch v0` |
| Mount path | the workspace root |
| Library names | `swap_amm`, `brink_index`, `brink_timelock`, `brink_sale`, `brink_allowlist`, `brink_venue`, `brink_vaults`, `brink_split` |

## Reproduce a build

From a clean clone at the recorded commit:

```bash
solana-verify build --library-name brink_sale --arch v0 --cargo-build-sbf-args="--tools-version v1.57"
solana-verify get-executable-hash target/deploy/brink_sale.so
solana-verify get-program-hash -um GYCJoDiZCh7mbqyWRjYJmpPUt3nPLs6887tGm58rmK3v
```

End to end, building in the image and comparing with the chain in one command:

```bash
solana-verify verify-from-repo -um --program-id GYCJoDiZCh7mbqyWRjYJmpPUt3nPLs6887tGm58rmK3v \
  https://github.com/brinkmarkets/brink-programs --commit-hash a17664c4969f0444bcb42b612665b86ad7bdbbd1 \
  --library-name brink_sale --arch v0 --cargo-build-sbf-args="--tools-version v1.57"
```

Use `-ud` for devnet programs, with the commit of the tag in the table below.

## After a deployment

```bash
# 1. on-chain hash equals the image build
solana-verify get-program-hash -u <cluster> <program-id>
# 2. verification record, signed by the upgrade authority (no rebuild: the hash was checked in step 1)
solana-verify verify-from-repo -u <cluster> --program-id <program-id> https://github.com/brinkmarkets/brink-programs \
  --commit-hash <commit> --library-name <library> --arch v0 --cargo-build-sbf-args="--tools-version v1.57" \
  --skip-build -k <authority>
# 3. mainnet-beta only: remote verification, then read the status
solana-verify remote submit-job --program-id <program-id> --uploader <authority>
solana-verify remote get-job --job-id <job-id>
solana-verify remote get-status --program-id <program-id>
```

## Deployed programs

Executable hashes as reported by `solana-verify get-program-hash`, each reproduced byte for byte by the pinned image
from the source tree in the last column. Source trees are git tags of the public repository; `git checkout <tag>`
gives the exact tree, and the commit each tag points at is the commit recorded on chain and passed to `solana-verify`.

| Program | Cluster | Program id | Executable hash | Source tree |
|---|---|---|---|---|
| `brink_sale` | mainnet-beta | `GYCJoDiZCh7mbqyWRjYJmpPUt3nPLs6887tGm58rmK3v` | `ded88914d9cba20e0a18c810aa4e37bd4c2ee2c4c87f6583bdd69e9df6f44eef` | tag `programs-2026-10-05-sale` |
| `brink_sale` | devnet | `GYCJoDiZCh7mbqyWRjYJmpPUt3nPLs6887tGm58rmK3v` | `ded88914d9cba20e0a18c810aa4e37bd4c2ee2c4c87f6583bdd69e9df6f44eef` | tag `programs-2026-10-05-sale` |
| `brink_timelock` | devnet | `CeQz4x7Ad7Hg715Tn4PHtvtAxYin2PHv4KGaD3KS3kM5` | `68d50b228e0bb99ffa445153b3b46fdd15de3f1ab4ccc78be2027548086f0f6d` | tag `programs-2026-10-07-round-12`; program source unchanged since |
| `brink_allowlist` | devnet | `FaNttHSrjcQruS8qBNFDdmAnKq9Z4ZGu2y1UkMYXe1UR` | `651b8715c02b480fb5718948b26def2cc0a1dbba64f79a4f4f6e011f2ca67f05` | tag `programs-2026-10-07-round-12`; program source unchanged since |
| `brink_vaults` | devnet | `HhDTKdT36W1vE1DBgKnXDp7gFtxpxhRxovmfanuhaQiy` | `ebe896cc59b702d812201d533f2f35dbf47d47c753c19ab30e5bf8da8a1200b0` | tag `programs-2026-10-07-round-12`; program source unchanged since |
| `brink_split` | devnet | `ASYzAAxpLwQW5XwdL1GJbpQ15onLR6uM1HBSCRTza9sz` | `7ad25ec4b21ed383b13d5e89763a3179d9907511e3dcc242bab28a485134d5b9` | tag `programs-2026-10-07-round-12`; program source unchanged since |
| `swap_amm` | devnet | `EbD862QySygYKCMd8RHM2JaHqnduLQXB8Y632U3pQtce` | `d27dece9ce52c3819ae8e2b1fd02d019d9f1f3791307eea7e352165f98b0f931` | tag `programs-2026-10-07-round-12`; program source unchanged since |
| `brink_index` | devnet | `J2udZ8xzSsETsrSnbW3DeLFBTVooKRup3P7LWKSRwuvS` | `6a16d35a592f454ac014935ebf60ef5ccd4a315ba77fb1b0f573210e4646ba7f` | tag `programs-2026-10-07-round-12`; program source unchanged since |
| `brink_venue` | devnet | `Boj38zJFfsN72DBL2nrJEfk1ewskn11EbdikgHMBX7v2` | `76827757250e297e22fa3e2f37cf480fbc6012c543b2225d62bf5dfeda7beb82` | tag `programs-2026-10-07-round-12`; program source unchanged since |

## On-chain verification records

Written on 7 October 2026 by the upgrade authority `GJo68McMNbsWpj4prqSxR658Jgcvx7hgMvHEjFRXxfLB` with
`solana-verify verify-from-repo --skip-build` after the hash comparison above. Each record names the repository,
the commit and the build arguments `--library-name <library> --arch v0 --cargo-build-sbf-args="--tools-version v1.57"`
and can be read with `solana-verify get-program-pda --program-id <id> -s GJo68McMNbsWpj4prqSxR658Jgcvx7hgMvHEjFRXxfLB`.

| Program | Cluster | Tag | Commit | Record transaction |
|---|---|---|---|---|
| `brink_sale` | mainnet-beta | `programs-2026-10-05-sale` | `a17664c4969f0444bcb42b612665b86ad7bdbbd1` | `38yQqzJ8VmVewayzzj4mUofi5TZJfARHwuQgYw8G6eC1HvAknhBViWHgyw9BBNxD9M62trREQHbaV35bFzaMC91N` |
| `brink_sale` | devnet | `programs-2026-10-05-sale` | `a17664c4969f0444bcb42b612665b86ad7bdbbd1` | `58694bubaGDoXdy2eLPnFbFANP1GniRWiJFKV8g6uH8ZKDSTTVeJsEQ2KYjbeBLshDUWwhT6SQnp23QS3FmagEcV` |
| `swap_amm` | devnet | `programs-2026-10-07-round-12` | `87e2ffc57e41a8ad4a695de6c37d760831650c54` | `3GCVYqemeG8mDg21ZoAAhjwVSF9SC2z2A4g4nWaZjDtSVtyr2Jv8NaexJd6e9v3Hv8sUNgKzx1KJDHoMRRYeCewu` |
| `brink_index` | devnet | `programs-2026-10-07-round-12` | `87e2ffc57e41a8ad4a695de6c37d760831650c54` | `7ymKDFDp9CDikm2gpYt2vQzs9J5H83mkk6oY7ArubP7LX5Ho71DkffnXbUR5ug6ReS8Kc3Qqa9EGorhdVsuRbJj` |
| `brink_timelock` | devnet | `programs-2026-10-07-round-12` | `87e2ffc57e41a8ad4a695de6c37d760831650c54` | `483SAVSBfj9UVR4SAW45JYj6RZoGCLDsk7h3RT4ndxNQy2NtKxhcJZZXLc1yxFdgAXL3NJaZn71Rqv5cVcj7gs7k` |
| `brink_allowlist` | devnet | `programs-2026-10-07-round-12` | `87e2ffc57e41a8ad4a695de6c37d760831650c54` | `2ywLaK6qs1BQZE3J9XuBDhk1sVFhbwrBwfebQ3KTk7Si4PXmGpMCQx2FxCefbU4rXhixJEyrQQwoZ6tUotJeKagF` |
| `brink_venue` | devnet | `programs-2026-10-07-round-12` | `87e2ffc57e41a8ad4a695de6c37d760831650c54` | `sMpDJ8EstS721JhBb6gJ7pnwwBAo6GN2vpfoUaQN3Xi8ZGsjGaN3B5jpi7GcRaCnJ5tLfmFAH7kALz6KsLUvNWh` |
| `brink_vaults` | devnet | `programs-2026-10-07-round-12` | `87e2ffc57e41a8ad4a695de6c37d760831650c54` | `3RsjJWtLbMopgqxT1Jw3gydPRN6JdRphf81xhP4TwU8Y44pnmuoqjVjgAerNX3NBn2oLRKAsZNNzMAkzLzaMSP7F` |
| `brink_split` | devnet | `programs-2026-10-07-round-12` | `87e2ffc57e41a8ad4a695de6c37d760831650c54` | `3c9MbjgjUVw4o8izh3G8ZdZFucTEMUnEspF7UNc9EQHwWuMUZhgLMuY2z2WkbDjxaviAH9aTkR9ptydWqhH7zoKt` |

### Remote verification, mainnet-beta

The remote verifier rebuilt `brink_sale` from the public repository at the recorded commit and matched the chain:
job `3fb8fd18-d510-413c-8768-9b813650e544`, completed 2026-10-07T19:12:08Z, on-chain hash and executable hash both
`ded88914d9cba20e0a18c810aa4e37bd4c2ee2c4c87f6583bdd69e9df6f44eef`. The explorers read this status from
`https://verify.osec.io/status/GYCJoDiZCh7mbqyWRjYJmpPUt3nPLs6887tGm58rmK3v` and show the program as verified. The
remote service covers mainnet-beta only; for the devnet programs the signed record and the reproducible hash are the
verification, and `solana-verify verify-from-repo -ud` with the commit in the table reproduces it end to end.

Records are per deployment: an upgrade clears the verification, so after every upgrade the hash comparison, the record
and, on mainnet-beta, the remote job are repeated before the deployment is reported.

The BRINK mint (`8s2mvyRgAGfbvRCsDWfrpjsNA3ygLaLitPdezHj6WsNr`) is an account of the standard SPL Token program; it
runs no Brink code, so there is no Brink build to verify for it.
