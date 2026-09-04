<p align="center">
  <img src="docs/assets/logo.svg" width="420" alt="URI Agent logo">
</p>

# URI Agent

[English](README.md) | [简体中文](README.zh-CN.md)

[![License: MIT](https://img.shields.io/badge/license-MIT-6ed2c2.svg)](LICENSE)

URI Agent is an extensible terminal coding agent that keeps model context
focused on the current task. Its tools use URI protocols to load their full
contracts, instructions, and resources only when selected, much like Skills in
other agents. At first, the model sees only each tool's compact name and
description.

Four built-in tools give the model a compact interface for reading, executing,
and editing:

```text
read(uri: string, body: string)
exec(uri: string, body: string)
replace(path: string, old_text: string, new_text: string)
apply_patch(patch: string)
```

`read` and `exec` always take string bodies and route them through URI
protocols; use `""` when a body is empty. Typed tools handle structured or
escape-heavy arguments. Trusted WASM plugins can add protocols and typed tools
at runtime.

> [!WARNING]
> URI Agent is not a sandbox. File and shell protocols, and enabled WASM
> plugins, run with the authority of the `uri-agent` process. Use only projects,
> configuration, and plugins you trust.

URI Agent is an early release and may change between dated versions. Model
requests and their context are sent to the provider you select. Unless offline
mode is enabled, URI Agent also fetches model catalog metadata from pi.dev and
supported providers.

## Why URI Agent

- **Progressive context:** load protocol contracts, Skill resources, embedded
  documentation, and oversized output only when they are needed.
- **Extensible tools:** add protocols or typed tools through linked Rust plugins
  and trusted WASM plugins without changing runtime dispatch.
- **Built-in MCP bridge:** connect stdio and Streamable HTTP servers with
  `:mcp`. Each server becomes an on-demand `<name>-mcp://` protocol, with
  query-first arguments and a complete-JSON fallback for complex schemas.
- **ACP editor integration:** use URI Agent from compatible editors through
  stable ACP v1 over stdio. Each session can select its model, and its
  conversation can later reopen in the normal TUI.
- **Broad model access:** choose from the pi.dev catalog, use provider-specific
  sign-in, and discover account models without leaving the model selector.
- **Durable work:** let long commands continue as managed tasks and resume work
  across restarts. Append-only SQLite sessions preserve drafts, frozen startup
  context, titled working notes, and rollover or summary checkpoints.
- **Session collaboration:** give running TUI sessions persistent human names,
  inspect their model status, and exchange durable Queue or Steer messages
  across URI Agent processes. Stopped peers are not started automatically.
- **Local semantic retrieval:** run on-demand semantic and hybrid search across
  project files and saved conversations with bundled zvec and Model2Vec assets.
- **One terminal workflow:** use Queue and Steer, web access, keyboard and mouse
  controls, image input, and `@` file or `@@` session references from one
  conversation surface.

## Model and provider coverage

URI Agent targets broad compatibility with the pi.dev catalog rather than a
fixed handful of models. As of 2026-09-04, the catalog coverage is:

| Catalog measure | Supported |
| --- | ---: |
| API families | 5 of 9 |
| Model entries | 1,132 of 1,337 (84.7%) |
| Provider IDs | 35 of 39 |
| Provider IDs with live discovery | 28 of 35 runnable |

The supported API families are OpenAI Responses, OpenAI Codex Responses,
OpenAI Chat Completions, Anthropic Messages, and Google Generative AI. Live
provider results are cached per credential and supplement the shared catalog,
so newly available account models can appear before pi.dev adds them.

Where generic catalog compatibility is not enough, URI Agent provides dedicated
integrations for:

- ChatGPT Codex subscription OAuth and WebSocket transport;
- Cloudflare AI Gateway with credential-safe endpoint handling;
- WorkBuddy China browser login and account model discovery;
- the explicitly experimental Antigravity private protocol; and
- Abliteration.ai credential-scoped live discovery with static fallback
  models.

URI Agent also supports provider-specific login flows for Anthropic, GitHub
Copilot, Kimi Coding, xAI, Radius, and OpenRouter.

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

On x86-64 or ARM64 Linux, the installer verifies the release checksum and
installs the complete versioned bundle under `~/.local/bin`:

```bash
curl --proto '=https' --tlsv1.2 -LsSf https://raw.githubusercontent.com/4fuu/uri-agent/main/scripts/install.sh | sh
```

Every installation method above includes the matching zvec dynamic library,
Jieba dictionaries, and embedding model. URI Agent does not download these
semantic retrieval assets at runtime. Source builds must prepare the same
assets as described in the [development
guide](docs/development.md#native-retrieval-assets).

### Start your first session

Launch URI Agent with the project directory it should use:

```bash
uri-agent --cwd /path/to/project
```

`--cwd` selects the project and default working directory; it does not restrict
filesystem access. If the project contains `AGENTS.md`, review it before
launch: new sessions include those instructions in their frozen startup
context.

URI Agent does not choose a default model. In the TUI:

1. Run `:login` to save an API key or complete a supported OAuth flow.
2. Run `:model` and select a runnable model.
3. Press `Space`, enter a request, and press `Enter`.

The first session is working when protocol activity appears and the assistant
returns an answer based on the project. Press `F1` or run `:help` for the active
commands and key bindings.

See [Models and configuration](docs/configuration.md) for provider support,
authentication, offline mode, environment variables, and custom endpoints.

### Connect an ACP client

After signing in to a model provider in the TUI, configure an ACP client to
launch:

```text
uri-agent --acpv1
```

The client supplies an absolute project directory for each session, and one ACP
process can host independent sessions for multiple projects. Compatible ACP
clients can select an authenticated model and thinking level before the first
prompt without changing URI Agent's defaults. The first prompt persists the
session under its assigned ID; after the client releases it, the session can
reopen in the TUI. See [ACP v1](docs/acp.md) for supported content, MCP servers,
lifecycle operations, and ownership constraints.

## Documentation

| Goal | Guide |
| --- | --- |
| Connect an editor or other ACP client | [ACP v1](docs/acp.md) |
| Choose providers, authenticate, or change settings | [Models and configuration](docs/configuration.md) |
| Use the conversation, commands, keymap, terminal, or attachments | [Terminal interface](docs/interface.md) and [terminal features](docs/terminal.md) |
| Understand tools, protocols, tasks, and complete output | [Protocols, tasks, and output](docs/protocols.md) |
| Use project instructions or Skills | [Startup context and Skills](docs/context.md) |
| Resume sessions or understand collaboration, notes, rollover, and persistence | [Sessions and context](docs/sessions.md) |
| Build or audit an extension | [WASM plugins](docs/plugins.md) |

The [`docs/` index](docs/README.md) includes contributor and release guides. At
runtime, `<protocol>://help` is the authoritative reference for a protocol's
accepted URIs and body shape; `F1` and `:help` show the active interface
reference.

## Development

The project builds on stable Rust. Read [`AGENTS.md`](AGENTS.md) before changing the repository; the [development guide](docs/development.md) defines module ownership, change rules, and required verification.

## License

[MIT](LICENSE) © 2026 4fuu
