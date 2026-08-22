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
| `src/prompts.rs` | Initial system prompt, model-facing tool descriptions, and built-in protocol help |
| `src/protocol.rs` | `Protocol`, descriptors, registry, address splitting, dispatch, and output presentation |
| `src/builtins/` | Built-in project-instruction, binary-hint, file, exact replacement, Codex patch, Bash, and PowerShell plugins |
| `src/plugin.rs` | Plugin declarations, startup notices, system prompt fragments, and protocol, command, generic panel, and status registration |
| `src/task.rs` | In-process task lifecycle, waiting, cancellation, records, and notices |
| `src/output.rs` | Inline output limits, previews, and complete-output persistence |
| `src/skill.rs` | Skill discovery, frontmatter, protocol naming, snapshots, and resource containment |
| `src/session.rs` | SQLite schema, session boundaries, frozen context, events, drafts, checkpoints, and replay |
| `src/compaction.rs` | Context estimation, complete-turn compaction boundaries, summaries, and retained history |
| `src/runtime.rs` | User turns, image attachments, model/tool loop, tool-call correlation, and compaction triggers |
| `src/keymap.rs` | Built-in Rhai mappings and global/project overrides |
| `src/terminal.rs` | Embedded PTY lifecycle, emulation, resize, and input encoding |
| `src/oauth/` | OAuth provider flows, callbacks, device codes, refresh, and response parsing |
| `src/tui.rs`, `src/tui/` | Conversation state, floats, rendering, interaction, animation, and model selection |

Put behavior in the module that owns the corresponding state and contract. Before adding a wrapper, helper, or type, check whether changing the existing source of truth is clearer. A one-use abstraction is justified only when it enforces a named invariant or removes real complexity.

## Non-negotiable product contracts

### Model-facing interface and protocols

- The model sees exactly two tool definitions: `read` and `exec`.
- Split protocol addresses only at the first `://`. The registry treats the remainder as opaque and does not perform RFC URL parsing, decoding, normalization, or generic option handling.
- Accept any JSON value as `body` and pass it to the selected protocol unchanged.
- Protocol names are unique. A protocol may implement `read`, `exec`, or both.
- Each protocol documents its operational contract at `<protocol>://help`. Keep [built-in help](../src/prompts.rs) synchronized with behavior, including valid addresses, body shapes, asynchronous behavior, result routes, limits, and an example.
- Prefer extending `Protocol` over adding another model-facing concept or embedding every capability in the initial prompt.
- Reserve plugin system prompt fragments for context the model must receive before its first tool call. Prompt-only plugins do not need to register a protocol.

The detailed protocol behavior is in [Protocols, tasks, Skills, and extensions](protocols.md).

### Tasks, writes, and output

- Asynchronous task acceptance is not completion. Status and final content remain available through the owning protocol's read route.
- URI options belong to their protocol. `bash` and `pwsh` own `?wait=N`; the registry must not interpret it.
- A wait timeout leaves the task running.
- Shell cancellation terminates child processes, not only the parent future.
- File writes remain atomic. Exact replacement rejects missing and ambiguous matches.
- Preserve oversized output in the session output directory and return a readable `file://` address.
- Enable only an available `bash` protocol on non-Windows platforms. On Windows, keep native-shell policy in the `pwsh` plugin: require PowerShell 7 or newer, warn and leave `pwsh` disabled when unavailable, and suppress `bash` when enabled.

### Skills and sessions

- Discover Skills once at startup from the documented project and user roots. Never compile or copy a machine-specific discovered Skill list into the product.
- Normalize each accepted Skill to `<name>-skill`, keep first-wins precedence, and skip protocol collisions with a clear notice.
- Contain Skill resource reads within the frozen Skill directory, including after following symlinks.
- Append plugin system prompt fragments after the generated protocol list, preserving plugin registration order.
- Freeze the complete generated system prompt and each selected Skill's name, description, and canonical `SKILL.md` path when creating a session.
- Resume from the frozen snapshot. Never synthesize current prompt or Skill state for an old session, and never rebind a same-named Skill at another path.
- A missing frozen Skill file fails explicitly. A resumed session without frozen context is invalid.
- Session events are append-only. Compaction changes model replay by adding a checkpoint; it does not delete original events.
- Persist each transcript/model-replay message boundary in one transaction; streaming deltas are transient.
- Record provider, model, and thinking changes as events and restore their folded state on resume.
- Compaction normally keeps complete recent turns. It may split an oversized turn only at a valid message boundary that does not orphan a tool result.
- Retry a detected provider context overflow after compaction at most once per user turn.
- Preserve provider tool-call identity during replay.
- Keep detached turns owned until completion. Session switching leaves them running; process exit cancels, durably settles, and joins them.
- The canonical launch directory is the project boundary. Latest and explicit session resume cannot cross it.

The user-visible lifecycle is in [Sessions and context](interface.md#sessions-and-context).

### TUI and extensions

- The TUI is one conversation surface. Do not introduce Browse, Insert, or Detail modes, a slash-command syntax, or a second command path.
- Startup may show the splash, then the conversation. Empty history keeps the centered animated brand, working directory, provider/model/effort or login prompt, and a local compose/command/help hint; nonempty history uses role-specific transcript blocks and one highlighted footer with expanded status available through `F4` or `:status`.
- `i` opens the composer; `Enter` sends, `Shift+Enter` inserts a newline, and `Esc` preserves the draft.
- `:` opens a searchable command panel from the conversation. Its input only filters the list; commands needing a choice or text open a selector or input float instead of parsing search text.
- Final assistant responses remain readable in the transcript, while each completed turn's intermediate timeline folds into one process row. Expanding it reveals independently foldable reasoning and tool summaries. Click or `Enter` toggles a block, right-click opens its full document even during streaming, and `r`, `t`, and `h` filter or jump among reasoning, tool, and user blocks.
- Arrow keys and mouse input are first-class. `j` and `k` may remain aliases, but defaults and help must not require Vim knowledge.
- Route configurable keys through the layered Rhai keymap.
- Route command panel entries and key-bindable command IDs through `CommandRegistry`.
- Register extension protocols, commands, panels, and status through `PluginHost`. Keep panel rendering generic and plugin-specific behavior out of `src/tui.rs`.
- Keep the interface keyboard-complete and preserve mouse hit regions for selectable lists and command panels.
- Use direct drag selection in read-only views and Shift-drag where a float must keep ordinary clicks.
- Preserve terminal restoration, mouse selection, and OSC52 copy on every exit and error path.

The active user behavior and defaults are in [Terminal interface and sessions](interface.md).

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
2. update its `<protocol>://help` contract in `src/prompts.rs`;
3. preserve opaque registry routing and body pass-through;
4. add focused normal-path and boundary-condition tests;
5. update [protocol documentation](protocols.md) when public cross-cutting behavior changes.

Do not add generic registry behavior for syntax that belongs to one protocol.

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
- [`docs/protocols.md`](protocols.md) owns cross-cutting protocol, task, output, Skill, and extension detail.
- [`docs/configuration.md`](configuration.md) owns models, authentication, files, precedence, CLI options, and custom providers.
- [`docs/interface.md`](interface.md) owns TUI behavior, commands, keymaps, terminal interaction, attachments, sessions, and compaction.
- This document owns architecture, module boundaries, engineering invariants, change rules, and verification.
- `<protocol>://help` owns the exact model-facing contract for one protocol.
- [`AGENTS.md`](../AGENTS.md) remains a concise agent entry point and links here instead of duplicating the complete manual.

Use `URI Agent` in prose and `uri-agent` for the binary, crate, commands, and filesystem names.
