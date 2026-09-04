# Models and configuration

URI Agent combines the [pi model catalog](https://github.com/earendil-works/pi),
provider discovery, local model definitions, layered settings, and process
overrides. Run `uri-agent --help` for the exact CLI, and use `:settings` to see
each effective value and its source.

## First-time setup

URI Agent does not choose a default provider or model:

1. Run `:login` to save an API key or complete an available OAuth flow.
2. Run `:model` and select a runnable model.
3. Use `:settings` to inspect or change model and Agent values.

The model selector shows only providers with a usable credential source.
`:logout` removes a stored credential; if that was the current provider's last
source, URI Agent also clears its saved default and the current session model.

## Model catalog

URI Agent merges pi.dev records, supported provider model-list APIs, built-in
provider records, and local `models.json`. The runtime supports these pi API
families:

- `openai-responses`
- `openai-codex-responses`
- `openai-completions`
- `anthropic-messages`
- `google-generative-ai`

The root [README](../README.md#model-and-provider-coverage) publishes the dated
catalog coverage visible to prospective users. A catalog entry does not
guarantee account access; credentials, subscription, region, and provider
entitlements still apply.

Provider discovery supplements the shared catalog with models available to the
current credential. Results are cached per credential so switching accounts
does not expose another account's discovery results. Pi records win when the
shared catalog later adds the same provider and model ID. Discovered-only
records inherit conservative compatibility metadata and omit unverified prices.

Startup loads cached data immediately and refreshes eligible sources in the
background. Use `:refresh-catalog` or refresh from Model Hub when an immediate
update is needed. Offline mode disables catalog networking while retaining
local files and matching cached results:

```text
uri-agent --offline
URI_AGENT_OFFLINE=1 uri-agent
PI_OFFLINE=1 uri-agent
```

### Provider-specific setup

**Abliteration.ai.** Use provider ID `abliteration`, sign in through `:login`,
or set `ABLITERATION_API_KEY`; `ABLIT_KEY` is a lower-priority compatibility
alias. Built-in fallback records remain available when live model discovery
fails.

**ChatGPT Codex.** Models using `openai-codex-responses` require the
`openai-codex` OAuth entry created by browser or device-code login. An OpenAI
Platform API key cannot authenticate the subscription endpoint. WebSocket and
SSE transport recovery is automatic.

**Cloudflare AI Gateway.** Run `:login` and supply the Cloudflare token, account
ID, and gateway ID. A blank gateway ID uses `default`. URI Agent constructs the
gateway endpoint locally and ignores catalog transport and authentication
fields for this provider so a catalog change cannot redirect the Cloudflare
token. It supports catalog models using OpenAI Responses, OpenAI Completions,
and Anthropic Messages. Process values are also accepted:

```bash
export CLOUDFLARE_API_TOKEN='<cloudflare-api-token>'
export CLOUDFLARE_ACCOUNT_ID='<cloudflare-account-id>'
export CLOUDFLARE_GATEWAY_ID='<gateway-id>' # optional
```

`CLOUDFLARE_API_KEY` remains a lower-priority token alias. Generic API-key
overrides can replace the token but do not supply the required account or
gateway metadata.

**WorkBuddy.** Provider ID `workbuddy` supports the WorkBuddy China browser
flow at `https://copilot.tencent.com`; international, Tencent-internal iOA, and
custom browser-login domains are not supported. `codebuddy` is not a provider
alias. WorkBuddy discovers account models from its product configuration when
available; otherwise selectable models must already exist in pi.dev, cache, or
`models.json`.

Reference-client environment names remain supported:

```bash
export CODEBUDDY_INTERNET_ENVIRONMENT='internal'
export CODEBUDDY_BASE_URL='https://custom.example'
export CODEBUDDY_API_KEY='<workbuddy-api-key>'
export CODEBUDDY_AUTH_TOKEN='<custom-bearer-token>'
export CODEBUDDY_REMOTE_CONFIG_DISABLED='true'
```

`CODEBUDDY_BASE_URL` changes model and cloud-configuration endpoints, not the
browser-login or OAuth refresh endpoints. Credential priority is:

```text
auth.json
< CODEBUDDY_API_KEY
< models.json apiKey
< CODEBUDDY_AUTH_TOKEN
< URI_AGENT_API_KEY
< --api-key
```

**Experimental Antigravity.**

> [!WARNING]
> This integration uses undocumented Google Antigravity OAuth and Cloud Code
> endpoints. It may change without notice, conflict with provider terms, or
> trigger account restrictions. Do not treat it as a stable production path.

Run `:login` and choose **Google Antigravity**. Only the resulting OAuth
credential is accepted; API-key sources cannot replace it. URI Agent includes
an extracted client identity for experimentation. These process variables can
override it when required:

```bash
export ANTIGRAVITY_OAUTH_CLIENT_ID='<google-oauth-client-id>'
export ANTIGRAVITY_OAUTH_CLIENT_SECRET='<google-oauth-client-secret>'
export ANTIGRAVITY_USER_AGENT='<complete-antigravity-user-agent>'
export ANTIGRAVITY_USER_AGENT_VERSION='<version>'
```

Values saved through `:set-env` configure Agent commands, not URI Agent's own
OAuth process. New logins bind refresh to their issuing client ID. Set
`ANTIGRAVITY_IDENTITY_PROMPT` before launch only when an experiment requires a
custom identity prefix.

## Authentication and credential precedence

`:login` accepts API keys and supports these provider-specific flows:

| Provider ID | Login |
| --- | --- |
| `abliteration` | API key |
| `cloudflare-ai-gateway` | API token, account ID, and gateway ID |
| `antigravity` | Experimental Google browser OAuth |
| `anthropic` | Claude Pro/Max browser OAuth |
| `workbuddy` | WorkBuddy China browser login |
| `openrouter` | Browser PKCE |
| `openai-codex` | Browser or device-code login |
| `github-copilot` | Device-code login, with optional Enterprise domain |
| `kimi-coding` | Subscription device-code login |
| `xai` | SuperGrok or X Premium device-code login |
| `radius` | Browser or device-code login |
| `parallel`, `exa`, `tinyfish` | Web search and extraction API key |

OAuth refresh data and API keys are stored in `auth.json`. On Unix, URI Agent
keeps that file owner-only. Filesystem permissions are the protection boundary;
stored credentials are not encrypted.

For ordinary model providers, credential priority is:

```text
models.json apiKey
< auth.json
< provider environment variable
< URI_AGENT_API_KEY
< --api-key
```

The CLI key is process-only. Known providers use conventional variables such as
`OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `GEMINI_API_KEY`, `OPENROUTER_API_KEY`,
and `GROQ_API_KEY`. Web-provider credentials are independent of the active
model and use `PARALLEL_API_KEY`, `EXA_API_KEY`, or `TINYFISH_API_KEY` above a
stored web credential.

## Configuration locations

The default configuration directory is `~/.config/uri-agent` on macOS and
Linux, and normally `%AppData%\uri-agent` on Windows. Set
`URI_AGENT_CONFIG_DIR` to replace it. Older macOS installations under
`~/Library/Application Support/uri-agent` are migrated automatically while
files already present in the new location are kept.

| Path | Purpose |
| --- | --- |
| `<config>/settings.json` | Global model, Agent, role, plugin, and terminal settings |
| `<config>/auth.json` | Provider credentials |
| `<config>/environment.json` | Agent environment values |
| `<config>/models.json` | Custom providers, models, headers, and overrides |
| `<config>/models-store.json` | Generated catalog and discovery cache |
| `<config>/keymap.rhai` | Global keymap overrides |
| `<config>/mcp.json` | User-scoped MCP servers |
| `<config>/wasm-plugins/` | Trusted WASM modules |
| `<project>/.agents/mcp.json` | Project-scoped MCP servers |
| `<project>/.uri-agent/settings.json` | Project settings |
| `<project>/.uri-agent/keymap.rhai` | Project keymap overrides |

Sessions and complete outputs use platform data and cache locations described
in [Sessions and context](sessions.md) and [Protocols, tasks, and
output](protocols.md#complete-output-and-diagnostics). Semantic indexes under
`<platform-cache-dir>/uri-agent/retrieval/v2/` are disposable and rebuilt or
incrementally refreshed by ranked searches. Retrieval runtime assets are
installed beside the executable.

Configuration writes are atomic and preserve symbolic links at managed file
paths. Invalid or cyclic link chains fail without replacing the original.

## Agent environment

Use `:set-env` to add or replace a variable, or open **Agent environment** from
Settings to list names, update masked values, and remove entries. Names use the
portable form `[A-Za-z_][A-Za-z0-9_]*`.

Values are global, stored as plaintext in private `environment.json`, and
injected into future Agent `bash` and `pwsh` commands. They override inherited
variables with the same name but do not modify URI Agent's process or the
user-controlled `:terminal` PTY. Trusted linked and WASM plugins must explicitly
declare whole-environment access before using the host interface.

## MCP servers

Run `:mcp` to add, edit, test, reconnect, enable, disable, or remove URI Agent's
MCP servers. User definitions live in `<config>/mcp.json`; project definitions
live in `<project>/.agents/mcp.json`. A project entry completely replaces the
same user name.

```json
{
  "servers": {
    "github": {
      "description": "Search and manage GitHub repositories",
      "enabled": true,
      "transport": "stdio",
      "command": "github-mcp-server",
      "args": ["stdio"],
      "environment": {
        "GITHUB_PERSONAL_ACCESS_TOKEN": "GITHUB_TOKEN"
      }
    },
    "postgres": {
      "description": "Read the application database",
      "enabled": true,
      "transport": "streamable-http",
      "url": "https://mcp.example.com/postgres",
      "headers": {
        "Authorization": "Bearer ${POSTGRES_MCP_TOKEN}"
      }
    }
  }
}
```

`stdio` arguments remain an exact string list rather than a shell command.
Environment mappings name values from [Agent environment](#agent-environment).
Streamable HTTP requires HTTPS except for loopback addresses; URLs cannot
contain credentials. Credential-bearing headers must reference Agent
Environment values instead of storing plaintext secrets. OAuth and HTTP+SSE
are not supported.

Enabled server names and descriptions join only new sessions. Existing
sessions retain their protocol set but resolve current transport and credential
references on each connection. An ACP client may instead provide a complete,
session-scoped profile; see [ACP v1](acp.md) and [MCP protocol
behavior](protocols.md#mcp).

## Settings fields and precedence

Global and project settings use camel-case JSON fields:

| Field | Meaning | Default |
| --- | --- | --- |
| `defaultProvider` | Default provider | unset |
| `defaultModel` | Default model for that provider | unset |
| `outputLimit` | Inline tool-result bytes | `32768` |
| `defaultThinkingLevel` | Fallback reasoning effort | `off` |
| `modelThinkingLevels` | Per-model effort by `provider/model` | `{}` |
| `modelRoles` | Named model routes for plugins | `{}` |
| `pluginSettings` | Plugin-owned values grouped by namespace | `{}` |
| `terminal` | Command opened by `:terminal` | unset |
| `keyDisplay` | `auto`, `macos`, or `text` hints | `auto` |
| `compaction.enabled` | Enable automatic checkpoints | `true` |
| `compaction.strategy` | `rollover` or `summary` | `rollover` |
| `compaction.reserveTokens` | Context reserved before checkpointing | `16384` |
| `compaction.keepRecentTokens` | Approximate recent replay kept by summaries | `20000` |

Settings resolve from lowest to highest priority:

```text
built-in default
< global settings.json
< <project>/.uri-agent/settings.json
< environment variable
< command-line flag
```

Process overrides include `URI_AGENT_PROVIDER`, `URI_AGENT_MODEL`,
`URI_AGENT_OUTPUT_LIMIT`, `URI_AGENT_THINKING`, `URI_AGENT_TERMINAL`, and
`URI_AGENT_KEY_DISPLAY`. TUI changes write project settings when that file
already exists; otherwise they write global settings. Process overrides remain
in force and are not replaced by those writes.

Compaction fields merge individually. For small model windows, reserve and
recent-history budgets are capped at one quarter of the context window. Agents
without the `context` protocol and Agents with a legacy compaction callback use
`summary` even when `rollover` is configured. See [Sessions and
context](sessions.md#context-windows-and-checkpoints).

### Thinking effort

Supported values are `off`, `minimal`, `low`, `medium`, `high`, `xhigh`, and
`max`; the active model determines which are selectable. `:effort` persists a
per-model choice in `modelThinkingLevels`. `defaultThinkingLevel` is the file
fallback, while `URI_AGENT_THINKING` and `--thinking` override it for one
invocation.

### Model roles and plugin settings

The built-in `small` role starts unassigned. Use `:model-roles` to assign it or
create custom roles. Global and project assignments are layered by complete
role name:

```json
{
  "modelRoles": {
    "review": {
      "provider": "anthropic",
      "model": "claude-sonnet-4-5",
      "thinking": "high"
    }
  }
}
```

Role names use ASCII letters, digits, `-`, and `_`. Provider and model must
identify a runnable catalog model. When `thinking` is omitted, resolution uses
the model-specific choice and then the default. Plugins resolve roles
dynamically without changing the conversation model; an Agent freezes its
resolved provider, model, and effort after the first durable submission.

`pluginSettings` is a separate, project-overridable JSON namespace for trusted
plugin configuration. It is not a credential store or permission boundary.

## CLI modes

Run `uri-agent --help` for current names, conflicts, and accepted values.
Notable modes are:

- native TUI session selection remains scoped to canonical `--cwd`;
- `--background` runs opted-in resident plugins under external supervision and
  does not daemonize or schedule jobs;
- `--acpv1` serves ACP over stdin/stdout, with project directories supplied by
  the client. See [ACP v1](acp.md).

## Custom providers and dynamic values

Define a local or custom OpenAI-compatible provider in `models.json`:

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

Provider entries may also define headers, compatibility values, authentication
behavior, and model overrides. Only models whose final API belongs to a
supported family are runnable. Reload Settings or refresh the catalog after
editing the file.

API keys and headers support pi-style environment expansion. A value beginning
with `!` executes a shell command when first needed and uses trimmed stdout;
the result is cached for the process.

> [!WARNING]
> A leading `!` executes with URI Agent's permissions. Do not load credential
> or header configuration from an untrusted project.
