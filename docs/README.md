# URI Agent documentation

The root [README](../README.md) explains what URI Agent is and provides the shortest path to a working session. This directory owns the detailed product and contributor documentation.

## Choose a document

| Goal | Document |
| --- | --- |
| Understand `read`, `exec`, protocol routing, tasks, output preservation, Skills, or extensions | [Protocols, tasks, Skills, and extensions](protocols.md) |
| Configure a model, credentials, reasoning effort, output limits, offline mode, or a custom endpoint | [Models and configuration](configuration.md) |
| Use the TUI, commands, keymaps, embedded terminal, image attachments, sessions, resume, or compaction | [Terminal interface and sessions](interface.md) |
| Change the codebase, find module ownership, preserve product contracts, or run verification | [Architecture and development](development.md) |

## Sources of truth

These documents explain stable concepts and cross-cutting behavior. More specific references remain authoritative:

- `uri-agent --help` defines the current command-line interface.
- `<protocol>://help` defines a registered protocol's accepted addresses, body shape, asynchronous behavior, result routes, and limits.
- `F1` and `:help` show the active command and keymap reference after global and project overrides are applied.
- The [pi model catalog](https://github.com/earendil-works/pi) plus local `models.json` defines the available providers and models.
- [`AGENTS.md`](../AGENTS.md) is the concise entry point for coding agents; the [development guide](development.md) contains the detailed repository rules it references.

## Documentation policy

- Keep root `README.md` and `README.zh-CN.md` equivalent as adoption guides.
- Detailed documents in `docs/` are maintained in English only.
- Put mutable detail in one authoritative document and link to it instead of copying it.
- Keep protocol-specific model instructions in `<protocol>://help`; documentation here should explain how the protocol system fits together.
