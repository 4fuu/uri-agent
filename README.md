# URI Agent

[English](README.md) | [简体中文](README.zh-CN.md)

[![License: MIT](https://img.shields.io/badge/license-MIT-6ed2c2.svg)](LICENSE)

URI Agent is a terminal coding agent built around a fixed, minimal model interface. The model always sees two tools—`read` and `exec`—and reaches files, shells, edits, Skills, and extensions through protocol addresses such as `file://...` and `bash://...`.

Protocols load their operational guidance on demand from `<protocol>://help`. Long-running operations become managed tasks, oversized output remains available through a `file://` address, and session history is stored in SQLite.

> [!WARNING]
> URI Agent is not a sandbox. File and shell protocols run with the permissions of the `uri-agent` process. Use it only with projects and configuration you trust.

URI Agent is currently a pre-1.0 project installed from source. Model requests and the context they need are sent to the provider you select. Unless offline mode is enabled, URI Agent also fetches model catalog metadata from pi.dev.

## Why URI Agent

- **Stable tool surface:** adding a capability does not add another model-facing tool schema.
- **Progressive context:** protocol and Skill instructions enter the context only when the model reads their help.
- **Built-in web access:** read HTTPS pages as Markdown and search through a logged-in Parallel or Exa account.
- **Observable execution:** asynchronous work exposes status and final output through protocol read routes.
- **Portable trusted extensions:** Extism WASM modules can add hot-reloadable protocols without changing the tool surface.
- **Durable conversations:** drafts, events, frozen session context, and compaction checkpoints survive restarts.
- **Live course correction:** while a turn runs, queue a follow-up for afterward or guide the next model request; undelivered messages remain editable.
- **Reusable Agent environment:** save tokens once for future Agent shell commands while keeping values out of project and session files.
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

Launch URI Agent with the project directory it should use:

```bash
uri-agent --cwd /path/to/project
```

`--cwd` sets the project and default working directory; it is not a filesystem access boundary. If the project contains `AGENTS.md`, review it before launch: new sessions include those instructions and freeze their startup context.

URI Agent does not choose a default model. In the TUI:

1. Run `:login` to save an API key or complete a supported OAuth flow.
2. Run `:model` and select a runnable model.
3. Press `Space`, enter a small read-only request such as `Read the top-level files and explain what this project does. Do not modify files.`, and press `Enter`.

The first session is working when protocol activity appears and the assistant returns an answer based on the project. Press `F1` or run `:help` for the active commands and key bindings.

Run `:set-env` to save a variable such as `NPM_TOKEN`; future Agent shell commands receive it automatically. Settings lists variable names without showing their values. See [Agent environment](docs/configuration.md#agent-environment) for its global scope, private file storage, `:terminal` separation, and plugin access.

See [Models and configuration](docs/configuration.md) for supported API families, authentication, offline mode, and custom endpoints.

## How protocols look

```text
read("file://src/main.rs?offset=1&limit=200")
read("https://search?limit=10", "stable Rust release notes")
read("https://www.rust-lang.org/")
read("uri-agent-docs://README.md")
read("code-review-skill://help")
exec("bash://?wait=30", "cargo test")  # Unix-like systems
exec("pwsh://?wait=30", "cargo test")  # Windows
```

URI Agent selects a protocol at the first `://`; that protocol owns the remaining target and optional JSON body. The built-in `https` protocol reads HTTPS pages without a login and searches after `:login` saves a Parallel or Exa API key. `uri-agent-docs` keeps version-matched documentation available from any startup working directory. Available shell protocols depend on the platform. Read `<protocol>://help` for the exact runtime contract, or see [Protocols, tasks, output, and Skills](docs/protocols.md) for the shared design.

## Extensions

Trusted WASM modules can add runtime-loadable protocols. WASM is a portable ABI here, not a security boundary: enabled plugins receive filesystem, HTTP, WASI, and built-in protocol access with the same user authority as URI Agent. Direct access to saved Agent environment values or provider API keys requires an explicit capability request in plugin source; it is an audit marker, not an approval flow. See [WASM plugins](docs/plugins.md) for installation, reload behavior, the ABI, SDK usage, and reliability limits.

## Documentation

The [`docs/` index](docs/README.md) organizes detailed documentation by task:

- [Protocols, tasks, output, and Skills](docs/protocols.md) — model-facing routing, built-ins, execution semantics, managed tasks, output preservation, and Skill discovery.
- [WASM plugins](docs/plugins.md) — installation, reload, trust boundaries, ABI, SDK, and runtime limits.
- [Models and configuration](docs/configuration.md) — catalog behavior, authentication, settings precedence, CLI flags, thinking levels, and custom providers.
- [Terminal interface and sessions](docs/interface.md) — composer and commands, keymaps, embedded terminal, image attachments, persistence, resume, and compaction.
- [Architecture and development](docs/development.md) — module ownership, non-negotiable invariants, change rules, and verification.

At runtime, `<protocol>://help` is the authoritative reference for a protocol's accepted URIs and body shape.

## Development

The project builds on stable Rust. Read [`AGENTS.md`](AGENTS.md) before changing the repository; the [development guide](docs/development.md) defines module ownership, change rules, and required verification.

## License

[MIT](LICENSE) © 2026 4fuu
