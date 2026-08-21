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
- **Async execution** — protocols can return managed tasks immediately. Waiting is protocol-owned, not a generic `exec` or URI behavior.
- **One Skill, one protocol** — every Skill receives its own model-visible protocol name.
- **Stable sessions** — each session freezes its complete system prompt and Skill metadata. Model messages, tool-call identity, and compaction checkpoints are stored in SQLite, while oversized output remains available through `file://`.

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

The built-in shell protocols accept `?wait=N` to wait for up to 300 seconds:

```text
exec("bash://?wait=30", "cargo test")
```

If the wait expires, the task continues in the background. Read its status and complete result from `<protocol>://tasks/<id>`. This option is implemented by `bash` and `pwsh` themselves: the router passes the opaque target unchanged, and does not interpret `wait` for other protocols. Protocol implementations may expose their own waiting syntax by calling the URI-independent task waiting API.

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
~/.agents/skills
~/.claude/skills
~/.codex/skills
```

Discovery runs once at process startup. Each scan root contributes its own `SKILL.md` or direct child directories containing `SKILL.md`; the binary never contains a developer machine's Skill list. If two Skills normalize to the same protocol, the higher-priority location wins.

Creating a session stores only each selected Skill's `name`, `description`, and canonical `SKILL.md` path, together with the complete generated system prompt. Resuming that session uses this frozen snapshot instead of rebuilding the prompt from the current filesystem. `://help` and resource reads still load from the saved path, so edits to the Skill body are visible; removing the saved file produces an explicit error. A new same-named Skill elsewhere does not rebind an old session.

### Extension registration

The Rust extension surface groups model and interface contributions through [`PluginHost`](src/plugin.rs):

- register one or more [`Protocol`](src/protocol.rs) implementations;
- register stable command IDs, titles, descriptions, and colon aliases for the command palette and Rhai keymaps;
- register asynchronous TUI panel providers whose returned documents support scrolling, mouse selection, and OSC52 copy.

Protocol contributions remain behind `read` and `exec`; command and panel registration never adds model-facing tools. The built-in protocols use the same protocol contract. URI Agent does not currently load native dynamic libraries: third-party Rust plugins are linked and registered during application assembly.

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

The complete remote catalog is cached, while the model picker shows models from runnable API families. Open it with `F3`, `:model`, the command palette, or `/model [query]`. It searches provider IDs, model IDs, display names, and API families; provider grouping, current selection, context size, reasoning support, API family, and current credential state remain visible while browsing. Arrow keys, mouse clicks, and double-click selection work alongside typed search. `Ctrl+R` refreshes the pi catalog in the background without freezing the animation or input loop.

Provider entries that require OAuth or ambient cloud credentials may appear when their API family is supported, but uri-agent currently implements API-key authentication only. Bedrock, Vertex, Azure Responses, Codex OAuth, and Mistral Conversations need dedicated Rust adapters before they can run.

## Configuration

Start uri-agent without an API key and open Settings with `F2`, `Ctrl+,`, the Space command panel, or `:settings`. `/settings`, `/model`, and `/login` remain available from Insert mode. Provider and model rows open the searchable model picker; Settings edits the active provider's credential, inline output limit, editor and picker commands, and whether each external program uses an embedded float or the full terminal. Saving applies changes immediately without discarding the current session.

### Text files

uri-agent keeps ordinary configuration editable and separates generated data from user overrides. The platform config directory is `~/.config/uri-agent` on Linux; set `URI_AGENT_CONFIG_DIR` to override it.

| File | Owner | Purpose |
| --- | --- | --- |
| `settings.json` | uri-agent and user | pi-compatible global settings, including `defaultProvider` and `defaultModel` |
| `auth.json` | uri-agent and user | pi-compatible provider credential records; mode `0600` on Unix |
| `models-store.json` | uri-agent | generated cache pulled from `pi.dev` |
| `models.json` | user | pi-compatible custom providers, models, headers, and model overrides |
| `keymap.rhai` | user | global Rhai key mappings layered over the modern modal defaults |
| `<cwd>/.uri-agent/settings.json` | user | optional project settings overlaid on global settings |
| `<cwd>/.uri-agent/keymap.rhai` | user | optional project key mappings layered over the global keymap |

When project settings already exist, the TUI writes provider, model, output-limit, editor, and picker changes there; otherwise it writes global settings. Credentials always go to global `auth.json`.

Example `settings.json`:

```json
{
  "defaultProvider": "openai",
  "defaultModel": "gpt-5.2",
  "outputLimit": 32768,
  "editor": "hx",
  "editorMode": "float",
  "picker": "fzf",
  "pickerMode": "float"
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
| `--editor` | `URI_AGENT_EDITOR`, `VISUAL`, `EDITOR` | Set the external editor command |
| `--editor-mode` | `URI_AGENT_EDITOR_MODE` | Use `float` or `fullscreen` editor integration |
| `--picker` | `URI_AGENT_PICKER` | Set the conversation fuzzy-picker command |
| `--picker-mode` | `URI_AGENT_PICKER_MODE` | Use `float` or `fullscreen` picker integration |
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

The append-only event stream retains original model messages after context compaction. When the estimated replay approaches the active model's pi catalog `contextWindow`, uri-agent asks the model for a durable summary, writes a compaction checkpoint to SQLite, and replays that summary plus complete recent user turns. Tool calls and their results are never split across the checkpoint boundary. Run `:compact` from the command line or command palette to create a checkpoint early; manual compaction requires at least one completed older turn.

The canonical launch directory is the project boundary. A normal launch creates a new session for that project. `--continue-session` resumes the most recently updated session whose stored directory matches the current project, and `--session <id>` rejects sessions belonging to another project. There is no cross-project session overview or directory picker in the TUI.

## TUI

The Ratatui interface separates event browsing, draft editing, and detailed inspection. The conversation surface shows one selectable preview per user message, response, reasoning segment, or tool call. A tool call and its result share one row, so streaming reasoning and large tool output never force the useful conversation off-screen. In Browse with no draft, `Enter` opens the complete selected event; `o` always opens it and `e` uses the configured editor. The same lists support wheel scrolling and mouse selection; double-click an event to inspect it.

An empty session opens with a compact ordered-dither pixel mark, the project and model, and a few initial entry points. The Helix-inspired one-line status bar shows only the active mode, model, selected event position, and information that currently requires attention: a retained draft, temporary notice, or active reasoning/tool/compaction state. It does not keep shortcut instructions or estimated counters on screen. The dither shimmer and in-progress activity wave are deterministic, low-noise animations that never change the layout; narrow terminals fall back to a compact welcome.

| Mode | Default keys | Action |
| --- | --- | --- |
| Browse | `↑/↓`, `Enter`, `o`, `i`, `e`, `/`, `y`, `Space`, `:`, mouse | Select previews; `Enter` submits a draft or opens detail when none exists |
| Insert | `Enter`, `Ctrl+E`, `Esc` | Add a line, edit the draft externally, or preserve it and return to Browse |
| Detail | `↑/↓`, `PageUp/PageDown`, `e`, `Esc`, drag, wheel | Inspect, select, copy, or externally view complete content |
| Embedded terminal | normal program keys, double `Esc`, Shift-drag | Operate the editor/picker, close its PTY, or select terminal text |
| Global | `F1`, `F2`, `F3`, `Ctrl+P`, `Ctrl+T`, `Ctrl+Shift+C`, `Ctrl+C` | Help, Settings, model picker, protocols, tasks, copy, and quit |

Browse mode follows the small useful part of Helix's interaction model rather than requiring Vim knowledge: arrows and mouse are first-class, `j/k` remain aliases, `Space` opens a clickable command panel, and `:` opens a command line. Insert is exclusively for editing: `Enter` inserts a newline, while `Esc` preserves the draft and returns to Browse; `Enter` from Browse then submits it. `/` opens the global conversation finder. Commands include `:settings`, `:model`, `:login`, `:find`, `:copy`, `:tasks`, `:protocols`, `:compact`, `:compose`, `:detail`, `:editor`, `:help`, and `:quit`. Registered plugin commands appear in the same palette and help panel. `F1` renders the active keymap on demand rather than keeping shortcut hints on screen.

Read-only floats support direct mouse drag selection. Use Shift-drag in interactive panels and embedded terminals so normal clicks still reach the application. Press `y` or `Ctrl+Shift+C` to copy the selection through OSC52; with no selection, the same action copies the visible panel.

### Rhai keymaps

Key mappings load in this order:

```text
built-in defaults
< <config-dir>/keymap.rhai
< <project>/.uri-agent/keymap.rhai
```

Each Rhai script calls `map(mode, key, action)` or `unmap(mode, key)`. Key names use forms such as `enter`, `space`, `shift+g`, and `ctrl+e`:

```rhai
map("browse", "x", "detail");
unmap("browse", "e");
map("insert", "ctrl+j", "newline");
```

Modes are `global`, `browse`, `insert`, `detail`, `list`, `tasks`, `models`, `settings`, `palette`, `command`, `text`, `selection`, and `terminal`. Available actions are the names shown by `F1`, including `next`, `previous`, `finder`, `copy`, `insert`, `submit`, `detail`, `editor`, `palette`, `command`, `newline`, `model`, `settings`, `protocols`, `tasks`, `escape`, `close`, and `quit`. A registered command ID is also a stable action ID, so `map("browse", "c", "compact")` or a plugin command ID can be bound directly. Scripts are limited to 100,000 Rhai operations and receive no host filesystem or process APIs.

### External editor and finder

[Helix](https://github.com/helix-editor/helix) is the default external editor; its executable is `hx`. It is optional—the rest of the TUI remains usable when it is absent, and URI Agent reports how to change the editor instead of exiting. Install it through the [official Helix installation instructions](https://docs.helix-editor.com/install.html), for example:

```bash
# macOS
brew install helix

# Arch Linux
sudo pacman -S helix
```

[fzf](https://github.com/junegunn/fzf) is the default conversation finder. Install it to use `/`, `:find`, or the command-panel action:

```bash
# Debian/Ubuntu
sudo apt install fzf

# macOS
brew install fzf
```

Both programs default to real PTYs rendered inside Ratatui floats. `editorMode` and `pickerMode` can be changed to `fullscreen` in Settings or `settings.json` when a program should temporarily take over the terminal instead. URI Agent restores raw mode, mouse capture, and bracketed paste after a fullscreen command returns. Set the commands in Settings, or override them with `EDITOR`/`VISUAL`/`URI_AGENT_EDITOR` and `URI_AGENT_PICKER`. GUI editors should include a wait option, such as `code --wait`, and use fullscreen mode.

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

The built-in file and shell protocols are not a sandbox. The agent has the filesystem and command permissions of the uri-agent process, including access to absolute paths. Run it only in trusted projects and with credentials the agent may use. Treat `auth.json`, `models.json`, project settings, Rhai keymaps, editor commands, and discovered Skills as trusted input.

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
