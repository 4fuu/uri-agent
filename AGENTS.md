# Repository Guide

## Purpose

URI Agent is a Rust coding agent whose entire model-facing tool surface is `read(uri, body?)` and `exec(uri, body?)`. Capabilities are registered as protocols and documented through `<protocol>://help`.

Keep these product invariants intact:

- The model sees exactly two tool definitions: `read` and `exec`.
- Split addresses only at the first `://`. Treat the remainder as an opaque target; do not introduce RFC URL parsing, decoding, or normalization.
- Accept any JSON value as `body` and pass it to the selected protocol unchanged.
- Protocol names are unique. A protocol may implement read, exec, or both.
- Exec protocols may use system-managed asynchronous tasks. URI options such as the shell protocols' `?wait=N` belong to each protocol: the registry must pass them through unchanged and never apply a generic wait. The task manager exposes URI-independent waiting; a timeout must leave the task running.
- Preserve oversized output in the session output directory and return a readable `file://` address.
- Discover Skills once at startup from the documented project and user roots. Give every discovered Skill its own `<normalized-name>-skill` protocol. Never compile or copy one developer environment's discovered Skill list into the product.
- Persist only each Skill's name, description, and canonical `SKILL.md` path in the session context. Freeze the complete generated system prompt when the session is created and reuse it unchanged on resume. Skill help and resources read from the frozen path at call time; a missing file must fail explicitly, and a same-named Skill at another path must never rebind the old session.
- Keep session persistence append-only. Context compaction writes a SQLite checkpoint and changes model replay without deleting original events. Cut only at complete user-turn boundaries so tool calls remain paired with their results.
- Register `bash` and `pwsh` only when an executable is present in the environment.
- Treat the canonical launch directory as the project boundary. Session resume and explicit IDs must not cross that boundary; do not add a cross-project session or directory overview unless the product direction changes.
- Keep Browse, Insert, and Detail as distinct TUI contexts. Conversation rows are previews; complete reasoning, tool calls/results, and messages remain individually inspectable.
- Treat Insert as draft editing, not submission: `Enter` inserts a newline and `Esc` returns to Browse without losing the draft. In Browse, `Enter` submits a non-empty draft and otherwise opens the selected event.
- Keep arrows and mouse as first-class navigation. `j/k` may remain aliases, but defaults and interface hints must not require Vim knowledge. Browse mode owns the discoverable Space command panel and `:` command line.
- Keep external editor and finder commands available as embedded PTY floats and as configurable fullscreen handoffs. Mouse selection and OSC52 copy must remain available without stealing ordinary clicks from interactive programs.
- Resolve keyboard behavior through the layered Rhai keymap. Do not reintroduce modeless hard-coded shortcuts for configurable actions.
- Route command-palette, colon-command, and key-bindable command IDs through `CommandRegistry`. Keep plugin panel rendering generic in the TUI; plugin-specific behavior belongs in registered providers.

## Code Map

- `src/main.rs`: application assembly and protocol registration.
- `src/catalog.rs`: pi.dev model catalog, cache, and `models.json` overlays.
- `src/config.rs`: layered text configuration, credentials, CLI, and environment overrides.
- `src/keymap.rs`: modern modal defaults and global/project Rhai overlays.
- `src/model.rs`: Rig provider adapter and the two model-facing tool schemas.
- `src/runtime.rs`: model/tool loop, automatic/manual compaction, and tool-call correlation.
- `src/compaction.rs`: context estimation, complete-turn checkpoint boundaries, and summary handoff construction.
- `src/plugin.rs`: protocol/command/TUI plugin registration and generic panel providers.
- `src/protocol.rs`: protocol contract, registry, dispatch, and output presentation.
- `src/builtins/`: file, edit, Bash, and PowerShell protocols.
- `src/task.rs`: asynchronous task lifecycle, waiting, cancellation, and notices.
- `src/output.rs`: output limits and complete-output persistence.
- `src/skill.rs`: Skill discovery, metadata parsing, and resource containment.
- `src/session.rs`: SQLite event persistence, frozen session context, compaction checkpoints, and replay.
- `src/prompts.rs`: model-facing system, tool, and protocol help text.
- `src/terminal.rs`: embedded PTY lifecycle, terminal emulation, resize, and input encoding.
- `src/tui.rs`: Browse/Insert modes, event details, editor/finder integration, text selection, overlays, and rendering.

Put behavior in the module that owns it. Avoid wrappers or helpers used by only one call site unless they enforce a named invariant.

## Development

Use stable Rust. Before completing a code change, run:

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo check
```

Add focused tests beside changed behavior. Shared protocol, task, session, or model-loop changes require tests for both the normal path and the affected boundary condition. Keep the fake backend tests provider-independent; live API keys are not required for the test suite.

## Change Guidelines

- Prefer extending `Protocol` over adding another model-facing tool.
- Register extension commands and TUI panels through `PluginHost`; do not add extension-specific branches to `src/tui.rs`.
- Keep protocol help in `src/prompts.rs` synchronized with behavior.
- Keep the provider layer thin and preserve provider tool-call identity during replay.
- Do not treat task acceptance as completion. Surface terminal status and content through the protocol's read route.
- Shell cancellation must terminate child processes, not only the parent future.
- File edits must remain atomic and exact replacements must reject missing or ambiguous matches.
- A resumed session without its frozen context is invalid. Do not silently synthesize a prompt from the current environment.
- Preserve terminal restoration on every TUI exit and error path.
- Keep the interface keyboard-complete and preserve mouse hit regions for selectable lists and command panels.
- Embedded PTYs must terminate their child and reader thread when closed. For fullscreen `fzf` or editor handoffs, restore the terminal before launch and recreate the event stream after returning.
- Do not add credentials, generated sessions, complete-output files, or `target/` artifacts to Git.

## Documentation

Write model-facing help as operational documentation: state the valid URI, accepted body shapes, async behavior, result address, limits, and one concrete example. Keep the initial system prompt short; detailed instructions belong behind `://help`.

Keep the English `README.md` and Chinese `README.zh-CN.md` equivalent. Update both when CLI flags, provider defaults, key bindings, scan locations, persistence paths, or public protocol behavior changes.
