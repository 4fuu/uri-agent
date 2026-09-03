<p align="center">
  <img src="docs/assets/logo.svg" width="420" alt="URI Agent logo">
</p>

# URI Agent

[English](README.md) | [简体中文](README.zh-CN.md)

[![License: MIT](https://img.shields.io/badge/license-MIT-6ed2c2.svg)](LICENSE)

URI Agent is a protocol-oriented terminal coding agent. Through URI protocols,
it lets any tool load the way Skills do in other agents: the model sees only a
compact name and description at first, then reads the full contract,
instructions, and resources when it selects that tool.

Its built-in plugins register a small model-facing interface:

```text
read(uri: string, body: string)
exec(uri: string, body: string)
replace(path: string, old_text: string, new_text: string)
apply_patch(patch: string)
```

`read` and `exec` route simple string input through URI protocols; their body is
always a string, including `""` when empty. Typed tools handle arguments that
would be awkward or error-prone to encode as strings. Trusted WASM plugins can
register more protocols and typed tools at runtime.

> [!WARNING]
> URI Agent is not a sandbox. File and shell protocols, and enabled WASM
> plugins, run with the authority of the `uri-agent` process. Use only projects,
> configuration, and plugins you trust.

URI Agent is an early release and may change between dated versions. Model
requests and their context are sent to the provider you select. Unless offline
mode is enabled, URI Agent also fetches model catalog metadata from pi.dev and
supported providers.

## Why URI Agent

- **Progressive context:** protocol contracts, Skill resources, embedded
  documentation, and oversized output stay out of the model context until they
  are needed.
- **Extensible tools:** linked and trusted WASM plugins can add protocols or
  typed tools without changing the runtime dispatch path.
- **Built-in MCP bridge:** `:mcp` manages stdio and Streamable HTTP servers,
  exposing each one as an on-demand `<name>-mcp://` protocol with query-first
  arguments and a complete-JSON fallback for complex schemas.
- **ACP editor integration:** `uri-agent --acpv1` serves stable ACP v1 over
  stdio, with per-session model setup and conversations that can later reopen
  through the normal TUI.
- **Broad model access:** the pi.dev catalog, provider-specific sign-in, and
  credential-scoped live discovery bring a wide provider ecosystem into one
  model selector.
- **Durable work:** long commands become managed tasks, while append-only
  SQLite sessions preserve drafts, frozen startup context, titled working
  notes, and rollover or summary checkpoints across restarts.
- **One terminal workflow:** Queue and Steer, built-in web access, keyboard and
  mouse controls, image input, and `@` file or `@@` session references share one
  conversation surface.

## Model and provider coverage

URI Agent deliberately targets broad pi.dev compatibility rather than a fixed
handful of models. As of 2026-08-30, the current catalog coverage is:

| Catalog measure | Supported |
| --- | ---: |
| API families | 5 of 9 |
| Model entries | 1,073 of 1,274 (84.2%) |
| Provider IDs | 35 of 39 |
| Provider IDs with live discovery | 28 of 35 runnable |

The supported API families are OpenAI Responses, OpenAI Codex Responses,
OpenAI Chat Completions, Anthropic Messages, and Google Generative AI. Live
provider results are cached per credential and supplement the shared catalog,
so newly available account models can appear before pi.dev adds them.

Dedicated integrations cover the places where generic catalog compatibility is
not enough: ChatGPT Codex subscription OAuth and WebSocket transport,
Cloudflare AI Gateway's credential-safe endpoint boundary, WorkBuddy China
browser login and account model discovery, and the explicitly experimental
Antigravity private protocol. A built-in Abliteration.ai catalog provides
credential-scoped live discovery and static fallback models. URI Agent also
supports provider-specific login flows for Anthropic, GitHub Copilot, Kimi
Coding, xAI, Radius, and OpenRouter.

Catalog contents and account entitlements change; a listed model still requires
the matching credentials, region, and subscription. See [Models and
configuration](docs/configuration.md#model-catalog) for current provider,
discovery, authentication, and compatibility details.

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

### Start your first session

Launch URI Agent with the project directory it should use:

```bash
uri-agent --cwd /path/to/project
```

`--cwd` sets the project and default working directory; it is not a filesystem access boundary. If the project contains `AGENTS.md`, review it before launch: new sessions include those instructions and freeze their startup context.

URI Agent does not choose a default model. In the TUI:

1. Run `:login` to save an API key or complete a supported OAuth flow.
2. Run `:model` and select a runnable model.
3. Press `Space`, enter a request, and press `Enter`.

The first session is working when protocol activity appears and the assistant returns an answer based on the project. Press `F1` or run `:help` for the active commands and key bindings.

See [Models and configuration](docs/configuration.md) for provider support,
authentication, offline mode, environment variables, and custom endpoints.

### Connect an ACP client

After configuring model authentication in the TUI, configure an ACP client to
launch:

```text
uri-agent --acpv1
```

The client supplies each session's absolute project directory; one ACP process
can host independent sessions for multiple projects. Compatible ACP clients can
select an authenticated model and thinking level before the first prompt
without changing URI Agent's defaults. The first prompt makes the session
durable under its already assigned ID; it can reopen in the TUI after the client
releases it. See [ACP v1](docs/acp.md) for supported content, MCP servers,
lifecycle operations, and ownership constraints.

## Documentation

| Goal | Guide |
| --- | --- |
| Connect an editor or other ACP client | [ACP v1](docs/acp.md) |
| Choose providers, authenticate, or change settings | [Models and configuration](docs/configuration.md) |
| Use the conversation, commands, keymap, terminal, or attachments | [Terminal interface](docs/interface.md) and [terminal features](docs/terminal.md) |
| Understand tools, protocols, tasks, and complete output | [Protocols, tasks, and output](docs/protocols.md) |
| Use project instructions or Skills | [Startup context and Skills](docs/context.md) |
| Resume sessions or understand notes, rollover, and persistence | [Sessions and context](docs/sessions.md) |
| Build or audit an extension | [WASM plugins](docs/plugins.md) |

The [`docs/` index](docs/README.md) includes contributor and release guides. At
runtime, `<protocol>://help` is the authoritative reference for a protocol's
accepted URIs and body shape; `F1` and `:help` show the active interface
reference.

## Development

The project builds on stable Rust. Read [`AGENTS.md`](AGENTS.md) before changing the repository; the [development guide](docs/development.md) defines module ownership, change rules, and required verification.

## License

[MIT](LICENSE) © 2026 4fuu
