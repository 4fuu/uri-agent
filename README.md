<p align="center">
  <img src="docs/assets/logo.svg" width="420" alt="URI Agent logo">
</p>

# URI Agent

[English](README.md) | [简体中文](README.zh-CN.md)

[![License: MIT](https://img.shields.io/badge/license-MIT-6ed2c2.svg)](LICENSE)

URI Agent is a terminal coding agent built to push progressive disclosure and on-demand context loading as far as possible. A new session begins with a small router: exactly two generic tools—`read` and `exec`—plus a one-line index of available protocols. Detailed instructions for files, shells, edits, web access, Skills, and extensions are not preloaded.

Before using a capability, the model reads its contract from `<protocol>://help`; only that protocol's guidance then enters the conversation. Adding a capability adds a protocol entry instead of another model-facing tool schema or manual. Long-running operations become managed tasks, oversized output remains available through a `file://` address, and session history is stored in SQLite.

> [!WARNING]
> URI Agent is not a sandbox. File and shell protocols run with the permissions of the `uri-agent` process. Use it only with projects and configuration you trust.

URI Agent is an early release and may change between dated versions. Model requests and the context they need are sent to the provider you select. Unless offline mode is enabled, URI Agent also fetches model catalog metadata from pi.dev.

## Progressive context, measured

With the current source, the fixed startup baseline on Unix with Bash and eight built-in protocols is:

| Component | Included content | UTF-8 size |
| --- | --- | ---: |
| System prompt | Routing rules and the built-in protocol index | 1,159 bytes (1.159 KB) |
| `read` + `exec` definitions | Both compact internal tool schemas | 1,326 bytes (1.326 KB) |
| **Total** | Fixed system prompt and tools | **2,485 bytes (2.485 KB)** |

```text
~2.5 KB fixed baseline
    → read("<protocol>://help")
    → that protocol's contract
    → task-specific reads and executions
```

Skills follow the same path: startup adds only each discovered Skill's name and description; its `SKILL.md` and bundled resources load when the model selects that Skill. Actual startup context also adds the project's `AGENTS.md`, when present, and a short detected-binary hint. The table isolates URI Agent's fixed baseline, serializes its internal tool definitions as compact JSON, and excludes provider-specific request wrappers.

## Why URI Agent

- **Context on demand:** protocol manuals, Skill instructions, and resources load only when the current task needs them.
- **Stable tool surface:** adding a capability does not add another model-facing tool schema.
- **Built-in web access:** search and extract HTTPS pages through a logged-in Parallel or Exa account, with local page conversion when neither is configured.
- **Observable execution:** asynchronous work exposes status and final output through protocol read routes.
- **Portable trusted extensions:** Extism WASM modules can add hot-reloadable protocols without changing the tool surface.
- **Durable conversations:** drafts, events, frozen session context, and compaction checkpoints survive restarts.
- **Live course correction:** while a turn runs, queue a follow-up for afterward or guide the next model request; undelivered messages remain editable.
- **Reusable Agent environment:** save tokens once for future Agent shell commands while keeping values out of project and session files.
- **Keyboard-complete TUI:** the conversation, composer, commands, model selection, settings, and terminal stay in one interface, with `@` file and `@@` session references.

The model-facing interface remains:

```text
read(uri: string, body?: any)
exec(uri: string, body?: any)
```

## Quick start

### Requirements

- credentials for a supported model provider;
- a terminal with standard keyboard input. Mouse support is optional.

### Install

With Homebrew on macOS:

```bash
brew tap 4fuu/uri-agent https://github.com/4fuu/uri-agent
brew install 4fuu/uri-agent/uri-agent
```

With Scoop on 64-bit Windows:

```powershell
scoop bucket add uri-agent https://github.com/4fuu/uri-agent
scoop install uri-agent
```

On x86-64 or ARM64 Linux, the installer verifies the release checksum and writes to `~/.local/bin`:

```bash
curl --proto '=https' --tlsv1.2 -LsSf https://raw.githubusercontent.com/4fuu/uri-agent/main/scripts/install.sh | sh
```

To build from crates.io, install a stable Rust toolchain and run:

```bash
cargo install --locked uri-agent
```

To build the current repository instead:

```bash
git clone https://github.com/4fuu/uri-agent.git
cd uri-agent
cargo install --locked --path .
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
read("sessions://search", {"query":"refresh token"})
read("https://search?limit=10", "stable Rust release notes")
read("https://www.rust-lang.org/")
read("uri-agent-docs://README.md")
read("code-review-skill://help")
exec("bash://?wait=30", "cargo test")  # Unix-like systems
exec("pwsh://?wait=30", "cargo test")  # Windows
```

URI Agent selects a protocol at the first `://`; that protocol owns the remaining target and optional JSON body. After `:login` saves a Parallel or Exa API key, the built-in `https` protocol uses that provider for search and page extraction. Without either web-provider login, search asks for login while page reads fall back to local HTTPS fetching and HTML-to-Markdown conversion. `uri-agent-docs` keeps version-matched documentation available from any startup working directory. Available shell protocols depend on the platform. Read `<protocol>://help` for the exact runtime contract, or see [Protocols, tasks, and output](docs/protocols.md) for the shared design.

## Extensions

Trusted WASM modules can add runtime-loadable protocols. WASM is a portable ABI here, not a security boundary: enabled plugins receive filesystem, HTTP, WASI, and built-in protocol access with the same user authority as URI Agent. Direct access to saved Agent environment values or provider API keys requires an explicit capability request in plugin source; it is an audit marker, not an approval flow. See [WASM plugins](docs/plugins.md) for installation, reload behavior, the ABI, SDK usage, and reliability limits.

## Documentation

The [`docs/` index](docs/README.md) routes readers to focused guides for protocols, startup context and Skills, WASM plugins, models and configuration, the terminal interface, sessions and compaction, development, and releases.

At runtime, `<protocol>://help` is the authoritative reference for a protocol's accepted URIs and body shape.

## Development

The project builds on stable Rust. Read [`AGENTS.md`](AGENTS.md) before changing the repository; the [development guide](docs/development.md) defines module ownership, change rules, and required verification.

## License

[MIT](LICENSE) © 2026 4fuu
