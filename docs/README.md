# URI Agent documentation

The root [README](../README.md) explains what URI Agent is and provides the shortest path to a working session. This directory owns the detailed product and contributor documentation.

## Choose a document

| Goal | Document |
| --- | --- |
| Understand `read`, `exec`, direct tools, protocol routing, MCP operations, built-ins, tasks, or output preservation | [Protocols, tasks, and output](protocols.md) |
| Understand project instructions, or create and troubleshoot a Skill | [Startup context and Skills](context.md) |
| Build, install, reload, or audit a trusted WASM plugin | [WASM plugins](plugins.md) |
| Configure a model, credentials, MCP server, Agent environment variables, reasoning effort, output limits, offline mode, or a custom endpoint | [Models and configuration](configuration.md) |
| Use the conversation, composer, commands, MCP manager, and navigation | [Terminal interface](interface.md) |
| Customize keys, use the embedded terminal, copy text, or attach images | [Keymaps, terminal, and attachments](terminal.md) |
| Resume a session or understand persistence, the model/tool loop, retries, frozen context, and compaction | [Sessions and context](sessions.md) |
| Change the codebase, find module ownership, preserve product contracts, or run verification | [Architecture and development](development.md) |
| Prepare or configure a release | [Release process](release.md) |

## Sources of truth

These documents explain stable concepts and cross-cutting behavior. More specific references remain authoritative:

- `uri-agent --help` defines the current command-line interface.
- The active model-tool schemas define direct-tool arguments; `<protocol>://help` defines a registered protocol's accepted addresses, string body, execution behavior, result routes, and limits.
- `uri-agent-docs://README.md` exposes this documentation embedded in the running binary, independent of its startup working directory.
- `wasm_plugin://help` publishes active WASM plugin state and routes to separate loading and authoring help pages.
- `F1` and `:help` show the active command and keymap reference after global and project overrides are applied.
- The [pi model catalog](https://github.com/earendil-works/pi) plus local `models.json` defines the available providers and models.
- [`AGENTS.md`](../AGENTS.md) is the concise entry point for coding agents; the [development guide](development.md) contains the detailed repository rules it references.

## Documentation policy

- Keep root `README.md` and `README.zh-CN.md` equivalent as adoption guides.
- Detailed documents in `docs/` are maintained in English only.
- Put mutable detail in one authoritative document and link to it instead of copying it.
- Keep protocol-specific model instructions in `<protocol>://help`; documentation here should explain how the protocol system fits together.
