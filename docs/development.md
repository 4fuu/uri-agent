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
protocol / model-tool / command / TUI registries <------------+
          |
          v
   runtime model loop <----> append-only SQLite session
          |
          v
generic tool dispatch ----> protocol or direct tool ----> bounded or preserved output
```

For a resumed session, the stored `SessionContext` and session-scoped protocol
records replace newly generated prompt, protocol, and Skill context. Current
startup discovery must not reinterpret historical sessions.

## Repository map

| Path | Ownership |
| --- | --- |
| `src/main.rs` | Application assembly, plugin installation, Skill registration, and runtime/TUI wiring |
| `src/catalog.rs`, `src/catalog/` | pi.dev, built-in, and provider cloud model catalogs, including WorkBuddy cloud discovery, credential-scoped cache, `models.json` overlays, model limits, and pricing |
| `src/config.rs` | CLI parsing, layered settings, credential files, environment overrides, and dynamic values |
| `src/model/` | Public model contracts, failure classification, catalog-driven request transforms, Rig provider adapters, dedicated Cloudflare AI Gateway and WorkBuddy boundaries, Codex WebSocket and experimental Antigravity transports, and multimodal support |
| `src/oauth/`, `src/oauth/providers/` | OAuth provider registration, browser and device flows, token refresh, and WorkBuddy session identity and authenticated headers |
| `src/agent.rs` | Process-wide `AgentHost`, Agent specifications and handles, depth enforcement, child lifecycle, and background resident execution |
| `src/clipboard.rs` | Cross-platform clipboard text and image reads, with image PNG encoding |
| `src/prompts.rs` | Initial system prompt, model-facing tool descriptions, and shared result formatting |
| `src/protocol.rs` | `Protocol`, text and image read output, descriptors, registry, address splitting, dispatch, and output presentation |
| `src/builtins/` | Built-in project-instruction, embedded-documentation, file, grep, session archive, HTTPS, MCP, exact replacement, Codex patch, unified tasks, Bash, PowerShell, and model-tool plugins, including protocol help, direct-tool schemas, and provider-specific internals |
| `src/plugin.rs` | Plugin declarations, startup notices, system prompt fragments, permissions, persistent settings, and model-tool, protocol, command, generic panel, status, composer completion, and submission-effect registration |
| `src/process.rs` | Cross-platform child-process isolation, process-tree ownership, termination, and root-process reaping |
| `src/tool_download.rs` | PATH-first resolution and pinned, checksummed fallback installation for plugin-managed executables |
| `src/wasm_plugin.rs` | Persistent module discovery, manager protocol help, Extism ABI and host calls, dynamic protocol and model-tool routing, trusted permissions, and atomic reload |
| `sdk/` | ABI-v6 Rust guest types, export macro, and built-in protocol, Agent, state, resident, model-role, and plugin-setting host calls |
| `src/plugin_state.rs` | Separate global/project plugin-state SQLite storage, revisions, and compare-and-set |
| `examples/wasm-plugin/` | Buildable Rust guest plugin example |
| `src/task.rs` | In-process task lifecycle, foreground-to-background promotion, capacity, progress, waiting, cancellation, records, and notices |
| `src/output.rs` | Inline output limits, previews, complete-output persistence, and per-session JSONL diagnostics |
| `src/skill.rs` | Skill discovery, frontmatter, protocol naming and help, snapshots, and resource containment |
| `src/session.rs` | SQLite schema, session boundaries, frozen context, events, drafts, checkpoints, and replay |
| `src/compaction.rs` | Context estimation, complete-turn compaction boundaries, summaries, and retained history |
| `src/runtime.rs` | User turns, image attachments, model/tool loop, tool-call correlation, and compaction triggers |
| `src/keymap.rs` | Built-in Rhai mappings and global/project overrides |
| `src/terminal.rs` | Embedded PTY lifecycle, emulation, resize, and input encoding |
| `src/oauth/` | OAuth orchestration, callbacks, device codes, shared token parsing, and provider-owned login and refresh flows |
| `src/tui.rs`, `src/tui/` | Public TUI facade, conversation state, composer, controller/input handling, rendering, animation, Markdown, model selection, and focused tests |

Put behavior in the module that owns the corresponding state and contract. Before adding a wrapper, helper, or type, check whether changing the existing source of truth is clearer. A one-use abstraction is justified only when it enforces a named invariant or removes real complexity.

## Linked Rust extensions

First-party capabilities use the plugin path exposed to linked Rust extensions:

1. A [`Plugin`](../src/plugin.rs) may declare protocol and direct model-tool descriptors, startup notices, and a system prompt fragment that is added before a new session's prompt is frozen. A plugin with configuration-derived protocols may instead own stable session protocol records whose descriptors are restored before a resumed session installs capabilities.
2. `PluginRegistry` validates descriptor names, session-record ownership, and tool schemas, rejects duplicates, requires declarations to match installed capabilities, collects notices, and preserves registration order for prompt fragments.
3. Plugins install model tools, protocols, commands, panel providers, status providers, composer completion providers, and submission effects through `PluginHost`; they may resolve model roles, use plugin settings or separately permissioned state, create child Agents through the process-wide `AgentHost`, and opt into resident callbacks, while prompt-only plugins need no runtime registration.
4. Simple string-input capabilities remain behind `read` and `exec`; typed or escape-heavy operations should register a direct model tool, while commands join the searchable panel and key-bindable command registry.

Sensitive or model-consuming host access uses explicit plugin permissions. Environment, credentials, downloads, Agents, and separate plugin state are refused when undeclared. These declarations are audit markers for trusted source, not approval state. Model-role lookup and project-overridable `PluginSettings` require no permission.

A linked plugin can register `CommandTarget::ModelRole` with its settings
namespace, key, and default role to reuse the generic TUI selector. The
selection is an ordinary value under `pluginSettings`, separate from the
`modelRoles` route definitions. Agent creation selects prompt mode and all or
exact named tool/protocol sets in `AgentSpec`; no generic model-facing Agent
creation protocol or tool is registered.

TUI extensions return stateful semantic panel rows, tones, selection, cursor and
key-hint state; semantic status items; text replacement candidates for the
composer; or failure-isolated effects for an accepted submission. The generic
TUI renders panel state and forwards keyboard, paste, page, selection, and
activation events while the provider owns its workflow and data. Completion
of provider-owned background work calls the panel context's wake handle; the
next mutable `view` call consumes that completed state without blocking input.
Hints with actions receive generic mouse hit regions and are forwarded as the
same action events as key bindings. Composer completion
providers receive the current lines and character-based cursor position, then
return a replacement range and labeled candidates; the TUI owns popup
rendering, stale-result rejection, selection, and insertion. Status providers
run while frames are drawn, so they must be fast and non-blocking. Keep
operational behavior inside registered protocols, commands, panel providers,
or submission providers; reserve prompt fragments for context required before
the first tool call.

URI Agent does not load native dynamic libraries. Third-party runtime protocols
and direct tools use the trusted [WASM plugin](plugins.md) path instead.

## Architectural contracts

### Model-facing interface and protocols

- Linked plugins register `read`, `exec`, typed `replace`, and typed
  `apply_patch`; runtime WASM plugins may register additional model tools.
- Runtime dispatch is generic over the model-tool registry and must not
  special-case tool names.
- Every model tool, including `read` and `exec`, is declared and installed by a
  plugin rather than coupled to the runtime.
- `read` and `exec` require a string `body`. Split protocol addresses only at
  the first `://`, then pass the opaque remainder and body, including `""`, to
  the selected protocol unchanged.
- Protocol names are unique. Every protocol implements `read` so its mandatory
  `<protocol>://help` route is available; it may additionally implement `exec`.
- A protocol may declare ordered shared-help dependencies through the generic
  protocol contract. Dependencies must be registered and selected with the
  dependent protocol; the runtime requires their help reads before the
  dependent protocol's still-mandatory own help read.
- Each protocol documents its exact model-facing operation contract at `<protocol>://help`; implementation, tests, and help must remain synchronized.
- Linked protocols that return images use `Protocol::read_output` and retain a
  textual result for transcript presentation. The runtime carries images as
  typed model content, rejects them for text-only models, and persists them in
  the correlated model-message boundary. Ordinary `Protocol::read`
  implementations remain text-compatible through the default adapter.
- Keep protocol bodies semantically plain text when practical. Put operation
  selection and bounded options in the protocol-owned URI path or query. If a
  capability needs complex structured arguments, or its common calls require
  substantial escaping, register a typed direct model tool through the owning
  plugin instead of encoding that payload in a protocol body. Do not embed
  every capability in the initial prompt.
- Reserve plugin system prompt fragments for context the model must receive before its first tool call. Prompt-only plugins do not need to register a protocol.

The shared routing and execution lifecycle is in [Protocols, tasks, and output](protocols.md).

### Execution and persistence

- Protocol execution returns its final result directly by default.
- Use a managed task only when an operation necessarily runs long enough that keeping the tool call open is inappropriate, or when the caller explicitly requests background execution.
- Managed task acceptance is not completion. Status, latest output, final content, and cancellation remain available through the unified `tasks` protocol.
- URI syntax belongs to its protocol; the registry must not interpret protocol-specific options.
- Shell cancellation terminates child processes, waits for root-process cleanup, and only then settles the managed task.
- File writes remain atomic. Exact replacement rejects missing and ambiguous matches.
- Patch application preflights the complete in-memory plan before writing and
  rolls back every affected file after a commit failure.
- Preserve oversized output in the session output directory and return a readable `file://` address.
- Keep model-facing tool results separate from user-facing detail and diagnostic
  metadata. Diagnostics may record identifiers, field names, sizes, timing, and
  state, but not raw arguments, credentials, environment values, or successful
  tool output.
- Contain Skill resource reads within the frozen Skill directory, including after following symlinks.
- Freeze the complete generated system prompt and each selected Skill's name, description, and canonical `SKILL.md` path when creating a session.
- Resume from the frozen snapshot rather than regenerating current prompt or Skill state.
- Session events are append-only. Compaction changes model replay by adding a checkpoint; it does not delete original events.
- Persist each transcript/model-replay message boundary in one transaction; streaming deltas are transient.
- Preserve provider tool-call identity during replay.
- Keep detached turns owned until completion. Session switching leaves them running; process exit cancels, durably settles, and joins them.
- Treat the canonical launch directory as the project boundary for attachments and session selection.

The exact Skill rules are in [Startup context and Skills](context.md); persistence and compaction are in [Sessions and context](sessions.md).

### TUI and extensions

- The TUI is one conversation surface. Do not introduce Browse, Insert, or Detail modes, a slash-command syntax, or a second command path.
- Keep the interface keyboard-complete, with arrow keys and mouse input as first-class paths.
- Route configurable keys through the layered Rhai keymap, commands through `CommandRegistry`, and extension UI through generic `PluginHost` registrations.
- Keep completion triggers and candidate generation in providers. The composer may understand generic ranges and candidates, but not file or session reference syntax.
- Preserve mouse hit regions for selectable lists and command panels.
- Preserve terminal restoration, mouse selection, and OSC52 copy on every exit and error path.

Conversation behavior is owned by [Terminal interface](interface.md); keymaps, the embedded PTY, selection, and attachments are in [Keymaps, terminal, and attachments](terminal.md). `F1` and `:help` remain the runtime reference.

### Models and configuration

- Keep provider adapters thin; catalog metadata and explicit compatibility values should drive provider-specific request shape.
- When endpoint or authentication security cannot trust catalog transport fields, isolate that provider in its own `src/model/<provider>.rs` module. Document which catalog fields remain authoritative and which local values replace them. Future exceptions must not add provider branches to generic Rig construction or request transformation.
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

### Direct model-tool changes

When adding or changing a direct model tool:

1. implement `ModelTool` in the plugin that owns the behavior;
2. return the same descriptor from `Plugin::model_tool_descriptors` and install
   the tool through `PluginHost::model_tools`;
3. use a strict JSON Schema and reject unknown fields when decoding typed
   arguments;
4. keep runtime dispatch generic and add focused schema, dispatch, and failure
   tests;
5. update [protocol documentation](protocols.md) when the active public tool
   surface changes.

Use `PluginPermission::Downloads` before calling `PluginHost::downloads` for a
linked plugin-managed executable. The request exists to make download behavior
easy to find in source review; it does not show an approval prompt. Prefer a
working executable from `PATH`, then use a pinned URL, checksum, version check,
bounded download, process and cross-process lock, and atomic cache install.

### WASM plugin and SDK changes

Keep `wasm_plugin://help` and its detailed help paths, [WASM plugin documentation](plugins.md), the [`uri-agent-plugin-sdk`](../sdk/) API, and the buildable example synchronized. ABI v6 has no compatibility path. Test both a valid module and the affected reload, collision, permission, persistence, resident, or resource-limit boundary. Preserve whole-set replacement and keep guest host calls out of dynamic WASM routing. Agent access and separate state require manifest permissions; plugin settings do not. Enforce the global depth-2 maximum and persisted same-project depth-1 parent requirement.

### Agent, model, Skill, session, and runtime changes

Preserve the single process-wide `AgentHost`, full `AgentRuntime` behavior for every caller, provider-independent model loops, append-only persistence, durable submissions, model/thinking freeze, and atomic compaction spec-update/checkpoint invariant. Shared changes to Agent, protocol, task, session, compaction, or runtime boundaries need both a normal-path test and the affected boundary-condition test. Session tests should verify persisted replay, not only in-memory state.

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
- [`docs/protocols.md`](protocols.md) owns cross-cutting protocol, task, and output detail.
- [`docs/context.md`](context.md) owns project instructions, Skill discovery and resources, and frozen startup context.
- [`docs/plugins.md`](plugins.md) owns WASM installation, reload, ABI, trust boundaries, and runtime limits; [`sdk/README.md`](../sdk/README.md) owns Rust guest SDK usage.
- [`docs/configuration.md`](configuration.md) owns models, authentication, files, precedence, CLI override semantics, and custom providers.
- [`docs/interface.md`](interface.md) owns the conversation surface, composer, commands, and navigation.
- [`docs/terminal.md`](terminal.md) owns keymaps, terminal interaction, selection, copying, and attachments.
- [`docs/sessions.md`](sessions.md) owns persistence, project scoping, frozen session context, the model/tool loop, request retries, and compaction.
- This document owns architecture, module boundaries, engineering invariants, change rules, and verification.
- [`docs/release.md`](release.md) owns release versioning, preparation, automation, and first-release setup.
- `uri-agent --help` owns the exact CLI contract.
- `<protocol>://help` owns the exact model-facing contract for one protocol.
- [`AGENTS.md`](../AGENTS.md) remains a concise agent entry point and links here instead of duplicating the complete manual.

Use `URI Agent` in prose and `uri-agent` for the binary, crate, commands, and filesystem names.
