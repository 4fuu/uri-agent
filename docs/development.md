# Architecture and development

URI Agent is a stable-Rust terminal application. Its architecture is organized around a fixed model interface, registered protocols, append-only sessions, and one generic TUI surface. This document is the detailed engineering reference linked from [`AGENTS.md`](../AGENTS.md).

## System shape

The application assembles capabilities at startup, freezes the model context for a new session, and then runs the model/tool loop against the registered protocols:

```text
CLI + files + environment
          |
          v
  configuration + pi catalog
          |
          v
built-in plugins + discovered Skills
          |
          +----> protocol descriptors + prompt fragments
          |                         |
          |                         v
          |                generated system prompt
          |                         |
          |                         v
          |                 frozen SessionContext
          v                         |
protocol / command / TUI registries <------------+
          |
          v
   runtime model loop <----> append-only SQLite session
          |
          v
 read/exec dispatch ----> managed tasks ----> bounded or preserved output
```

For a resumed session, the stored `SessionContext` replaces newly generated prompt and Skill context. Current startup discovery must not reinterpret historical sessions.

## Repository map

| Path | Ownership |
| --- | --- |
| `src/main.rs` | Application assembly, plugin installation, Skill registration, and runtime/TUI wiring |
| `src/catalog.rs` | pi.dev model catalog, cache, `models.json` overlays, model limits, and pricing |
| `src/config.rs` | CLI parsing, layered settings, credential files, environment overrides, and dynamic values |
| `src/model.rs` | Rig provider adapters, model request compatibility, multimodal support, and the two tool schemas |
| `src/clipboard.rs` | Cross-platform clipboard image reads and PNG encoding |
| `src/prompts.rs` | Initial system prompt, model-facing tool descriptions, and shared result formatting |
| `src/protocol.rs` | `Protocol`, descriptors, registry, address splitting, dispatch, and output presentation |
| `src/builtins/` | Built-in project-instruction, binary-hint, embedded-documentation, file, exact replacement, Codex patch, Bash, and PowerShell plugins, including their protocol help |
| `src/plugin.rs` | Plugin declarations, startup notices, system prompt fragments, and protocol, command, generic panel, and status registration |
| `src/wasm_plugin.rs` | Persistent module discovery, manager protocol help, Extism ABI and host calls, dynamic protocol routing, trusted permissions, and atomic reload |
| `sdk/` | Rust guest ABI types, export macro, and built-in protocol host calls |
| `examples/wasm-plugin/` | Buildable Rust guest plugin example |
| `src/task.rs` | In-process task lifecycle, waiting, cancellation, records, and notices |
| `src/output.rs` | Inline output limits, previews, and complete-output persistence |
| `src/skill.rs` | Skill discovery, frontmatter, protocol naming and help, snapshots, and resource containment |
| `src/session.rs` | SQLite schema, session boundaries, frozen context, events, drafts, checkpoints, and replay |
| `src/compaction.rs` | Context estimation, complete-turn compaction boundaries, summaries, and retained history |
| `src/runtime.rs` | User turns, image attachments, model/tool loop, tool-call correlation, and compaction triggers |
| `src/keymap.rs` | Built-in Rhai mappings and global/project overrides |
| `src/terminal.rs` | Embedded PTY lifecycle, emulation, resize, and input encoding |
| `src/oauth/` | OAuth provider flows, callbacks, device codes, refresh, and response parsing |
| `src/tui.rs`, `src/tui/` | Conversation state, floats, rendering, interaction, animation, and model selection |

Put behavior in the module that owns the corresponding state and contract. Before adding a wrapper, helper, or type, check whether changing the existing source of truth is clearer. A one-use abstraction is justified only when it enforces a named invariant or removes real complexity.

## Linked Rust extensions

First-party capabilities use the plugin path exposed to linked Rust extensions:

1. A [`Plugin`](../src/plugin.rs) may declare protocol descriptors, startup notices, and a system prompt fragment that is added before a new session's prompt is frozen.
2. `PluginRegistry` validates descriptor names, rejects duplicates, collects notices, and preserves registration order for prompt fragments.
3. Plugins install protocols, commands, panel providers, and status providers through `PluginHost`; prompt-only plugins need no runtime registration.
4. Protocols remain behind `read` and `exec`, while commands join the searchable panel and key-bindable command registry.

Sensitive Agent environment access uses one explicit `PluginPermission::Environment` declaration. A declared plugin can obtain `PluginEnvironment` from `PluginHost` and dynamically read any user-managed variable by name or snapshot. There is no variable allowlist or approval state: the declaration is an audit marker for trusted source, while an undeclared plugin is refused by the host interface.

TUI extensions return generic documents and semantic status items. Status providers run while frames are drawn, so they must be fast and non-blocking. Keep operational behavior inside registered protocols, commands, or panel providers; reserve prompt fragments for context required before the first tool call.

URI Agent does not load native dynamic libraries. Third-party runtime protocols use the trusted [WASM plugin](plugins.md) path instead.

## Architectural contracts

### Model-facing interface and protocols

- The model sees exactly two tool definitions: `read` and `exec`.
- Split protocol addresses only at the first `://`; pass the opaque remainder and optional JSON body to the selected protocol unchanged.
- Protocol names are unique. A protocol may implement `read`, `exec`, or both.
- Each protocol documents its exact model-facing operation contract at `<protocol>://help`; implementation, tests, and help must remain synchronized.
- Prefer extending `Protocol` over adding another model-facing concept or embedding every capability in the initial prompt.
- Reserve plugin system prompt fragments for context the model must receive before its first tool call. Prompt-only plugins do not need to register a protocol.

The shared routing and execution lifecycle is in [Protocols, tasks, output, and Skills](protocols.md).

### Execution and persistence

- Asynchronous task acceptance is not completion. Status and final content remain available through the owning protocol's read route.
- URI syntax belongs to its protocol; the registry must not interpret protocol-specific options.
- Shell cancellation terminates child processes, not only the parent future.
- File writes remain atomic. Exact replacement rejects missing and ambiguous matches.
- Preserve oversized output in the session output directory and return a readable `file://` address.
- Contain Skill resource reads within the frozen Skill directory, including after following symlinks.
- Freeze the complete generated system prompt and each selected Skill's name, description, and canonical `SKILL.md` path when creating a session.
- Resume from the frozen snapshot rather than regenerating current prompt or Skill state.
- Session events are append-only. Compaction changes model replay by adding a checkpoint; it does not delete original events.
- Persist each transcript/model-replay message boundary in one transaction; streaming deltas are transient.
- Preserve provider tool-call identity during replay.
- Keep detached turns owned until completion. Session switching leaves them running; process exit cancels, durably settles, and joins them.
- Treat the canonical launch directory as the project boundary for attachments and session selection.

The exact Skill rules are in [Skills](protocols.md#skills); the user-visible persistence and compaction lifecycle is in [Sessions and context](interface.md#sessions-and-context).

### TUI and extensions

- The TUI is one conversation surface. Do not introduce Browse, Insert, or Detail modes, a slash-command syntax, or a second command path.
- Keep the interface keyboard-complete, with arrow keys and mouse input as first-class paths.
- Route configurable keys through the layered Rhai keymap, commands through `CommandRegistry`, and extension UI through generic `PluginHost` registrations.
- Preserve mouse hit regions for selectable lists and command panels.
- Preserve terminal restoration, mouse selection, and OSC52 copy on every exit and error path.

The active keys, interactions, and visible behavior are owned by [Terminal interface and sessions](interface.md), with `F1` and `:help` as the runtime reference.

### Models and configuration

- Keep provider adapters thin; catalog metadata and explicit compatibility values should drive provider-specific request shape.
- Keep fake-backend tests provider-independent. Tests must not require live credentials or network access.
- Preserve unknown downloaded model fields in the catalog cache.
- Keep global/project/environment/CLI precedence and credential precedence consistent with [Models and configuration](configuration.md).
- Treat leading `!` configuration values as code execution. Do not weaken the trust warning or accidentally log secret values.

## Change rules

### Scope and ownership

- Make the smallest change that fully implements the requested behavior.
- Keep unrelated refactors, formatting, generated data, and speculative configurability out of the patch.
- Follow references when removing behavior and delete code that exists only for that behavior.
- Do not commit credentials, sessions, complete-output files, `.uri-agent/`, `.amp/`, or `target/` artifacts.

### Protocol changes

When adding or changing a protocol:

1. update its implementation and descriptor;
2. update its `<protocol>://help` contract in the owning protocol module;
3. preserve opaque registry routing and body pass-through;
4. add focused normal-path and boundary-condition tests;
5. update [protocol documentation](protocols.md) when public cross-cutting behavior changes.

Do not add generic registry behavior for syntax that belongs to one protocol.

### WASM plugin and SDK changes

Keep `wasm_plugin://help`, [WASM plugin documentation](plugins.md), the [`uri-agent-plugin-sdk`](../sdk/) API, and the buildable example synchronized. Test both a valid module and the affected reload, collision, permission, or resource-limit boundary. Preserve whole-set replacement and keep guest host calls out of dynamic WASM routing.

### Model, Skill, session, and runtime changes

Preserve provider-independent model-loop behavior and append-only persistence. Shared changes to protocol, task, session, compaction, or runtime boundaries need both a normal-path test and the affected boundary-condition test. Session tests should verify persisted replay, not only in-memory state.

### TUI changes

Keep input and rendering paths aligned: a visible action must be keyboard-accessible, and selectable controls need mouse hit regions. Test the affected surface and its input path. For rendering changes, assert meaningful cells or text rather than only taking a snapshot.

## Verification

Use stable Rust. Before completing a code change, run:

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo check
```

Scale focused tests to the changed behavior before running the full suite. Do not require live API keys in tests.

For documentation-only changes, the full Rust suite is unnecessary unless code or documentation generation changed. Verify instead:

- relative links and referenced files exist;
- examples match current protocol, CLI, command, configuration, and keymap names;
- stated defaults and precedence match source;
- `README.md` and `README.zh-CN.md` remain equivalent;
- detailed `docs/` content stays English-only;
- protocol behavior changes are also reflected in `<protocol>://help`.

## Documentation ownership

- Root READMEs own project fit, critical warnings, the shortest successful setup, and navigation.
- [`docs/protocols.md`](protocols.md) owns cross-cutting protocol, task, output, and Skill detail.
- [`docs/plugins.md`](plugins.md) owns WASM installation, reload, ABI, trust boundaries, and runtime limits; [`sdk/README.md`](../sdk/README.md) owns Rust guest SDK usage.
- [`docs/configuration.md`](configuration.md) owns models, authentication, files, precedence, CLI override semantics, and custom providers.
- [`docs/interface.md`](interface.md) owns TUI behavior, commands, keymaps, terminal interaction, attachments, sessions, and compaction.
- This document owns architecture, module boundaries, engineering invariants, change rules, and verification.
- `uri-agent --help` owns the exact CLI contract.
- `<protocol>://help` owns the exact model-facing contract for one protocol.
- [`AGENTS.md`](../AGENTS.md) remains a concise agent entry point and links here instead of duplicating the complete manual.

Use `URI Agent` in prose and `uri-agent` for the binary, crate, commands, and filesystem names.
