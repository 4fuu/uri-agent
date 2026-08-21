# AGENTS.md

Guidance for coding agents working in this repository.

## Project

URI Agent is a Rust terminal coding agent. Its defining constraint is a fixed model-facing interface:

```text
read(uri, body?)
exec(uri, body?)
```

Capabilities are registered as protocols and document their operational contract at `<protocol>://help`. Preserve this design instead of adding model-facing tools or embedding every capability in the initial system prompt.

## Non-negotiable product contracts

- The model sees exactly two tool definitions: `read` and `exec`.
- Split protocol addresses only at the first `://`. Treat the remainder as opaque; do not apply RFC URL parsing, decoding, or normalization.
- Accept any JSON value as `body` and pass it to the selected protocol unchanged.
- Protocol names are unique. A protocol may implement `read`, `exec`, or both.
- Asynchronous task acceptance is not completion. Surface status and final content through the protocol's read route.
- URI options belong to their protocol. In particular, `bash` and `pwsh` own `?wait=N`; the registry must never interpret it as a generic option.
- A task wait timeout leaves the task running.
- Preserve oversized output in the session output directory and return a readable `file://` address.
- Register `bash` and `pwsh` only when the corresponding executable exists.
- Discover Skills once at startup from the documented project and user roots. Never compile or copy one machine's discovered Skill list into the product.
- Give each accepted Skill a normalized `<name>-skill` protocol. Keep first-wins precedence and skip collisions with registered protocol names with a clear notice.
- Freeze the complete generated system prompt and each selected Skill's name, description, and canonical `SKILL.md` path when a session is created. Resume from that snapshot; never rebind a same-named Skill at another path.
- A missing frozen Skill file fails explicitly when read. A resumed session without frozen context is invalid.
- Session events are append-only. Compaction adds a SQLite checkpoint and changes model replay without deleting original events.
- Compaction boundaries must be complete user turns; never separate a tool call from its result.
- The canonical launch directory is the project boundary. Latest and explicit session resume must not cross it.
- The TUI is a single conversation surface. There are no Browse/Insert/Detail modes and no persistent status bar.
- Startup may show the splash animation, then replace it with the conversation view. An empty conversation keeps the animated brand view; once records exist there is no header, and the bottom shows a pi-style footer (`cwd (branch)`, usage/cost/context meter, model) with hints on the last line.
- `i` opens a floating composer: `Enter` sends, `Shift+Enter` inserts a newline, and `Esc` keeps the draft in SQLite.
- `:` opens the colon command panel anywhere except inside an open float. There are no slash commands.
- Commands that need a choice or extra text open a floating selector or input. Do not add a second command syntax.
- History rows stay folded until opened. Click or `Enter` expands a row, including reasoning. `r`, `t`, and `h` jump among reasoning, tool, and user rows.
- Arrow keys and mouse input are first-class. `j`/`k` may be aliases, but defaults and help must not require Vim knowledge.
- Route configurable keys through the layered Rhai keymap. Do not add modeless hard-coded shortcuts for configurable actions.
- Route palette entries, colon commands, and key-bindable command IDs through `CommandRegistry`.
- Keep plugin-specific behavior in registered protocols, commands, or panel providers. TUI panel rendering stays generic.
- Preserve terminal restoration, mouse selection, and OSC52 copy on every exit and error path.

## Repository map

- `src/main.rs` — application assembly and protocol registration.
- `src/catalog.rs` — pi.dev model catalog, cache, and `models.json` overlays.
- `src/config.rs` — CLI, layered settings, credentials, and environment overrides.
- `src/model.rs` — Rig provider adapters and the two model-facing tool schemas.
- `src/prompts.rs` — initial system prompt and built-in protocol help.
- `src/protocol.rs` — protocol trait, registry, dispatch, and output presentation.
- `src/builtins/` — built-in plugins for file reads, exact replacement, Codex-style patching, Bash, and PowerShell.
- `src/task.rs` — task lifecycle, waiting, cancellation, and notices.
- `src/output.rs` — inline output limits and complete-output persistence.
- `src/skill.rs` — Skill discovery, metadata parsing, naming, and resource containment.
- `src/session.rs` — SQLite persistence, frozen context, checkpoints, and replay.
- `src/compaction.rs` — context estimation and complete-turn checkpoint construction.
- `src/runtime.rs` — model/tool loop, tool-call correlation, and compaction triggers.
- `src/plugin.rs` — plugin declarations plus protocol, command, and generic TUI panel registration.
- `src/keymap.rs` — default Rhai mappings and global/project overlays.
- `src/terminal.rs` — embedded PTY lifecycle, emulation, resize, and input encoding.
- `src/oauth.rs` — Anthropic OAuth login, callback server, and token refresh.
- `src/tui.rs` — splash, conversation surface, composer/command floats, selectors, and rendering.

Put behavior in the module that owns it. Before adding a wrapper, helper, or type, check whether changing the existing source of truth is clearer. Avoid one-use abstractions unless they enforce a named invariant.

## Change rules

### Protocols, tasks, and output

- Prefer extending `Protocol` over introducing another model-facing concept.
- Keep a protocol's help in `src/prompts.rs` synchronized with its behavior. State valid URIs, accepted body shapes, async behavior, result routes, limits, and one example.
- Keep file writes atomic. Exact replacement must reject both missing and ambiguous matches.
- Shell cancellation must terminate child processes, not only the parent future.
- Do not add a generic registry wait. Protocols may use the URI-independent task manager to expose their own bounded wait syntax.

### Models, Skills, and sessions

- Keep provider adapters thin and preserve provider tool-call identity during replay.
- Keep fake-backend tests provider-independent; tests must not require live API keys.
- Contain Skill resource reads within the frozen Skill directory, including through symlinks.
- Do not synthesize current prompt or Skill state when resuming a session.
- Do not delete old events during compaction or split paired tool events at a checkpoint.

### TUI and extensions

- Keep the interface keyboard-complete and preserve mouse hit regions for selectable lists and command panels.
- Register extension commands and panels through `PluginHost`; do not add extension-specific branches to `src/tui.rs`.
- Direct drag selection in read-only views; Shift-drag when a float needs to keep ordinary clicks.

### Scope and generated data

- Make the smallest change that fully implements the requested behavior.
- Keep unrelated refactors and formatting out of the patch.
- Do not commit credentials, generated sessions, complete-output files, `.uri-agent/`, `.amp/`, or `target/` artifacts.

## Verification

Use stable Rust. Add focused tests beside changed behavior. Shared protocol, task, session, compaction, or model-loop changes need both a normal-path test and the affected boundary-condition test. TUI changes should cover the affected surface and input path.

Before completing a code change, run:

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo check
```

For documentation-only changes, verify links, examples, CLI names, defaults, and English/Chinese parity; the full Rust suite is unnecessary unless documentation generation or code was changed.

## Documentation

- Keep `README.md` and `README.zh-CN.md` equivalent.
- Update both when public behavior changes, including CLI flags, provider defaults, key bindings, Skill scan roots, persistence paths, configuration, or protocol behavior.
- Keep the README focused on adoption and first success. Put detailed model-facing operations behind `<protocol>://help`.
- Use `URI Agent` in prose and `uri-agent` for the binary, crate, commands, and filesystem names.
