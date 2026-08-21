# URI Agent

[English](README.md) | [简体中文](README.zh-CN.md)

[![License: MIT](https://img.shields.io/badge/license-MIT-6ed2c2.svg)](LICENSE)

URI Agent is a terminal coding agent built around a small, stable model interface. The model always sees two tools—`read` and `exec`—and reaches files, shells, file modifications, Skills, and future integrations through protocol addresses such as `file://...` and `bash://...`.

This design keeps tool schemas out of the model context until they are useful. Every protocol documents itself at `<protocol>://help`, long-running work is represented by managed tasks, and oversized output remains available through a `file://` address instead of being discarded.

> [!WARNING]
> URI Agent is not a sandbox. Its file and shell protocols run with the permissions of the `uri-agent` process. Use it only in projects and environments you trust.

## Quick start

### Requirements

- a stable Rust toolchain and Git;
- an API key for a supported provider;
- a terminal with standard keyboard input. Mouse support is optional.

### Install from source

```bash
git clone https://github.com/4fuu/uri-agent.git
cd uri-agent
cargo install --path .
```

### Start a session

URI Agent does not pick a default model. Launch it in the project you want the agent to work in, then run `:login` and `:model`:

```bash
uri-agent --cwd /path/to/project
```

If nothing is configured, the launch view shows `尚未配置，请运行 :login`. `:login` saves an API key or completes Anthropic OAuth; `:model` selects from the runnable catalog.

To send the first request:

1. Press `i` to open the composer float.
2. Type the request. `Shift+Enter` inserts a newline.
3. Press `Enter` to send. `Esc` keeps the draft in SQLite.

Press `:` for commands, or `F1` for the active keymap and command reference.

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
| `replace` | `read`, `exec` | Atomically replace one exact text match |
| `apply_patch` | `read`, `exec` | Apply Codex-style add, delete, update, and move patches |
| `bash` | `read`, `exec` | Run Bash commands as managed tasks when Bash is installed |
| `pwsh` | `read`, `exec` | Run PowerShell 7 commands as managed tasks when `pwsh` is installed |
| `<name>-skill` | `read` | Load one discovered Skill and its bundled resources |

`bash` and `pwsh` are detected at startup and registered only when their executables are available.

`replace` requires a nonempty `old_text` that occurs exactly once. `apply_patch` accepts a patch string enclosed by `*** Begin Patch` and `*** End Patch`; read `apply_patch://help` for the Codex-style file-operation and hunk grammar.

### Managed tasks and complete output

Execution is asynchronous by default. A shell or file-modification request normally returns a task address immediately:

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

URI Agent uses the [pi](https://github.com/earendil-works/pi) model catalog and currently runs these API families through its Rust/Rig backend:

- `openai-responses`
- `openai-completions`
- `anthropic-messages`
- `google-generative-ai`

The model picker shows only runnable API families. `:login` matches Pi Agent: API keys, plus OAuth for Anthropic, OpenRouter, OpenAI Codex, GitHub Copilot, Kimi Code, xAI, and Radius. Stored credentials use the same `auth.json` shape (`type: "api_key"` or `type: "oauth"`).

The catalog is cached for four hours. Use `--offline`, `URI_AGENT_OFFLINE=1`, or `PI_OFFLINE=1` to disable catalog requests and use only local data. `Ctrl+R` refreshes the catalog from the model picker or Settings.

Downloaded model records are cached without dropping fields, including future fields URI Agent does not yet interpret. The active backend applies catalog limits and tiered pricing, input modalities, `reasoning` and `thinkingLevelMap`, `samplingParams`, and request-relevant `compat` settings such as `maxTokensField`, `forceAdaptiveThinking`, role/tool strictness, and provider-specific thinking formats.

Thinking defaults to `off`. Run `:effort` to show the active model's value and supported levels, or `:effort high` to change it. The command and the Thinking row in `:settings` persist per-model defaults in `modelThinkingLevels`, keyed by `provider/model`; switching models restores the matching value. `defaultThinkingLevel` is the file-level fallback, while `--thinking <LEVEL>` and `URI_AGENT_THINKING` override it for the current invocation. Levels are `off`, `minimal`, `low`, `medium`, `high`, `xhigh`, and `max`.

For a model whose catalog `input` includes `image`, mention a project-relative PNG, JPEG, GIF, or WebP file as a standalone `@path` in the composer, for example `Describe @screenshots/error.png`. The image is sent as binary multimodal content rather than prompt text. Attachments cannot escape the project boundary, and text-only models reject them explicitly.

## Configuration

Press `:settings` to inspect the active provider, model, credential status, thinking level, and output limit. Use `:login` / `:logout` for credentials and `:model` to change models. Changes apply to the current session immediately.

On Linux, the default config directory is `~/.config/uri-agent`; set `URI_AGENT_CONFIG_DIR` to use another location.

| File | Purpose |
| --- | --- |
| `settings.json` | Global provider, model, thinking, output, and terminal settings |
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
--thinking <LEVEL>       Set model reasoning effort (default: off)
--output-limit <BYTES>   Set inline output size (minimum 1024)
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

Startup shows a short splash, then a single conversation surface. An empty conversation keeps the centered animated mark and shows only the working directory and active provider/model beneath it, followed by a locally centered compose/command/help key hint. Once records appear, folded history keeps the highlighted one-line URI status bar; click it, press `F4`, or run `:status` to open the detailed project/session/usage view. Opening the composer pauses UI animation and places the terminal cursor at the text caret so IME candidate windows remain anchored correctly.

| Surface | Useful defaults |
| --- | --- |
| Conversation | `i` compose, `:` commands, `?` help, `Enter` open/fold, `r`/`t`/`h` jump thinking/tools/you |
| Composer | `Enter` send, `Shift+Enter` newline, `Esc` keep draft |
| Command | type to filter, `Tab` complete, `Enter` run, `Esc` close |
| Global | `F1` help, `F2` settings, `F3` models, `F4` status, `Ctrl+C` quit |

Useful colon commands: `:login`, `:logout`, `:model`, `:effort`, `:status`, `:resume`, `:new`, `:set-terminal`, `:terminal`, `:compact`, `:help`, `:q`.

`:set-terminal` saves the command for the floating terminal (`pwsh`, `bash`, …). `:terminal` opens it as a PTY float; double `Esc` closes it. Shift-drag selects text so ordinary clicks still reach the program.

Arrow keys and mouse input are first-class; `j` and `k` are optional aliases. Read-only views support mouse selection and OSC52 copy.

The keymap is layered from built-in defaults through global and project `keymap.rhai` files. Each file can map or remove an action:

```rhai
map("main", "x", "copy");
unmap("main", "j");
map("composer", "ctrl+j", "newline");
```

## Extending URI Agent

Rust extensions declare their protocol descriptors and register protocols, commands, generic TUI panel providers, and status providers through [`PluginRegistry`](src/plugin.rs) and `PluginHost`. `host.tui.register_status(...)` accepts a fast, non-blocking provider that returns a `TuiStatusItem`. The provider receives `TuiStatusContext`; use its `expanded` field to return more detail for the bottom panel. Semantic tones keep color rendering in the generic TUI. The first-party `file`, `replace`, `apply_patch`, `bash`, and `pwsh` capabilities use this same plugin path rather than being assembled directly by the application. Protocols remain behind `read` and `exec`; commands join the command palette, colon command line, and keymap action registry. URI Agent does not currently load native dynamic libraries, so third-party Rust extensions must be linked during application assembly.

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
