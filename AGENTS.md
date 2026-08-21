# Repository Guide

## Purpose

URI Agent is a Rust coding agent whose entire model-facing tool surface is `read(uri, body?)` and `exec(uri, body?)`. Capabilities are registered as protocols and documented through `<protocol>://help`.

Keep these product invariants intact:

- The model sees exactly two tool definitions: `read` and `exec`.
- Split addresses only at the first `://`. Treat the remainder as an opaque target; do not introduce RFC URL parsing, decoding, or normalization.
- Accept any JSON value as `body` and pass it to the selected protocol unchanged.
- Protocol names are unique. A protocol may implement read, exec, or both.
- Exec is asynchronous by default. `?wait=N` is an explicit bounded wait of at most 300 seconds; a timeout must leave the task running.
- Preserve oversized output in the session output directory and return a readable `file://` address.
- Give every discovered Skill its own `<normalized-name>-skill` protocol. `://help` returns its `SKILL.md` plus the real Skill directory; resource routes must not escape that directory.
- Register `bash` and `pwsh` only when an executable is present in the environment.

## Code Map

- `src/main.rs`: application assembly and protocol registration.
- `src/config.rs`: CLI options, provider selection, and defaults.
- `src/model.rs`: Rig provider adapter and the two model-facing tool schemas.
- `src/runtime.rs`: model/tool loop and tool-call correlation.
- `src/protocol.rs`: protocol contract, registry, dispatch, and output presentation.
- `src/builtins/`: file, edit, Bash, and PowerShell protocols.
- `src/task.rs`: asynchronous task lifecycle, waiting, cancellation, and notices.
- `src/output.rs`: output limits and complete-output persistence.
- `src/skill.rs`: Skill discovery, metadata parsing, and resource containment.
- `src/session.rs`: append-only JSONL persistence and replay.
- `src/prompts.rs`: model-facing system, tool, and protocol help text.
- `src/tui.rs`: Ratatui state, input handling, overlays, and rendering.

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
- Keep protocol help in `src/prompts.rs` synchronized with behavior.
- Keep the provider layer thin and preserve provider tool-call identity during replay.
- Do not treat task acceptance as completion. Surface terminal status and content through the protocol's read route.
- Shell cancellation must terminate child processes, not only the parent future.
- File edits must remain atomic and exact replacements must reject missing or ambiguous matches.
- Preserve terminal restoration on every TUI exit and error path.
- Keep the interface keyboard-complete; mouse support is supplementary.
- Do not add credentials, generated sessions, complete-output files, or `target/` artifacts to Git.

## Documentation

Write model-facing help as operational documentation: state the valid URI, accepted body shapes, async behavior, result address, limits, and one concrete example. Keep the initial system prompt short; detailed instructions belong behind `://help`.

Keep the English `README.md` and Chinese `README.zh-CN.md` equivalent. Update both when CLI flags, provider defaults, key bindings, scan locations, persistence paths, or public protocol behavior changes.
