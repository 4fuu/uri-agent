<p align="center">
  <img src="docs/assets/logo.svg" width="420" alt="URI Agent logo">
</p>

# URI Agent

[English](README.md) | [简体中文](README.zh-CN.md)

[![License: MIT](https://img.shields.io/badge/license-MIT-6ed2c2.svg)](LICENSE)

URI Agent is a terminal coding agent built around URI protocols with typed
direct tools for operations whose arguments are complex or escape-heavy. The
linked built-in plugins register four tools, and trusted WASM plugins may add
more typed tools at runtime:

```text
read(uri: string, body: string)
exec(uri: string, body: string)
replace(path: string, old_text: string, new_text: string)
apply_patch(patch: string)
```

The `read` and `exec` body is always required and always a string. Use `""`
when a protocol takes no body, plain text for textual input, and complete
serialized JSON text for structured protocol input.

URI Agent extends to most capabilities the same loading pattern other agents use
for Skills: expose a compact name and description first, then load full
instructions and resources only when selected. A new session therefore
preloads routing rules, one-line protocol descriptors, and active tool schemas;
detailed protocol contracts remain at `<protocol>://help`, Skill bodies and
documentation remain behind their protocols, and oversized results remain
behind `file://` addresses. This preserves aggressive on-demand and progressive
context loading while direct edit tools avoid JSON-in-string escaping.

Simple string-input capabilities add a protocol entry rather than a preloaded
manual. Typed or escape-heavy capabilities can register a direct tool through
the same plugin system. Shell commands return directly when short,
automatically become managed background tasks when long, and notify the model
on completion without polling. Append-only session history is stored in
SQLite.

For image-capable models, the built-in `file` protocol returns PNG, JPEG, GIF,
and WebP files directly to the model when they are read.

> [!WARNING]
> URI Agent is not a sandbox. File and shell protocols run with the permissions of the `uri-agent` process. Use it only with projects and configuration you trust.

URI Agent is an early release and may change between dated versions. Model requests and the context they need are sent to the provider you select. Unless offline mode is enabled, URI Agent also fetches model catalog metadata from pi.dev and, when credentials are configured, supported providers' model-list APIs.

## Progressive startup context

```text
compact routing rules + protocol index + active tool schemas
    → read("<protocol>://help", "")
    → that protocol's contract
    → task-specific reads and executions
```

Skills follow the same path: startup adds only each discovered Skill's name and
description; its `SKILL.md` and bundled resources load when the model selects
that Skill. Actual startup context also adds the project's `AGENTS.md` when
present. Direct tools contribute their typed schemas but do not preload a
separate manual.

## Why URI Agent

- **URI-native progressive context:** one address space covers every resource and action, while Skills-style loading keeps contracts, instructions, resources, and complete output out of context until needed.
- **Reliable and extensible:** the stable string-based `read` / `exec` contract handles simple protocols, while typed direct tools avoid nested escaping and both paths remain plugin-registered.
- **Current models and sign-in:** URI Agent combines pi.dev's cloud catalog and provider login methods with credential-scoped live discovery, exposing runnable new provider models before the shared catalog catches up.
- **Durable, observable work:** managed tasks expose status and final output, automatically notify the model on completion, and restore settled reports with their session; append-only sessions, drafts, frozen context, and compaction checkpoints survive restarts.
- **One controllable terminal workflow:** built-in web access, live Queue and Guidance, keyboard-complete controls, and `@` file or `@@` session references stay in one interface.

## pi.dev model coverage

URI Agent is compatible with the model configurations distributed through pi.dev. As of 2026-08-26, its implemented API families and provider discovery cover:

| Catalog measure | Supported |
| --- | ---: |
| API families | 5 of 9 |
| Model entries | 1,107 of 1,307 (84.7%) |
| Provider IDs | 35 of 39 |
| Provider IDs with live discovery | 28 of 35 runnable |

Live provider results are cached per credential and supplement rather than replace pi.dev metadata. Catalog contents and account entitlements change; a listed model still requires the matching credential, region, and subscription. See [Models and configuration](docs/configuration.md#model-catalog) for the exact discovery coverage, API families, and authentication requirements.

## Quick start

### Requirements

- credentials for a supported model provider;
- a terminal with standard keyboard input. Mouse support is optional.

### Install

With Homebrew on Apple Silicon macOS:

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

Trusted WASM modules can add runtime-loadable protocols and typed direct tools.
WASM is a portable ABI here, not a security boundary: enabled plugins receive
filesystem, HTTP, WASI, and built-in protocol access with the same user
authority as URI Agent. Direct access to saved Agent environment values or
provider API keys requires an explicit capability request in plugin source; it
is an audit marker, not an approval flow. See [WASM
plugins](docs/plugins.md) for installation, reload behavior, the ABI, SDK usage,
and reliability limits.

## Documentation

The [`docs/` index](docs/README.md) routes readers to focused guides for protocols, startup context and Skills, WASM plugins, models and configuration, the terminal interface, sessions and compaction, development, and releases.

At runtime, `<protocol>://help` is the authoritative reference for a protocol's accepted URIs and body shape.

## Development

The project builds on stable Rust. Read [`AGENTS.md`](AGENTS.md) before changing the repository; the [development guide](docs/development.md) defines module ownership, change rules, and required verification.

## License

[MIT](LICENSE) © 2026 4fuu
