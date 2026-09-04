# Architecture and development

URI Agent is a stable-Rust terminal application organized around a fixed model
interface, registered extensions, append-only sessions, and one generic TUI.
This is the engineering reference linked from [`AGENTS.md`](../AGENTS.md).

## System shape

```text
CLI + files + environment
          |
          v
configuration + catalogs + startup discovery
          |
          +----> plugin and protocol descriptors
          |                  |
          |                  v
          |          generated system prompt
          |                  |
          v                  v
tool / protocol / command / TUI registries
          |                  |
          v                  v
      runtime model loop <-> append-only SQLite session
          |
          v
generic tool dispatch -> protocol or direct tool -> bounded output
```

New sessions freeze generated prompt, protocol, and Skill state. Resumed
sessions restore that snapshot instead of reinterpreting current startup
discovery.

## Repository map

| Path | Ownership |
| --- | --- |
| `src/main.rs` | Application assembly and runtime/TUI wiring |
| `src/acp/` | ACP v1 transport, sessions, content mapping, and MCP handoff |
| `src/catalog.rs`, `src/catalog/` | Pi, built-in, provider, and user model catalogs |
| `src/config.rs` | CLI, settings, credentials, environment, and precedence |
| `src/oauth/` | Provider login and refresh flows |
| `src/model/` | Model contracts, request transforms, and provider adapters |
| `src/agent.rs` | Process-wide Agent ownership, depth, and lifecycle |
| `src/plugin.rs` | Linked plugin declarations and extension registries |
| `src/protocol.rs` | Protocol contract, routing, dispatch, and output projection |
| `src/prompts.rs` | Initial prompt, tool descriptions, and shared formatting |
| `src/builtins/` | Linked protocols, direct tools, commands, and providers |
| `src/retrieval/` | Model2Vec/zvec indexing and semantic or hybrid ranking |
| `src/task.rs` | Managed work, waiting, cancellation, and notices |
| `src/output.rs` | Inline limits, complete output, and diagnostics |
| `src/skill.rs` | Skill discovery, naming, snapshots, and resource containment |
| `src/session.rs` | SQLite schema, events, drafts, checkpoints, and replay |
| `src/compaction.rs` | Rollover and summary checkpoint strategies |
| `src/runtime.rs` | User turns, model/tool loop, retries, and checkpoint triggers |
| `src/keymap.rs`, `src/terminal.rs` | Key bindings and embedded PTY behavior |
| `src/tui.rs`, `src/tui/` | Conversation state, controllers, rendering, and UI tests |
| `src/wasm_plugin.rs` | WASM discovery, ABI, permissions, and dynamic dispatch |
| `src/plugin_state.rs` | Global and project plugin-state databases |
| `src/process.rs`, `src/atomic_file.rs` | Process-tree and atomic-file primitives |
| `src/tool_download.rs` | Pinned fallback downloads for plugin-managed tools |
| `sdk/`, `examples/wasm-plugin/` | Guest ABI, SDK, and buildable example |

Put behavior in the module that owns its state and contract. Add a helper or
type only when it enforces a named invariant, removes real duplication, or
matches an established reusable boundary.

## Linked Rust extensions

A [`Plugin`](../src/plugin.rs) declares prompt fragments, permissions, session
records, protocols, typed model tools, commands, and generic TUI providers.
`PluginRegistry` validates declarations against installed capabilities, rejects
name collisions, and preserves prompt-fragment order before a new session is
frozen.

Simple string-input operations belong behind `read` or `exec`; structured or
escape-heavy operations use typed direct tools. Commands join the shared
registry, and extension UI returns semantic state for generic panels, status,
composer completion, or submission effects. Keep operational behavior in those
registered interfaces rather than prompt prose.

Environment, credentials, downloads, Agents, and separate plugin state require
declared permissions. These are source-audit markers for trusted code, not
interactive approval. Model roles and project-overridable plugin settings do
not require permissions.

URI Agent links one fixed native dynamic library for first-party zvec retrieval.
It is not a general native plugin ABI; third-party runtime extensions use the
trusted [WASM path](plugins.md).

## Native retrieval assets

Each release binds an exact zvec crate and native archive, Jieba dictionary,
and Model2Vec revision. Checksums, targets, and staging layout live in
[`scripts/prepare-retrieval-assets.py`](../scripts/prepare-retrieval-assets.py).
The application crate is not published independently because a Cargo-installed
binary would omit these matched assets.

For a native Unix development build:

```bash
target=x86_64-unknown-linux-gnu # or aarch64-unknown-linux-gnu / aarch64-apple-darwin
stage="target/retrieval-assets/$target"
python3 scripts/prepare-retrieval-assets.py --target "$target" --output "$stage"
ZVEC_LIB_DIR="$stage" cargo build --locked --package uri-agent
cp target/debug/uri-agent "$stage/"
LD_LIBRARY_PATH="$stage${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}" \
  "$stage/uri-agent" --version # use DYLD_LIBRARY_PATH="$stage" on macOS
```

Windows uses target `x86_64-pc-windows-msvc`, `uri-agent.exe`, and
`zvec_c_api.dll`. To exercise the complete native retrieval path after staging
host assets:

```bash
URI_AGENT_TEST_RETRIEVAL_ASSETS="$stage" ZVEC_LIB_DIR="$stage" \
  LD_LIBRARY_PATH="$stage${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}" \
  cargo test --locked --package uri-agent --lib \
  retrieval::tests::bundled_assets_rebuild_and_search -- --ignored --exact
```

## Architectural contracts

### Registration and model interface

- Runtime dispatch is generic over registered model tools and must not
  special-case tool names.
- Every tool is declared and installed by a plugin. Protocol names and tool
  names are unique.
- `read` and `exec` always receive a string body. The registry splits only the
  first `://` and passes the opaque remainder and body unchanged.
- Every protocol implements its mandatory help read. Exact protocol behavior
  belongs to `<protocol>://help`; implementation, tests, and help change
  together.
- Protocol images remain typed model content while retaining a textual
  transcript projection. Text-only models reject image content.
- System-prompt fragments contain only context required before the first tool
  call. Full operation instructions remain on demand.

### Execution and persistence

- Operations return final results directly unless they necessarily become long
  running or the caller explicitly requests managed work.
- Task acceptance is not completion. Terminal state and complete output remain
  available after resume; processes themselves do not restart.
- Cancellation owns and reaps process trees before work settles.
- File edits are atomic. Patch application preflights all files and rolls back
  a partial commit.
- Oversized results preserve complete bytes and return a readable address.
  Diagnostics never copy secrets, raw arguments, or successful tool output.
- Session events are authoritative and append-only. Derived indexes may be
  removed and rebuilt without changing replay.
- A transcript/model message boundary commits in one transaction, including
  correlated tool results and typed image content.
- Rollover and summary add checkpoints rather than deleting history. Resumed
  sessions use frozen startup state and preserve provider tool-call identity.
- Recovery views share stable record anchors and exclude their own protocol
  calls so searches do not recursively alter the indexed corpus.
- Semantic caches bind source revisions and retrieval assets, verify freshness
  before ranked reads, and never advertise partial updates as current.

### Configuration, providers, and UI

- Catalog metadata drives generic provider behavior. Security-sensitive
  endpoint or authentication exceptions live in dedicated provider modules,
  not branches spread through generic construction.
- Configuration and credential precedence must remain consistent with [Models
  and configuration](configuration.md). Leading `!` values remain explicit
  code execution and must never be logged with secret output.
- Provider-independent tests use fake backends and require no live credentials
  or network access.
- The TUI remains one keyboard-complete conversation surface. Commands,
  keymaps, completion, status, panels, and submission effects use their generic
  registries rather than second routing paths.
- Terminal restoration, selection, mouse input, and OSC52 copy remain correct
  on normal and error exits.

## Change rules

### Scope and ownership

- Make the smallest complete change and leave unrelated cleanup out.
- Follow references when removing behavior and remove code that exists only for
  it.
- Do not commit credentials, generated sessions, complete outputs,
  `.uri-agent/`, `.amp/`, or `target/` artifacts.

### Contract changes

When changing a protocol, update its implementation, descriptor, help, and
focused tests. Also update shared protocol documentation only when the stable
cross-protocol behavior changes.

When changing a direct model tool, keep the schema strict, register it through
the owning plugin, and test validation, dispatch, presentation, and failure
paths. Do not add runtime branches for its name.

WASM ABI changes require synchronized host, SDK, example, ABI version,
compatibility tests, and plugin documentation. Reject unsupported versions
explicitly rather than guessing compatibility.

For session, Agent, model, Skill, ACP, or TUI changes, preserve the applicable
contracts above and read the owning focused document before editing. Test both
fresh and restored state when persistence is involved, and keyboard plus mouse
paths when an interaction is affected.

## Verification

Use stable Rust. For code changes, run focused tests first, then:

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo check
```

Tests must not require live credentials or network access.

For documentation-only changes, verify instead:

- relative links and referenced files exist;
- examples match current protocol, CLI, command, configuration, and keymap
  names;
- defaults and precedence match source;
- `README.md` and `README.zh-CN.md` remain equivalent;
- detailed `docs/` content remains English-only;
- protocol behavior changes are also reflected in `<protocol>://help`.

## Documentation ownership

- Root READMEs own project fit, public coverage data, critical warnings, first
  success, and navigation.
- [`docs/acp.md`](acp.md) owns ACP transport and session lifecycle.
- [`docs/protocols.md`](protocols.md) owns cross-protocol, task, and output
  concepts; runtime help owns exact protocol syntax.
- [`docs/context.md`](context.md) owns project instructions and Skills.
- [`docs/plugins.md`](plugins.md) and [`sdk/README.md`](../sdk/README.md) own WASM
  runtime and guest SDK usage.
- [`docs/configuration.md`](configuration.md) owns models, authentication,
  settings, MCP configuration, and precedence; `uri-agent --help` owns exact
  CLI syntax.
- [`docs/interface.md`](interface.md) and [`docs/terminal.md`](terminal.md) own
  interaction concepts; `F1` and `:help` own active commands and keys.
- [`docs/sessions.md`](sessions.md) owns persistence, Agents, retries, and
  checkpoints.
- This document owns architecture, module boundaries, engineering invariants,
  change rules, and verification.
- [`docs/release.md`](release.md) owns release procedure.

Use `URI Agent` in prose and `uri-agent` for the binary, crate, commands, and
filesystem names.
