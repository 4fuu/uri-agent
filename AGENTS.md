# AGENTS.md

Guidance for coding agents working anywhere in this repository. This file is the entry point; detailed contracts belong to the linked documents.

## Project contract

URI Agent is a Rust terminal coding agent with a fixed model-facing interface:

```text
read(uri, body?)
exec(uri, body?)
```

Capabilities are registered as protocols and publish their operational instructions at `<protocol>://help`. Preserve this design: do not add another model-facing tool or place every capability in the initial system prompt.

## Read before changing

Before changing code, read the applicable repository map, change rules, and verification guidance in [`docs/development.md`](docs/development.md). Then read every focused document that owns the affected behavior:

| Area | Authoritative detail |
| --- | --- |
| Protocol routing, built-ins, tasks, output | [`docs/protocols.md`](docs/protocols.md) |
| Project instructions, binary hints, Skills, frozen startup context | [`docs/context.md`](docs/context.md) |
| WASM installation, reload, ABI, permissions, SDK | [`docs/plugins.md`](docs/plugins.md) |
| Models, authentication, configuration, CLI overrides, custom providers | [`docs/configuration.md`](docs/configuration.md) |
| Conversation UI, composer, commands, navigation | [`docs/interface.md`](docs/interface.md) |
| Keymaps, embedded terminal, selection, attachments | [`docs/terminal.md`](docs/terminal.md) |
| Session persistence, scoping, model/tool loop, retries, compaction | [`docs/sessions.md`](docs/sessions.md) |
| Module ownership, linked Rust extensions, invariants, change rules, verification | [`docs/development.md`](docs/development.md) |
| Versioning and release workflow | [`docs/release.md`](docs/release.md) |
| Documentation ownership or an unclear destination | [`docs/README.md`](docs/README.md) |

Read all applicable documents for a cross-domain change; unrelated documents are not required. `uri-agent --help` defines the exact CLI contract, while `<protocol>://help` defines a protocol's exact model-facing contract. If implementation, tests, help, or documentation disagree, make them consistent.

## Working rules

- Preserve the project contract and the architectural contracts in the applicable focused document.
- Put behavior in the module that owns its state and contract; prefer changing the source of truth over adding a wrapper or duplicated type.
- Make the smallest complete change. Leave unrelated refactors, formatting, and speculative configurability alone.
- Follow references when removing behavior and remove code that exists only for that behavior.
- Do not commit credentials, generated sessions, complete-output files, `.uri-agent/`, `.amp/`, or `target/` artifacts.

## Verification

Use stable Rust. Add focused tests beside changed behavior, then run the checks required by [`docs/development.md#verification`](docs/development.md#verification). Tests must not require live credentials or network access.

For documentation-only changes, use the documentation verification path in that section; the full Rust suite is unnecessary unless code or documentation generation changed.

## Documentation changes

- Keep `README.md` and `README.zh-CN.md` equivalent and focused on adoption, first success, critical warnings, and navigation.
- Update both root READMEs when public setup or top-level behavior changes.
- Update the owning detailed document when domain behavior changes; do not copy mutable detail into other layers.
- Update `<protocol>://help` whenever a protocol's model-facing operations change.
