# Release process

URI Agent and `uri-agent-plugin-sdk` share a Cargo version in the form `YYYY.MDD.REVISION`. Use the calendar date in `Asia/Hong_Kong`: the month is not padded, the day is always two digits, and the first release of the day uses revision zero. For example, `2026.823.0` is the first release on 2026-08-23.

## Prepare a release

On `main`, run:

```bash
python3 scripts/set-version.py 2026.823.0
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo check
```

Manually dispatch `release.yml` with that version. Its date must match the current date in `Asia/Hong_Kong`.

The workflow reruns verification, publishes the SDK before the application crate, builds x86-64 and ARM64 Linux archives, an Apple Silicon macOS archive, and a 64-bit Windows archive, generates `SHA256SUMS`, updates the in-repository Homebrew formula and Scoop manifest, commits that metadata to `main`, creates the `v<version>` GitHub Release, and tests all three installation paths. It aborts if `main` moves while release jobs are running.

## First-release setup

Before the first release:

1. Make the repository public and create a protected GitHub environment named `release`.
2. Add a crates.io API token as the environment secret `CARGO_REGISTRY_TOKEN`; crates.io requires a token for each crate's first publication.
3. Allow GitHub Actions to write repository contents and ensure the release job can push its package-metadata commit to `main`.

After both crates have been published once, configure a crates.io trusted publisher for each crate with repository `4fuu/uri-agent`, workflow `release.yml`, and environment `release`. Then delete `CARGO_REGISTRY_TOKEN`; later releases use crates.io OIDC authentication.
