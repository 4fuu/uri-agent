# Configuration

UAT combines the pi model catalog, layered settings, provider credentials, and
project MCP configuration. Run `uat --help` for the generated command-line
reference. There is no in-app settings panel.

## Precedence

For a normal setting:

1. command-line flag;
2. environment variable;
3. project `.uat/settings.json`;
4. global `<config>/settings.json`;
5. built-in default.

Project settings live in the startup working directory. They apply only to
sessions from that project.

## Files

| Path | Purpose |
| --- | --- |
| `<config>/settings.json` | Global model and compaction settings |
| `<project>/.uat/settings.json` | Project overrides |
| `<config>/auth.json` | Stored provider credentials |
| `<config>/environment.json` | Agent Environment values available to MCP servers |
| `<config>/mcp.json` | Global MCP servers |
| `<project>/.agents/mcp.json` | Project MCP servers |
| `<config>/models.json` | Custom provider and model definitions |
| `<project>/AGENTS.md` | Project instructions, frozen into new sessions |

`<config>` defaults to `~/.config/uat` on Linux and macOS. `UAT_CONFIG_DIR`
overrides it. On Windows the conventional application config directory is used.
The session database is stored in the same config directory.

## Settings

```json
{
  "defaultProvider": "anthropic",
  "defaultModel": "claude-sonnet-4-5",
  "defaultThinkingLevel": "high",
  "modelThinkingLevels": {
    "openai/gpt-5.2": "medium"
  },
  "compaction": {
    "enabled": true,
    "reserveTokens": 16384,
    "keepRecentTokens": 20000
  }
}
```

| Key | Meaning | Default |
| --- | --- | --- |
| `defaultProvider` | Provider selected at startup | unset |
| `defaultModel` | Default model for that provider | unset |
| `defaultThinkingLevel` | Fallback reasoning effort | `off` |
| `modelThinkingLevels` | Per-model effort keyed by `provider/model` | `{}` |
| `compaction.enabled` | Summarize context automatically | `true` |
| `compaction.reserveTokens` | Tokens reserved before a checkpoint | `16384` |
| `compaction.keepRecentTokens` | Approximate recent history retained | `20000` |

Unknown fields are preserved when UAT rewrites a settings file.

## Environment variables

| Variable | Effect |
| --- | --- |
| `UAT_PROVIDER` | Provider for this process |
| `UAT_MODEL` | Model for this process |
| `UAT_API_KEY` | API key for every provider |
| `UAT_THINKING` | `off`, `minimal`, `low`, `medium`, `high`, `xhigh`, or `max` |
| `UAT_CONFIG_DIR` | Override the configuration directory |
| `UAT_OFFLINE` | Disable catalog networking |
| `UAT_HTTP_TOKEN` | Bearer token required by `uat serve` |
| `PI_OFFLINE` | Compatibility alias for offline mode |

Provider-specific credentials are read from conventional variables, such as
`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GEMINI_API_KEY`, `DEEPSEEK_API_KEY`, and
`COPILOT_GITHUB_TOKEN`. `uat --help` prints the full provider table for this
build.

## Choosing a model

```bash
uat models
uat -x "review the diff" --model anthropic/claude-sonnet-4-5
uat --provider openai --model gpt-5.2
```

`uat models` prints runnable `<provider>/<id>` values. With no configured model,
`-x` and the REPL fail with a message rather than guessing a default.

## Credentials

The simplest approach for scripts is the provider environment variable or
`--api-key`. API keys can also be stored in `<config>/auth.json`:

```json
{
  "deepseek": {"type": "api_key", "key": "..."}
}
```

`--api-key` applies only to the current process and is never written to
`auth.json`.

OAuth refresh data may appear in `auth.json` for providers that support OAuth,
but the simple mode has no login command. Supply a token through the provider's
documented environment variable or use an API-key provider when configuring a
headless runner.

## Offline and catalog behavior

Unless `--offline` or `UAT_OFFLINE=1` is set, UAT refreshes stale catalog data
from pi.dev and supported providers. A failed refresh falls back to the cached
catalog with a warning. Model access still depends on the selected provider's
credentials and entitlements.

Custom providers can be defined in `<config>/models.json`. See the generated
`uat --help` output and the pi model catalog format for the accepted fields.

## MCP

Global and project MCP files use the same shape:

```json
{
  "servers": {
    "github": {
      "description": "GitHub tools for this repository",
      "transport": "stdio",
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-github"],
      "environment": {"GITHUB_TOKEN": "GITHUB_TOKEN"}
    },
    "docs": {
      "description": "Documentation server",
      "transport": "streamable-http",
      "url": "https://mcp.example.com/mcp",
      "headers": {"Authorization": "Bearer ${DOCS_TOKEN}"}
    }
  }
}
```

Project entries override global entries with the same name. A server requires a
non-empty `description`. `enabled: false` disables it.

For stdio servers, `environment` maps a child-process variable to an Agent
Environment name. Define values in `<config>/environment.json`:

```json
{
  "GITHUB_TOKEN": "github_pat_..."
}
```

For Streamable HTTP servers, headers may reference Agent Environment with
`${NAME}`. Credential-bearing headers must use such a reference; literal secrets
in MCP config are rejected.

Edit these files directly. The `mcp` tool is read-only with respect to server
configuration; it lists and calls configured servers.

## Compaction

UAT estimates request size from the frozen system prompt, history, and tool
schemas. When usage approaches the model window, it asks the model for a
checkpoint summary and replaces older history while retaining recent turns.
`notes` remain available after a checkpoint. Scripts rely on automatic
checkpoints and durable notes.

## Security

UAT is not a sandbox. Tool and MCP processes inherit the authority of the `uat`
process. Keep configuration and environment files private, and review MCP
commands before enabling them.
