<p align="center">
  <strong>UAT</strong>
</p>

# UAT

[English](README.md) | [简体中文](README.zh-CN.md)

[![License: MIT](https://img.shields.io/badge/license-MIT-6ed2c2.svg)](LICENSE)

UAT is a small, text-only coding agent designed to be composed by scripts. It
provides ordinary tools, a Rhai codemode for combining them, MCP access, durable
notes, code search, and an HTTP ingress. Inputs and outputs are plain text; UAT
does not write final answers to files on its own.

> [!NOTE]
> The previous URI/protocol implementation is preserved on the
> [`legacy/uri-protocol`](https://github.com/4fuu/uri-agent/tree/legacy/uri-protocol)
> branch. Its last version is `2026.1030.0`. The current `main` branch has no
> URI/protocol tool form.

## Quick start

Requirements: stable Rust, and a model provider credential. Search also uses a
bundled or downloaded `rg`.

```bash
# Release archives bundle the native retrieval library. For a source build,
# stage it first (see docs/development.md#native-retrieval-assets).
cargo build --release --bin uat

export DEEPSEEK_API_KEY=...
./target/release/uat -x "summarize this repository"
```

`uat -x "<prompt>"` runs one turn, prints only the final assistant reply to
stdout, writes progress to stderr, and exits. Piped stdin is appended to the
prompt:

```bash
cat error.log | uat -x "explain this failure"
```

Run `uat` without a prompt for the line-oriented REPL, or resume a session:

```bash
uat --continue-session
uat --session <session-id>
```

## Tools

UAT exposes a deliberately small model-facing surface:

| Tool | Purpose |
| --- | --- |
| `read` | Read a text file with optional offset and line limit |
| `write` | Write a file atomically |
| `edit` | Replace one exact text occurrence |
| `bash` | Run a shell command in the project; returns output and exit code |
| `search` | Exact ripgrep search, or semantic/hybrid code search |
| `notes` | List, read, add, replace, and delete durable session notes |
| `steer` | List active sessions, name this session, or send another session a message |
| `mcp` | List and call tools, prompts, and resources on configured MCP servers |
| `apply_patch` | Apply one transactional multi-file Codex-style patch |
| `codemode` | Run a Rhai script that calls the other tools |

`codemode` keeps intermediate tool output inside the script. Only values passed
to `text(...)` or returned by the script enter the model conversation.

```rhai
let files = bash("rg -n 'TODO' src");
let notes = notes_list();
text(files);
text(notes);
```

## HTTP interface

`uat serve` exposes the same runtime over HTTP so another process, language, or
shell can compose it:

```bash
UAT_HTTP_TOKEN=change-me uat serve --listen 127.0.0.1:8737
```

```bash
# Run a prompt and wait for the reply.
curl -s http://127.0.0.1:8737/v1/messages \
  -H 'Authorization: Bearer change-me' \
  -H 'content-type: application/json' \
  -d '{"text":"review the current diff"}'

# Start work without waiting, then poll the session.
curl -s http://127.0.0.1:8737/v1/messages \
  -H 'Authorization: Bearer change-me' \
  -H 'content-type: application/json' \
  -d '{"text":"inspect the tests","wait":false}'

curl -s http://127.0.0.1:8737/v1/sessions/<session-id> \
  -H 'Authorization: Bearer change-me'

# Inject steering input into an active turn.
curl -s http://127.0.0.1:8737/v1/messages \
  -H 'Authorization: Bearer change-me' \
  -H 'content-type: application/json' \
  -d '{"session":"<session-id>","text":"also check Windows paths","mode":"steer"}'
```

Endpoints: `GET /v1/health`, `GET /v1/sessions`,
`GET /v1/sessions/{id}`, `POST /v1/messages`, and
`POST /v1/sessions/{id}/cancel`. A non-loopback bind requires
`UAT_HTTP_TOKEN`; protect a loopback server too if other local users are not
trusted.

## Sessions and context

Sessions are append-only SQLite records. A session freezes its system prompt
and project instructions at creation, preserves pending input across restarts,
and checkpoints the context with summaries near the model's limit. `notes` is
the durable working record; `search` covers the live workspace.

`steer` coordinates already-running UAT processes. Use `delivery: "steer"` for
the next model boundary or `delivery: "queue"` for a later turn. A successful
send means the target accepted the message durably, not that it has finished
the work.

## MCP

Configure servers in `<config>/mcp.json` globally or `.agents/mcp.json` per
project. The file shape is:

```json
{
  "servers": {
    "github": {
      "description": "GitHub tools for this repository",
      "transport": "stdio",
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-github"],
      "environment": {"GITHUB_TOKEN": "GITHUB_TOKEN"}
    }
  }
}
```

The `mcp` tool then lists server tools and calls them. MCP content is marked as
untrusted reference data.

## Configuration

UAT reads `<config>/settings.json`, project `.uat/settings.json`, environment
variables, and command-line flags. The default config directory is
`~/.config/uat` on Linux/macOS (use `UAT_CONFIG_DIR` to override). API keys may
come from provider environment variables, `--api-key`, or `<config>/auth.json`.
Without the URI protocol layer there is no in-app settings panel; these files
and `uat --help` are the configuration surface.

See [documentation](docs/README.md) for details:

- [Tools and codemode](docs/tools.md)
- [Configuration](docs/configuration.md)
- [Sessions and HTTP](docs/sessions.md)
- [Project context](docs/context.md)
- [Development](docs/development.md)

## Build notes

Release archives include the zvec native library, embedding model, and
dictionaries. A Cargo-installed binary cannot include them, so source builds
must stage those assets and set the platform library path. See
[development](docs/development.md#native-retrieval-assets).

## Security

UAT is not a sandbox. File, shell, and MCP tools run with the authority of the
`uat` process. Only run it on projects and with MCP servers you trust. Peer
messages and MCP content are untrusted data, not authorization.

UAT is an early release and may change between dated versions. Model requests
and their context are sent to the provider you select. Unless offline mode is
enabled, UAT fetches model catalog metadata from pi.dev and supported providers.
