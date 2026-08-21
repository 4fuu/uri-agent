# URI Agent

[English](README.md) | [简体中文](README.zh-CN.md)

[![License: MIT](https://img.shields.io/badge/license-MIT-6ed2c2.svg)](LICENSE)

URI Agent is a terminal coding agent built around a small, stable model interface. The model always sees two tools—`read` and `exec`—and reaches files, shells, edits, Skills, and future integrations through protocol addresses such as `file://...` and `bash://...`.

This design keeps tool schemas out of the model context until they are useful. Every protocol documents itself at `<protocol>://help`, long-running work is represented by managed tasks, and oversized output remains available through a `file://` address instead of being discarded.

> [!WARNING]
> URI Agent is not a sandbox. Its file and shell protocols run with the permissions of the `uri-agent` process. Use it only in projects and environments you trust.

## Quick start

### Requirements

- a stable Rust toolchain and Git;
- an API key for a supported provider;
- a terminal with standard keyboard input. Mouse support is optional.

[Helix](https://helix-editor.com/) and [fzf](https://github.com/junegunn/fzf) are optional. They power the default external editor and conversation finder, but URI Agent runs without them.

### Install from source

```bash
git clone https://github.com/4fuu/uri-agent.git
cd uri-agent
cargo install --path .
```

### Start a session

A fresh configuration defaults to OpenAI. Set its key and launch URI Agent at the project you want the agent to work in:

```bash
export OPENAI_API_KEY="<your-api-key>"
uri-agent --cwd /path/to/project
```

You can also start without a key, press `F2`, and configure the provider, model, and credential in Settings. Press `F3` to search the runnable model catalog.

To send the first request:

1. Press `i` to enter Insert mode.
2. Type the request. `Enter` inserts a newline.
3. Press `Esc` to keep the draft and return to Browse mode.
4. Press `Enter` in Browse mode to submit it.

Press `Space` for the command panel or `F1` for the active keymap and command reference.

## Why protocols

The model-facing tool surface never grows when a capability is added:

```text
read(uri: string, body?: any)
exec(uri: string, body?: any)
```

Examples:

```text
read("file://src/main.rs?offset=1&limit=200")
read("code-review-skill://help")
exec("bash://?wait=30", "cargo test")
```

URI Agent splits an address only at the first `://`. The remaining target is opaque: the registry does not URL-decode, normalize, or reinterpret it. `body` may be any JSON value and is passed to the selected protocol unchanged.

Protocols implement `read`, `exec`, or both through the [`Protocol`](src/protocol.rs) trait. Registering another protocol does not add another model-facing tool.

### Built-in protocols

| Protocol | Operations | Purpose |
| --- | --- | --- |
| `file` | `read` | Read files and bounded directory listings |
| `edit` | `read`, `exec` | Atomically write a file or replace one exact match |
| `bash` | `read`, `exec` | Run Bash commands as managed tasks when Bash is installed |
| `pwsh` | `read`, `exec` | Run PowerShell 7 commands as managed tasks when `pwsh` is installed |
| `<name>-skill` | `read` | Load one discovered Skill and its bundled resources |

`bash` and `pwsh` are detected at startup and registered only when their executables are available.

### Managed tasks and complete output

Execution is asynchronous by default. A shell or edit request normally returns a task address immediately:

```text
exec("bash://run", "cargo test")
→ Read status: bash://tasks/<id>
```

The shell protocols accept `?wait=N` for a bounded wait of up to 300 seconds:

```text
exec("bash://?wait=30", "cargo test")
```

If the wait expires, the task keeps running. Read `<protocol>://tasks/<id>` for its current status and eventual result. Waiting is a shell-protocol feature, not generic URI behavior.

When a tool result exceeds the configured inline limit, URI Agent returns a head-and-tail preview and saves the complete bytes in the session output directory. The preview includes a readable `file://` address for the full result.

## Skills

URI Agent discovers Skills once at startup from these roots, in priority order:

```text
<project>/.agents/skills
<project>/.claude/skills
<project>/.codex/skills
~/.agents/skills
~/.claude/skills
~/.codex/skills
```

Each root may contain its own `SKILL.md` or direct child directories containing `SKILL.md`. A Skill needs `name` and `description` YAML frontmatter. Its normalized name becomes a dedicated protocol:

```yaml
---
name: Code Review
description: Review a change for correctness and regressions.
---
```

```text
code-review-skill://help
code-review-skill://scripts/check.py
```

The first Skill for a normalized protocol name wins. Skills that collide with an already registered built-in protocol are skipped with a notice. Resource reads are contained within the Skill directory.

A new session freezes the complete generated system prompt plus each selected Skill's name, description, and canonical `SKILL.md` path. Resuming the session reuses that snapshot. Skill help and resources are still read from the frozen path, so a missing file fails explicitly and a same-named Skill elsewhere cannot silently replace it.

## Models and authentication

URI Agent uses the [pi](https://github.com/badlogic/pi-mono) model catalog and currently runs these API families through its Rust/Rig backend:

- `openai-responses`
- `openai-completions`
- `anthropic-messages`
- `google-generative-ai`

The model picker shows only runnable API families. Authentication is currently API-key based; providers that require OAuth or ambient cloud credentials may appear in the catalog but cannot run without a dedicated adapter.

The catalog is cached for four hours. Use `--offline`, `URI_AGENT_OFFLINE=1`, or `PI_OFFLINE=1` to disable catalog requests and use only local data. `Ctrl+R` refreshes the catalog from the model picker or Settings.

## Configuration

Press `F2` to edit the active provider, model, credential, output limit, external editor, conversation picker, and their display modes. Changes apply to the current session immediately.

On Linux, the default config directory is `~/.config/uri-agent`; set `URI_AGENT_CONFIG_DIR` to use another location.

| File | Purpose |
| --- | --- |
| `settings.json` | Global provider, model, output, editor, and picker settings |
| `auth.json` | Global provider credentials; created with mode `0600` on Unix |
| `models.json` | User-defined providers, models, headers, and model overrides |
| `models-store.json` | Generated pi catalog cache |
| `keymap.rhai` | Global keymap overrides |
| `<project>/.uri-agent/settings.json` | Optional project settings |
| `<project>/.uri-agent/keymap.rhai` | Optional project keymap overrides |

Project settings override global settings. Environment variables override files, and command-line flags override environment variables. Credentials use this order, from lowest to highest priority:

```text
models.json apiKey
< auth.json
< provider environment variable
< URI_AGENT_API_KEY
< --api-key
```

Known providers use their conventional variables, including `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `GEMINI_API_KEY`, `OPENROUTER_API_KEY`, and `GROQ_API_KEY`.

Credential and header values in trusted configuration support pi-style environment expansion and a leading `!shell command`. A leading `!` executes with the permissions of URI Agent, so do not use configuration from an untrusted project.

### Common command-line options

```text
--provider <ID>          Select a provider
--model <ID>             Select one of that provider's models
--api-key <KEY>          Set a credential for this process only
--cwd <PATH>             Set the project and protocol working directory
--continue-session       Resume this project's latest session
--session <ID|latest>    Resume a specific session
--output-limit <BYTES>   Set inline output size (minimum 1024)
--editor <COMMAND>       Set the external editor
--editor-mode <MODE>     Use float or fullscreen
--picker <COMMAND>       Set the conversation finder
--picker-mode <MODE>     Use float or fullscreen
--offline                Disable pi catalog network requests
```

Run `uri-agent --help` for the current CLI reference.

### Custom OpenAI-compatible provider

Add a provider to `models.json` when you need a local or custom endpoint:

```json
{
  "providers": {
    "local-openai": {
      "baseUrl": "http://127.0.0.1:11434/v1",
      "api": "openai-completions",
      "apiKey": "local",
      "models": [
        {
          "id": "qwen3-coder",
          "name": "Qwen3 Coder",
          "contextWindow": 131072,
          "maxTokens": 16384
        }
      ]
    }
  }
}
```

## Sessions and context

Sessions are append-only and stored in SQLite:

```text
<platform-data-dir>/uri-agent/sessions.db
```

The canonical `--cwd` directory is the project boundary. `--continue-session` resumes only that project's latest session, and `--session <id>` rejects a session created for another project.

As model replay approaches the selected model's context window, URI Agent creates a summary checkpoint and replays it with complete recent user turns. Original events remain in SQLite, and tool calls are never separated from their results. Run `:compact` to request an earlier checkpoint.

## Terminal interface

| Context | Useful defaults |
| --- | --- |
| Browse | `↑/↓` select, `Enter` submit/open, `i` compose, `o` detail, `Space` commands, `:` command line |
| Insert | `Enter` newline, `Ctrl+E` external editor, `Esc` keep draft and return to Browse |
| Detail | `↑/↓` and `PageUp/PageDown` scroll, `e` external editor, `Esc` close |
| Global | `F1` help, `F2` settings, `F3` models, `Ctrl+P` protocols, `Ctrl+T` tasks, `Ctrl+C` quit |

Arrow keys and mouse input are first-class; `j` and `k` are optional aliases. Read-only panels support mouse selection and OSC52 copy. Interactive PTYs use Shift-drag so normal clicks continue to reach the embedded program.

The keymap is layered from built-in defaults through global and project `keymap.rhai` files. Each file can map or remove an action:

```rhai
map("browse", "x", "detail");
unmap("browse", "e");
map("insert", "ctrl+j", "newline");
```

Helix (`hx`) and fzf run in embedded PTY floats by default. Set their mode to `fullscreen` in Settings when an application should temporarily take over the terminal. GUI editors should include a wait option, such as `code --wait`.

## Extending URI Agent

Rust extensions register protocols, commands, and generic TUI panel providers through [`PluginHost`](src/plugin.rs). Protocols remain behind `read` and `exec`; commands join the command palette, colon command line, and keymap action registry. URI Agent does not currently load native dynamic libraries, so third-party Rust extensions must be linked during application assembly.

## Development

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo check
```

Repository invariants, module ownership, and change requirements are documented in [`AGENTS.md`](AGENTS.md).

## License

[MIT](LICENSE) © 2026 4fuu
