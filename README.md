# uri-agent

[English](README.md) | [简体中文](README.zh-CN.md)

[![License: MIT](https://img.shields.io/badge/license-MIT-6ed2c2.svg)](LICENSE)

A protocol-oriented coding agent with a focused terminal interface. Models always see the same two tools—`read` and `exec`—while protocols, Skills, tasks, and large outputs are loaded only when needed.

## Why uri-agent

Agent tool lists tend to grow with every integration. Their schemas consume context before the model knows whether it needs them, and each new tool adds another calling convention to learn.

uri-agent keeps the model-facing contract small:

- **Two tools** — every capability is reached through `read(uri, body?)` or `exec(uri, body?)`.
- **Progressive disclosure** — the initial prompt lists protocol names and purposes; detailed instructions live at `<protocol>://help`.
- **Opaque addresses** — the router splits only at the first `://` and leaves the remaining target untouched.
- **Asynchronous by default** — execution returns a task address immediately, with an optional bounded wait when the immediate result matters.
- **One Skill, one protocol** — each Skill gets a distinct, model-visible name instead of sharing a generic Skill endpoint.
- **Recoverable context** — sessions preserve model messages and tool-call identity; oversized output remains available through a `file://` address.

The result is a stable tool surface that can grow without turning every integration into permanent prompt overhead.

## Features

### A two-tool protocol router

The model receives exactly these tool definitions:

```text
read(uri: string, body?: any)
exec(uri: string, body?: any)
```

`body` may be any JSON value, including a Markdown string, array, object, number, boolean, or null. The registry passes the original URI, opaque target, and body to the selected protocol without URL decoding or normalization.

```text
read("file://src/main.rs?offset=1&limit=200")
read("code-review-skill://help")
exec("bash://?wait=30", "cargo test")
```

Protocols implement `read`, `exec`, or both through the [`Protocol`](src/protocol.rs) trait. Adding a protocol does not add another model-facing tool.

### Managed asynchronous tasks

`exec` is asynchronous unless a protocol documents otherwise. Shell and edit operations normally return a task address:

```text
exec("bash://run", "cargo test")
→ Read status: bash://tasks/<id>
```

Use `?wait=N` to wait for up to 300 seconds:

```text
exec("bash://?wait=30", "cargo test")
```

If the task finishes in that window, the result is returned directly. If the wait expires, the task continues in the background and can be inspected with `read("bash://tasks/<id>")`.

### Built-in protocols

| Protocol | Entry points | Purpose |
| --- | --- | --- |
| `file` | `read` | Read files and directory listings with bounded line ranges |
| `edit` | `read`, `exec` | Atomically write a file or replace one exact text match |
| `bash` | `read`, `exec` | Run managed Bash tasks when Bash is available |
| `pwsh` | `read`, `exec` | Run managed PowerShell 7 tasks when `pwsh` is available |
| `<name>-skill` | `read` | Load one Skill prompt and its associated resources |

The model reads `<protocol>://help` before using an unfamiliar protocol. Bash and PowerShell are detected at startup and registered only when their executables are available.

### Skills as prompt protocols

Every discovered `SKILL.md` must contain `name` and `description` YAML frontmatter. Its name becomes a dedicated protocol:

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

The help response includes the complete `SKILL.md` and the real `file://` directory containing its files, so the model can inspect or run bundled scripts. Resource paths cannot escape the Skill directory through `..` or symbolic links.

Skills are scanned in this order:

```text
<cwd>/.agents/skills
<cwd>/.claude/skills
<cwd>/.codex/skills
<cwd>/.amp/skills
~/.agents/skills
~/.claude/skills
~/.codex/skills
~/.config/amp/skills
~/.cache/amp/global-skills
```

When two Skills produce the same protocol name, the higher-priority location wins.

### Complete output without permanent context cost

Tool output is bounded before it is returned to the model. When content exceeds the configured limit, uri-agent shows a head-and-tail preview, saves the complete output, and returns a `file://` address for targeted follow-up reads.

### Provider-neutral sessions

The Rig adapter supports OpenAI Responses, Anthropic, and Gemini. uri-agent owns the tool loop and stores provider-neutral model messages in an append-only JSONL session, including tool-call IDs and reasoning signatures required for a correct resume.

An interrupted final JSONL write is repaired when the session is reopened. Earlier malformed records remain errors rather than being silently skipped.

### Focused terminal interface

The Ratatui interface supports streaming text and reasoning, multiline input, bracketed paste, mouse and keyboard scrolling, protocol and task overlays, task cancellation, session replay, and a low-noise dither animation while the model is working.

## Requirements

- Rust stable with Cargo
- A terminal with standard ANSI and alternate-screen support
- An API key for OpenAI, Anthropic, or Gemini
- Bash and/or PowerShell 7 only if the corresponding shell protocol is needed

## Installation

Clone and build the release binary:

```bash
git clone https://github.com/4fuu/uri-agent.git
cd uri-agent
cargo build --release
```

Run it from the repository:

```bash
export OPENAI_API_KEY=...
./target/release/uri-agent --cwd /path/to/project
```

### Install from source

```bash
cargo install --path .
uri-agent --cwd /path/to/project
```

## Configuration

The provider and model can be selected with flags or environment variables:

```bash
uri-agent \
  --provider anthropic \
  --model claude-sonnet-4-6 \
  --cwd /path/to/project
```

| Setting | Environment variable | Default | Effect |
| --- | --- | --- | --- |
| `--provider` | `URI_AGENT_PROVIDER` | `openai` | Select `openai`, `anthropic`, or `gemini` |
| `--model` | `URI_AGENT_MODEL` | Provider-specific | Override the model identifier |
| `--cwd` | — | `.` | Set the working directory used by built-in protocols |
| `--session` | — | New session | Resume a session ID; use `latest` for the newest |
| `--output-limit` | — | `32768` | Set model-visible bytes before complete output is saved |

Provider credentials use the standard environment variables:

| Provider | API key | Default model |
| --- | --- | --- |
| OpenAI | `OPENAI_API_KEY` | `gpt-5.2` |
| Anthropic | `ANTHROPIC_API_KEY` | `claude-sonnet-4-6` |
| Gemini | `GEMINI_API_KEY` | `gemini-3-flash-preview` |

Resume the most recent session:

```bash
uri-agent --session latest --cwd /path/to/project
```

Sessions are stored under the platform data directory at `uri-agent/sessions/<session-id>/events.jsonl`. Complete tool outputs are stored under the platform cache directory at `uri-agent/outputs/<session-id>/`.

## TUI controls

| Key | Action |
| --- | --- |
| `Enter` | Send the message |
| `Shift+Enter` | Insert a newline |
| `PageUp` / `PageDown` | Scroll the conversation or active overlay |
| `F1` | Open help |
| `Ctrl+P` | Open the protocol list |
| `Ctrl+T` | Open the task list |
| `x` | Cancel the selected task in the task list |
| `Esc` | Close the active overlay |
| `Ctrl+C` | Exit |

## Security

The built-in file and shell protocols are not a sandbox. The agent has the filesystem and command permissions of the uri-agent process, including the ability to address absolute paths. Run it only in trusted projects and with an environment whose credentials the agent may use.

## Development

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo check
```

The test suite covers protocol dispatch, arbitrary body forwarding, asynchronous waits, atomic edits, output preservation, Skill containment, session recovery, and a provider-independent end-to-end tool loop. Repository invariants and the code map are documented in [`AGENTS.md`](AGENTS.md).

## License

[MIT](LICENSE) © 2026 4fuu
