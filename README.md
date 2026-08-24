<p align="center">
  <img src="docs/assets/logo.svg" width="420" alt="URI Agent logo">
</p>

# URI Agent

[English](README.md) | [简体中文](README.zh-CN.md)

[![License: MIT](https://img.shields.io/badge/license-MIT-6ed2c2.svg)](LICENSE)

URI Agent is a terminal coding agent where every model-facing capability for reading resources or performing actions is exposed as a URI protocol. Files, directories, edits, shells, web pages, session archives, documentation, Skills, and extensions share one address space behind exactly two tools:

```text
read(uri: string, body?: any)
exec(uri: string, body?: any)
```

URI Agent extends to every capability the same loading pattern other agents use for Skills: expose a compact name and description first, then load full instructions and resources only when selected. A new session therefore preloads only routing rules and one-line protocol descriptors; detailed contracts remain at `<protocol>://help`, Skill bodies and documentation remain behind their protocols, and oversized results remain behind `file://` addresses. This preserves aggressive on-demand and progressive context loading while the fixed `read` / `exec` contract keeps tool calls reliable and registered protocols provide extreme flexibility. The fixed startup baseline remains around 2.5 KB instead of paying the context cost of every capability up front.

Adding a capability adds a protocol entry rather than another model-facing tool schema or preloaded manual. Long-running operations become managed tasks, and append-only session history is stored in SQLite.

> [!WARNING]
> URI Agent is not a sandbox. File and shell protocols run with the permissions of the `uri-agent` process. Use it only with projects and configuration you trust.

URI Agent is an early release and may change between dated versions. Model requests and the context they need are sent to the provider you select. Unless offline mode is enabled, URI Agent also fetches model catalog metadata from pi.dev.

## A ~2.5 KB fixed startup baseline

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

- **URI-native progressive context:** one address space covers every resource and action, while Skills-style loading keeps contracts, instructions, resources, and complete output out of context until needed.
- **Reliable and extensible:** the fixed `read` / `exec` contract stays stable while built-in, Skill, linked Rust, and trusted WASM protocols evolve independently.
- **pi.dev models and sign-in:** URI Agent uses pi.dev's cloud model catalog and provider login methods, exposing every catalog model whose API family its backend supports through one selector.
- **Durable, observable work:** managed tasks expose status and final output; append-only sessions, drafts, frozen context, and compaction checkpoints survive restarts.
- **One controllable terminal workflow:** built-in web access, live Queue and Guidance, keyboard-complete controls, and `@` file or `@@` session references stay in one interface.

## pi.dev model coverage

URI Agent is compatible with the model configurations distributed through pi.dev. As of 2026-08-24, its implemented API families cover:

| Catalog measure | Supported |
| --- | ---: |
| API families | 5 of 9 |
| Model entries | 1,107 of 1,307 (84.7%) |
| Provider IDs | 35 of 39 |

Catalog contents and account entitlements change; a listed model still requires the matching credential, region, and subscription. See [Models and configuration](docs/configuration.md#model-catalog) for the exact API families and authentication requirements.

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

## Extensions

Trusted WASM modules can add runtime-loadable protocols. WASM is a portable ABI here, not a security boundary: enabled plugins receive filesystem, HTTP, WASI, and built-in protocol access with the same user authority as URI Agent. Direct access to saved Agent environment values or provider API keys requires an explicit capability request in plugin source; it is an audit marker, not an approval flow. See [WASM plugins](docs/plugins.md) for installation, reload behavior, the ABI, SDK usage, and reliability limits.

## Documentation

The [`docs/` index](docs/README.md) routes readers to focused guides for protocols, startup context and Skills, WASM plugins, models and configuration, the terminal interface, sessions and compaction, development, and releases.

At runtime, `<protocol>://help` is the authoritative reference for a protocol's accepted URIs and body shape.

## Development

The project builds on stable Rust. Read [`AGENTS.md`](AGENTS.md) before changing the repository; the [development guide](docs/development.md) defines module ownership, change rules, and required verification.

## License

[MIT](LICENSE) © 2026 4fuu
