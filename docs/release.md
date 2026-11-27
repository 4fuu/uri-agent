# Release

UAT versions use a calendar form `YYYY.MDD.REVISION`, for example
`2026.1030.0`. The crate version, tag, release assets, Formula, and Scoop
manifest must agree.

## Preconditions

- `main` is green with `cargo fmt --check`, `cargo check`, and `cargo test`.
- The version matches the Asia/Hong_Kong date prefix used by the release
  workflow.
- The tag points at the exact commit being released.
- Retrieval assets are staged for every target by
  `scripts/prepare-retrieval-assets.py`.

## Workflow

1. Set the version in `Cargo.toml` and update `Cargo.lock`.
2. Update the Formula and Scoop manifest to the new version, asset URLs, and
   hashes.
3. Run the local verification commands from [Development](development.md).
4. Commit the release metadata.
5. Tag `v<version>` on that commit.
6. Run the `Release` GitHub workflow from `main` with the tag in place.
7. After artifacts publish, run the installer smoke tests.

Do not move a published tag. If a release is wrong, publish a new revision.

## Assets

Each archive contains:

- the `uat` executable (or `uat.exe`);
- `libzvec_c_api` for the platform;
- the embedding model under `retrieval/models/potion-code-16M-v2/`;
- the Jieba dictionary files under `retrieval/jieba/`.

The Linux installer verifies SHA256SUMS and requires those files. The Homebrew
and Scoop installers use the same archive contents. UAT never downloads these
assets at runtime; semantic search reads them from the installation directory.

## Compatibility

Versions before the text-only migration used the URI/protocol interface. That
implementation remains on `legacy/uri-protocol`; its last version is
`2026.1030.0`. Current releases expose the `uat` binary and the traditional
tool form only.
