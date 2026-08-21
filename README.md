# uri-agent

[English](README.md) | [简体中文](README.zh-CN.md)

[![License: MIT](https://img.shields.io/badge/license-MIT-6ed2c2.svg)](LICENSE)

A protocol-oriented coding agent for the terminal. The model always sees two tools—`read` and `exec`—while protocols, Skills, task results, and large outputs are loaded only when needed.

## Design

Agent tool lists tend to grow with every integration. Their schemas consume context before the model knows whether it needs them, and every new tool adds another calling convention.

uri-agent keeps the model-facing contract stable:

- **Two tools** — every capability is reached through `read(uri, body?)` or `exec(uri, body?)`.
- **Progressive disclosure** — the system prompt lists protocol names and purposes; operational instructions live at `<protocol>://help`.
- **Opaque addresses** — the router splits at the first `://`; the remaining target is not URL-decoded or normalized.
- **Arbitrary bodies** — `body` may be any JSON value and is passed to the protocol unchanged.
- **Async execution** — `exec` returns a managed task by default; `?wait=N` requests a bounded wait when an immediate result is useful.
- **One Skill, one protocol** — every Skill receives its own model-visible protocol name.
- **Recoverable sessions** — model messages and tool-call identity are stored in SQLite, while oversized output remains available through `file://`.

## Tool protocol

The model receives exactly these definitions:

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

Protocols implement `read`, `exec`, or both through the [`Protocol`](src/protocol.rs) trait. Registering a protocol does not add a model-facing tool.

### Managed tasks

Shell and edit execution normally returns a task address immediately:

```text
exec("bash://run", "cargo test")
→ Read status: bash://tasks/<id>
```

Use `?wait=N` to wait for up to 300 seconds:

```text
exec("bash://?wait=30", "cargo test")
```

If the wait expires, the task continues in the background. Read its status and complete result from `<protocol>://tasks/<id>`.

### Built-in protocols

| Protocol | Operations | Purpose |
| --- | --- | --- |
| `file` | `read` | Read files and directory listings with bounded line ranges |
| `edit` | `read`, `exec` | Atomically write a file or replace one exact text match |
| `bash` | `read`, `exec` | Run managed Bash tasks when Bash is available |
| `pwsh` | `read`, `exec` | Run managed PowerShell 7 tasks when `pwsh` is available |
| `<name>-skill` | `read` | Load one Skill prompt and its bundled resources |

Bash and PowerShell are detected at startup and registered only when their executables are present.

### Skills as prompt protocols

Every discovered `SKILL.md` must contain `name` and `description` YAML frontmatter. The name is normalized into a dedicated protocol:

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

The help response includes the complete `SKILL.md` and the real `file://` directory containing its files, so the model can inspect or run bundled scripts. Resource routes cannot escape the Skill directory through `..` or symbolic links.

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

If two Skills normalize to the same protocol, the higher-priority location wins.

### Complete large output

Tool output is bounded before it is returned to the model. When content exceeds the configured limit, uri-agent returns a head-and-tail preview, preserves the complete bytes in the platform cache, and provides a `file://` address for targeted reads.

## Models and providers

uri-agent uses the current [pi](https://github.com/badlogic/pi-mono) cloud model catalog:

```text
https://pi.dev/api/models/providers
https://pi.dev/api/models/providers/<provider-id>
```

The generated `models-store.json` uses pi's cache schema, including `checkedAt`, `lastModified`, and `etag`. The cache refresh interval is four hours. A failed refresh leaves cached models usable; `--offline`, `URI_AGENT_OFFLINE=1`, or `PI_OFFLINE=1` disables catalog networking.

The Rust/Rig backend currently runs these pi API families:

- `openai-responses`
- `openai-completions`
- `anthropic-messages`
- `google-generative-ai`

The complete remote catalog is cached, while the Settings picker shows models from runnable API families. Provider entries that require OAuth or ambient cloud credentials may appear when their API family is supported, but uri-agent currently implements API-key authentication only. Bedrock, Vertex, Azure Responses, Codex OAuth, and Mistral Conversations need dedicated Rust adapters before they can run.

## Configuration

Start uri-agent without an API key and open Settings with `F2`, `Ctrl+,`, `/settings`, `/model`, or `/login`. The overlay edits provider, model, the selected provider's credential, and the inline output limit. Saving applies the model backend immediately without discarding the current session.

### Text files

uri-agent keeps ordinary configuration editable and separates generated data from user overrides. The platform config directory is `~/.config/uri-agent` on Linux; set `URI_AGENT_CONFIG_DIR` to override it.

| File | Owner | Purpose |
| --- | --- | --- |
| `settings.json` | uri-agent and user | pi-compatible global settings, including `defaultProvider` and `defaultModel` |
| `auth.json` | uri-agent and user | pi-compatible provider credential records; mode `0600` on Unix |
| `models-store.json` | uri-agent | generated cache pulled from `pi.dev` |
| `models.json` | user | pi-compatible custom providers, models, headers, and model overrides |
| `<cwd>/.uri-agent/settings.json` | user | optional project settings overlaid on global settings |

When project settings already exist, the TUI writes provider, model, and output-limit changes there; otherwise it writes global settings. Credentials always go to global `auth.json`.

Example `settings.json`:

```json
{
  "defaultProvider": "openai",
  "defaultModel": "gpt-5.2",
  "outputLimit": 32768
}
```

Example `auth.json`:

```json
{
  "openai": {
    "type": "api_key",
    "key": "$OPENAI_API_KEY"
  }
}
```

Example `models.json` override:

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

Credential and header values support pi-style `$VAR`, `${VAR}`, `$$`, `$!`, and a leading `!shell command`. Commands time out after ten seconds and are cached for the process lifetime. These files are trusted configuration: a leading `!` executes with the permissions of uri-agent.

### Precedence

Ordinary settings use this order, from lowest to highest:

```text
built-in defaults
< global settings.json
< project .uri-agent/settings.json
< URI_AGENT_* environment variables
< command-line flags
```

API keys use:

```text
models.json apiKey
< auth.json
< provider environment variable
< URI_AGENT_API_KEY
< --api-key
```

Environment variables and flags are runtime overrides and are not written back. The Settings overlay shows the effective source so a saved value is not mistaken for an active override.

### Command-line options

```bash
uri-agent \
  --provider anthropic \
  --model claude-sonnet-4-6 \
  --cwd /path/to/project
```

| Flag | Environment | Effect |
| --- | --- | --- |
| `--provider` | `URI_AGENT_PROVIDER` | Select a pi provider ID |
| `--model` | `URI_AGENT_MODEL` | Select a model ID for that provider |
| `--api-key` | `URI_AGENT_API_KEY` | Set a process-only credential |
| `--output-limit` | `URI_AGENT_OUTPUT_LIMIT` | Set inline output bytes; minimum 1024 |
| `--offline` | `URI_AGENT_OFFLINE`, `PI_OFFLINE` | Use the local model cache only |
| `--cwd` | — | Set the working directory exposed to built-in protocols |
| `--continue-session` | — | Resume the most recently updated session |
| `--session <id>` | — | Resume a session by ID; `latest` is also accepted |

Known providers use their standard environment names, such as `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `GEMINI_API_KEY`, `OPENROUTER_API_KEY`, and `GROQ_API_KEY`. A custom provider ID falls back to `<NORMALIZED_PROVIDER>_API_KEY`.

## Sessions

Sessions are stored in one SQLite database under the platform data directory:

```text
<data-dir>/uri-agent/sessions.db
```

SQLite WAL mode and transactional sequence allocation keep event order and model history consistent. Complete oversized outputs remain separate under the platform cache directory:

```text
<cache-dir>/uri-agent/outputs/<session-id>/
```

No JSONL compatibility layer is included because the SQLite format is the initial public persistence format.

## TUI

The Ratatui interface supports streaming text and reasoning, multiline and bracketed-paste input, mouse and keyboard scrolling, floating protocol/task/settings panels, task cancellation, session replay, and a low-noise dither animation while the model is working.

| Key | Action |
| --- | --- |
| `Enter` | Send the message |
| `Shift+Enter` | Insert a newline |
| `F2` | Open Settings |
| `Ctrl+,` | Open Settings |
| `PageUp` / `PageDown` | Scroll the conversation or active panel |
| `F1` | Open help |
| `Ctrl+P` | Open protocols |
| `Ctrl+T` | Open managed tasks |
| `Esc` | Stop editing or close the active panel |
| `Ctrl+C` | Exit |

Inside Settings, use `↑/↓` to select a field, `←/→` to browse, `Enter` to search a provider/model or edit a credential/output limit, `x` to clear the selected credential, `s` to save, and `r` to refresh the pi catalog.

## Installation

```bash
git clone https://github.com/4fuu/uri-agent.git
cd uri-agent
cargo build --release
./target/release/uri-agent --cwd /path/to/project
```

Or install from the checkout:

```bash
cargo install --path .
uri-agent --cwd /path/to/project
```

## Security

The built-in file and shell protocols are not a sandbox. The agent has the filesystem and command permissions of the uri-agent process, including access to absolute paths. Run it only in trusted projects and with credentials the agent may use. Treat `auth.json`, `models.json`, project settings, and discovered Skills as trusted input.

## Development

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo check
```

Repository invariants and the code map are documented in [`AGENTS.md`](AGENTS.md).

## License

[MIT](LICENSE) © 2026 4fuu
