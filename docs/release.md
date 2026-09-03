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

The workflow reruns verification, publishes the SDK crate, builds x86-64 and
ARM64 Linux archives, an Apple Silicon macOS archive, and a 64-bit Windows
archive, generates `SHA256SUMS`, updates the in-repository Homebrew formula and
Scoop manifest, commits that metadata to `main`, creates the `v<version>`
GitHub Release, and tests all three installation paths. It aborts if `main`
moves while release jobs are running. The application has `publish = false`:
release archives are its supported distribution unit because the executable
requires matched native and model assets.

## Bundled retrieval runtime

[`scripts/prepare-retrieval-assets.py`](../scripts/prepare-retrieval-assets.py)
is the release source of truth for the fixed retrieval bundle. It downloads
only pinned URLs and rejects every file whose SHA256 differs. The current
bundle binds:

- `zvec-rust` and zvec native archives at 0.7.0;
- `minishlab/potion-code-16M-v2` at revision
  `e9d2a44ca6a05ac6685f3b23709ea57eb7352d5b`;
- cppjieba dictionaries at commit
  `b3602bef7d1f67521a61788a74fb5801a0e62cd3`.

The zvec 0.7.0 prebuilt core reports `v0.0.0` through its C version function,
so it is identified by the exact Rust crate, release URL, and archive checksum
instead of that value. Updating zvec or the model requires changing the exact
Cargo dependency, Rust manifest constants, preparation-script revision and
checksums, and any affected schema or extractor version in one change.

Every release archive has this executable-relative shape:

```text
uri-agent[.exe]
libzvec_c_api.so | libzvec_c_api.dylib | zvec_c_api.dll
retrieval/models/potion-code-16M-v2/{model.safetensors,tokenizer.json,LICENSE}
retrieval/jieba/{jieba.dict.utf8,hmm_model.utf8,LICENSE}
retrieval/licenses/{zvec-LICENSE,zvec-NOTICE}
```

Linux releases use GNU targets. The x86-64 bundle has a glibc 2.27 ceiling;
the ARM64 bundle has a glibc 2.38 ceiling set by zvec's prebuilt native library.
The workflow verifies the executable RPATH (`$ORIGIN` on Linux and
`@executable_path` on macOS), dependency resolution, target-specific maximum
required glibc version, staged executable version, and all asset checksums. The
x86-64 Linux build also creates a real zvec collection and executes semantic
and hybrid queries with the bundled model and Jieba files. Installer smoke
tests verify that Linux, Homebrew, and Scoop retain the complete directory
layout. The running application never downloads retrieval assets.
