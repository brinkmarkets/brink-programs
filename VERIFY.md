# Verified builds

Every Brink program on chain is built by the pinned verifiable-build image from a commit in the public programs
repository, and is verified on chain straight after it is deployed. Nothing is deployed or upgraded any other way.

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
  <repository> --commit-hash <commit> --library-name brink_sale \
  --arch v0 --cargo-build-sbf-args="--tools-version v1.57"
```

Use `-ud` for devnet programs.

## After a deployment

```bash
# 1. on-chain hash equals the image build
solana-verify get-program-hash -u <cluster> <program-id>
# 2. verification record, signed by the upgrade authority (no rebuild: the hash was checked in step 1)
solana-verify verify-from-repo -u <cluster> --program-id <program-id> <repository> --commit-hash <commit> \
  --library-name <library> --arch v0 --cargo-build-sbf-args="--tools-version v1.57" --skip-build -k <authority>
# 3. mainnet-beta only: remote verification, then read the status
solana-verify remote submit-job --program-id <program-id> --uploader <authority>
solana-verify remote get-job --job-id <job-id>
solana-verify remote get-status --program-id <program-id>
```

## Deployed programs

Executable hashes as reported by `solana-verify get-program-hash`, each reproduced byte for byte by the pinned image
from the source tree in the last column.

| Program | Cluster | Program id | Executable hash | Source tree |
|---|---|---|---|---|
| `brink_sale` | mainnet-beta | `GYCJoDiZCh7mbqyWRjYJmpPUt3nPLs6887tGm58rmK3v` | `ded88914d9cba20e0a18c810aa4e37bd4c2ee2c4c87f6583bdd69e9df6f44eef` | sale release, 5 October |
| `brink_sale` | devnet | `GYCJoDiZCh7mbqyWRjYJmpPUt3nPLs6887tGm58rmK3v` | `ded88914d9cba20e0a18c810aa4e37bd4c2ee2c4c87f6583bdd69e9df6f44eef` | sale release, 5 October |
| `brink_timelock` | devnet | `CeQz4x7Ad7Hg715Tn4PHtvtAxYin2PHv4KGaD3KS3kM5` | `bf2b7c60642d49d56e8b729d67f9879f447ef78a10eb689faad3e3f1ca1932d6` | sale release, 5 October |
| `brink_allowlist` | devnet | `FaNttHSrjcQruS8qBNFDdmAnKq9Z4ZGu2y1UkMYXe1UR` | `449b52900113337565364d63dc964db2f994a547b7cd62d77d6bfde17b119dd4` | product release, 5 October |
| `brink_vaults` | devnet | `HhDTKdT36W1vE1DBgKnXDp7gFtxpxhRxovmfanuhaQiy` | `39c5ea145a9189d0414673e86d69e7a54d9fb9324bd091c061008b1726306617` | product release, 5 October |
| `brink_split` | devnet | `ASYzAAxpLwQW5XwdL1GJbpQ15onLR6uM1HBSCRTza9sz` | `ed2863865ef2e949c3e6d028ca20b2735caf9e2ef3cf95b33b93d934a3ba4193` | product release, 5 October |
| `swap_amm` | devnet | `EbD862QySygYKCMd8RHM2JaHqnduLQXB8Y632U3pQtce` | `f7be95ed518335aac8d04372d7d26f2d3f2f67b97272b77e5720d1c92dc1f2b6` | current |
| `brink_index` | devnet | `J2udZ8xzSsETsrSnbW3DeLFBTVooKRup3P7LWKSRwuvS` | `0d02256713722465ead8a2cfdb4700db2a947908e85af47855e0aad4edd91dff` | current |
| `brink_venue` | devnet | `Boj38zJFfsN72DBL2nrJEfk1ewskn11EbdikgHMBX7v2` | `493ac0586f21e50aac171907606015bd4892728aca5cc5a831212e4e39a70d68` | current |

The BRINK mint (`8s2mvyRgAGfbvRCsDWfrpjsNA3ygLaLitPdezHj6WsNr`) is an account of the standard SPL Token program; it
runs no Brink code, so there is no Brink build to verify for it.
