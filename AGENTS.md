# AGENTS.md

Guidance for coding agents working anywhere in this repository.

## Project contract

URI Agent is a Rust terminal coding agent with a fixed model-facing interface:

```text
read(uri, body?)
exec(uri, body?)
```

Capabilities are registered as protocols and publish their operational instructions at `<protocol>://help`. Preserve this design instead of adding model-facing tools or placing every capability in the initial system prompt.

## Read the relevant design first

The detailed engineering reference is [`docs/development.md`](docs/development.md). Read the focused document before changing its area:

| Area | Authoritative detail |
| --- | --- |
| Protocol routing, built-ins, tasks, output, Skills, plugins | [`docs/protocols.md`](docs/protocols.md) |
| Models, authentication, configuration, CLI, custom providers | [`docs/configuration.md`](docs/configuration.md) |
| TUI, commands, keymaps, terminal, attachments, sessions, compaction | [`docs/interface.md`](docs/interface.md) |
| Module ownership, invariants, change rules, verification | [`docs/development.md`](docs/development.md) |

Use [`docs/README.md`](docs/README.md) as the documentation index. Exact CLI behavior remains authoritative in `uri-agent --help`; exact protocol behavior remains authoritative in `<protocol>://help` and its implementation.

## Non-negotiable invariants

- The model sees exactly two tools: `read` and `exec`.
- Split a protocol address only at the first `://`. Pass the opaque remainder and any JSON `body` to the selected protocol unchanged.
- Protocol names are unique. A protocol may implement `read`, `exec`, or both.
- Keep each built-in protocol's help in `src/prompts.rs` synchronized with its addresses, body shape, asynchronous behavior, result routes, limits, and examples.
- Task acceptance is not completion. Results remain observable through the owning protocol's read route. Protocol-specific options stay protocol-specific; `?wait=N` belongs only to `bash` and `pwsh`, and timeout leaves the task running.
- Keep file writes atomic, terminate shell child processes on cancellation, and preserve oversized output behind a readable `file://` address.
- Register `bash` and `pwsh` only when their executables exist.
- Discover Skills once at startup, preserve first-wins protocol naming and containment, and freeze the generated prompt plus selected Skill metadata and canonical paths in every new session.
- Resume only from frozen session context. Session events remain append-only; compaction adds checkpoints, keeps complete user turns, and never separates a tool call from its result.
- Treat canonical `--cwd` as the project boundary for attachments and session resume.
- Keep one keyboard-complete conversation surface. Route commands through `CommandRegistry`, configurable keys through the layered Rhai keymap, and extension UI through generic `PluginHost` registrations.
- Preserve terminal restoration, mouse selection, and OSC52 copy on normal and error exits.

## Working rules

- Put behavior in the module that owns its state and contract; the repository map is in [`docs/development.md`](docs/development.md#repository-map).
- Prefer changing the source of truth over adding wrappers, adapters, one-use helpers, or duplicated types.
- Make the smallest complete change. Leave unrelated refactors, formatting, and speculative configurability alone.
- Follow references when removing behavior and remove code that exists only for that behavior.
- Keep provider adapters thin and fake-backend tests independent of live keys or network access.
- Do not commit credentials, generated sessions, complete-output files, `.uri-agent/`, `.amp/`, or `target/` artifacts.

## Tests and verification

Use stable Rust and add focused tests beside changed behavior. Shared protocol, task, session, compaction, or runtime changes need a normal-path test and the affected boundary-condition test. TUI changes must cover the affected surface and input path.

Before completing a code change, run:

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo check
```

For documentation-only changes, verify links, examples, current names, defaults, precedence, and English/Chinese README parity. The full Rust suite is unnecessary unless code or documentation generation changed.

## Documentation rules

- Keep `README.md` and `README.zh-CN.md` equivalent and focused on adoption, first success, critical warnings, and navigation.
- Maintain detailed documents under `docs/` in English only. Follow the ownership map in [`docs/development.md`](docs/development.md#documentation-ownership) instead of duplicating mutable detail.
- Update both root READMEs when public setup or top-level behavior changes. Update the owning detailed document for protocol, configuration, interface, session, or developer behavior.
- Keep model-facing protocol operations in `<protocol>://help` rather than expanding the initial prompt or README.
- Use `URI Agent` in prose and `uri-agent` for the binary, crate, commands, and filesystem names.
