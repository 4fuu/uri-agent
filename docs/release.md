# Release process

URI Agent and `uri-agent-plugin-sdk` share a calendar version in the form
`YYYY.MDD.REVISION`, using the date in `Asia/Hong_Kong`. The month is not padded,
the day is two digits, and the first release of a day uses revision zero. For
example, `2026.823.0` is the first release on 2026-08-23.

## Prepare a release

On `main`, set the version and run the full verification path:

```bash
python3 scripts/set-version.py 2026.823.0
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo check
```

Manually dispatch `release.yml` with the same version. Its date must match the
current date in `Asia/Hong_Kong`.

The workflow verifies again, publishes the SDK crate, builds Linux x86-64 and
ARM64, Apple Silicon macOS, and 64-bit Windows archives, generates checksums,
updates the Homebrew formula and Scoop manifest, commits package metadata to
`main`, creates the GitHub release, and tests all installation paths. It aborts
if `main` moves while the jobs run.

The application crate has `publish = false`. Release archives are the supported
distribution unit because the executable requires matched native retrieval
assets.

## Bundled retrieval runtime

[`scripts/prepare-retrieval-assets.py`](../scripts/prepare-retrieval-assets.py)
is the source of truth for pinned zvec archives, the Model2Vec model, Jieba
dictionaries, licenses, revisions, checksums, supported targets, and staged
layout. Update those values together with the Cargo dependency and any affected
retrieval schema or extractor version.

The release workflow verifies asset checksums, executable-relative dynamic
library lookup, target glibc ceilings, staged versions, and installer layout.
The x86-64 Linux job also creates a real collection and runs semantic and hybrid
queries. URI Agent never downloads these assets at runtime.
