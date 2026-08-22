# URI Agent

[English](README.md) | [简体中文](README.zh-CN.md)

[![License: MIT](https://img.shields.io/badge/license-MIT-6ed2c2.svg)](LICENSE)

URI Agent is a terminal coding agent built around a fixed, minimal model interface. The model always sees two tools—`read` and `exec`—and reaches files, shells, edits, Skills, and extensions through protocol addresses such as `file://...` and `bash://...`.

Protocols load their operational guidance on demand from `<protocol>://help`. Long-running operations become managed tasks, oversized output remains available through a `file://` address, and session history is stored in SQLite.

> [!WARNING]
> URI Agent is not a sandbox. File and shell protocols run with the permissions of the `uri-agent` process. Use it only with projects and configuration you trust.

## Why URI Agent

- **Stable tool surface:** adding a capability does not add another model-facing tool schema.
- **Progressive context:** protocol and Skill instructions enter the context only when the model reads their help.
- **Observable execution:** asynchronous work exposes status and final output through protocol read routes.
- **Durable conversations:** drafts, events, frozen session context, and compaction checkpoints survive restarts.
- **Keyboard-complete TUI:** the conversation, composer, commands, model selection, settings, and terminal stay in one interface.

The model-facing interface remains:

```text
read(uri: string, body?: any)
exec(uri: string, body?: any)
```

## Quick start

### Requirements

- a stable Rust toolchain and Git;
- credentials for a supported model provider;
- a terminal with standard keyboard input. Mouse support is optional.

### Install from source

```bash
git clone https://github.com/4fuu/uri-agent.git
cd uri-agent
cargo install --path .
```

### Start your first session

Launch URI Agent with the project directory it may access:

```bash
uri-agent --cwd /path/to/project
```

URI Agent does not choose a default model. In the TUI:

1. Run `:login` to save an API key or complete a supported OAuth flow.
2. Run `:model` and select a runnable model.
3. Press `i`, enter a request, and press `Enter` to send it.

When setup is complete, the welcome view shows the selected provider, model, and thinking effort instead of the setup prompt; submitted requests and responses then appear in the conversation.

`Shift+Enter` inserts a newline, `Esc` closes the composer while preserving its draft, `:` opens the searchable command panel, and `F1` shows the active keymap and command reference. Command aliases participate in search without cluttering the default list. Search text only filters commands; commands that need a value open a selector or a separate input float.

## How protocols look

```text
read("file://src/main.rs?offset=1&limit=200")
read("code-review-skill://help")
exec("bash://?wait=30", "cargo test")
```

URI Agent splits an address only at the first `://`; the selected protocol owns the remaining target. The registry passes the optional JSON `body` through unchanged. See [Protocols, tasks, Skills, and extensions](docs/protocols.md) for the complete design and built-in protocol inventory.

## Documentation

The [`docs/` index](docs/README.md) organizes detailed documentation by task:

- [Protocols, tasks, Skills, and extensions](docs/protocols.md) — model-facing contracts, built-ins, asynchronous execution, output preservation, Skill discovery, and plugin registration.
- [Models and configuration](docs/configuration.md) — catalog behavior, authentication, settings precedence, CLI flags, thinking levels, and custom providers.
- [Terminal interface and sessions](docs/interface.md) — composer and commands, keymaps, embedded terminal, image attachments, persistence, resume, and compaction.
- [Architecture and development](docs/development.md) — module ownership, non-negotiable invariants, change rules, and verification.

At runtime, `<protocol>://help` is the authoritative reference for a protocol's accepted URIs and body shape.

## Development

The project currently builds on stable Rust. Before submitting a code change, run:

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo check
```

Read [`AGENTS.md`](AGENTS.md) before changing the repository and use the [development guide](docs/development.md) for detailed ownership and testing rules.

## License

[MIT](LICENSE) © 2026 4fuu
