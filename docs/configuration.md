# Models and configuration

URI Agent combines the [pi model catalog](https://github.com/earendil-works/pi), live provider model discovery, local provider definitions, layered settings, and process-specific overrides. This document describes model support, authentication, file locations, precedence, and command-line configuration.

Run `uri-agent --help` for the current CLI syntax. In the TUI, `:settings` separates model and Agent values into tabs and shows each value's source or pending state. `:model` and `:model-roles` open the shared Model Hub for conversation models and role assignments.

## First-time setup

URI Agent does not choose a default provider or model. After starting it in a project:

1. Run `:login` to save an API key or complete an available OAuth flow.
2. Run `:model` to select a runnable model.
3. Use `:settings` to inspect the effective values.

`:model` lists only providers with a currently configured credential source.
`:logout` removes a stored credential. If it removes the current provider's
last credential source, URI Agent also clears that provider's saved default and
the current session's model instead of switching providers automatically.
Per-model thinking preferences are retained. Model and credential changes apply
to the current session without restarting the application.

## Model catalog

URI Agent downloads provider and model records from pi.dev, supplements them from supported providers' model-list APIs and built-in provider catalogs, and merges them with the local `models.json`. The Rust/Rig backend currently runs these API families:

- `openai-responses`
- `openai-codex-responses`
- `openai-completions`
- `anthropic-messages`
- `google-generative-ai`

URI Agent also provides a built-in `antigravity` family for the [experimental private protocol](#experimental-antigravity-private-protocol) and authenticated [`workbuddy` cloud discovery](#workbuddy). Neither is part of the pi.dev coverage count.

The model selector shows only models using a supported API family whose provider has a configured credential from `models.json`, `auth.json`, or a recognized provider environment variable. The generic `URI_AGENT_API_KEY` and `--api-key` overrides expose only the current provider rather than every catalog provider. Catalog model records are cached without dropping unknown fields so that future metadata survives a read/write cycle. A catalog entry does not guarantee account entitlement: availability still depends on the selected provider, credentials, region, and subscription.

Live discovery is enabled for 28 of the 35 runnable pi provider IDs:

```text
ant-ling, anthropic, baseten, cerebras, deepseek, google, groq,
huggingface, minimax, minimax-cn, moonshotai, moonshotai-cn, nvidia,
openai, opencode, opencode-go, openrouter, qwen-token-plan,
qwen-token-plan-cn, qwen-token-plan-individual, together, xai, xiaomi,
xiaomi-token-plan-ams, xiaomi-token-plan-cn, xiaomi-token-plan-sgp, zai,
zai-coding-cn
```

URI Agent does not attempt live discovery for `cloudflare-ai-gateway`, `cloudflare-workers-ai`, `fireworks`, `github-copilot`, `kimi-coding`, `openai-codex`, or `vercel-ai-gateway`: those supported backends do not expose a portable model-list route from which URI Agent can safely construct runnable unknown model IDs.

Provider results are additive. A pi model with the same provider and ID always wins once the cloud catalog catches up. For a model known only to the provider, URI Agent conservatively inherits runtime metadata from the nearest compatible pi model and omits price metadata rather than reporting an unverified cost. These provisional records are marked as discovered in the cache.

`openai-codex-responses` targets the ChatGPT Codex subscription endpoint and uses WebSocket streaming by default. URI Agent supplies the account ID from the OAuth access token, stable session headers and prompt cache key, and the Codex Responses request fields required for reasoning and tool calls. Within a session it reuses an idle connection, retains it for up to five idle minutes or 55 minutes total, and—when request options and history still match—continues from `previous_response_id` while sending only newly appended input. A busy connection is never shared between concurrent requests.

WebSocket setup or transport failure before the first provider event falls back to SSE and disables WebSocket for the rest of that session. A failure after an event is returned instead of replaying the request over SSE, which avoids duplicated text or tool calls. An expired `previous_response_id` is retried once with the full input, and a connection-limit response is retried once on a fresh WebSocket. Requests always set `store: false`.

The active backend applies catalog data relevant to requests and accounting, including:

- context windows, output limits, and tiered prices;
- text and image input modalities;
- `reasoning`, `thinkingLevelMap`, and `samplingParams`;
- request-relevant `compat` values such as token-field names, adaptive thinking, strict role or tool handling, and provider-specific thinking formats.

### Cloudflare AI Gateway

`cloudflare-ai-gateway` has an explicit half-dependent catalog boundary. The merged pi.dev and `models.json` catalog remains authoritative for model identity and capability metadata: API family, modalities, context and output limits, reasoning and thinking compatibility, request `compat` fields, and pricing. URI Agent deliberately ignores that provider record's `baseUrl`, `headers`, and `authHeader` fields. This prevents a catalog change from redirecting a Cloudflare account token or presenting it as an upstream OpenAI or Anthropic key.

The dedicated backend follows Cloudflare's [REST API contract](https://developers.cloudflare.com/ai-gateway/usage/rest-api/) and constructs its base locally:

```text
https://api.cloudflare.com/client/v4/accounts/{account_id}/ai/v1
```

It supports catalog families `openai-responses`, `openai-completions`, and `anthropic-messages`, producing `/responses`, `/chat/completions`, and `/messages` requests respectively. Other API families are rejected before network I/O. Every request sends `Authorization: Bearer <Cloudflare API token>` and `cf-aig-gateway-id: <gateway ID>`. Anthropic's normal `x-api-key` header is removed so the Cloudflare account token cannot become an upstream provider credential.

Catalog IDs are converted to Cloudflare REST wire IDs: unqualified OpenAI and Anthropic IDs gain `openai/` or `anthropic/`; `workers-ai/@cf/...` becomes `@cf/...`; already namespaced IDs remain unchanged. Because Cloudflare does not expose a portable complete listing for third-party models, URI Agent continues to get identity and capability records from the merged catalog rather than hard-coding or scraping a second model catalog.

Run `:login` and select `cloudflare-ai-gateway`. The three-step prompt collects the API token, account ID, and gateway ID, then saves all three atomically; cancelling any step saves nothing. The `auth.json` entry remains `type: "api_key"` and stores the routing values as `accountId` and `gatewayId` metadata. A blank gateway ID uses `default`. Existing token-only entries must run `:login` once to add the account and gateway metadata. The same values can be supplied as process environment variables:

```bash
export CLOUDFLARE_API_TOKEN='<cloudflare-api-token>'
export CLOUDFLARE_ACCOUNT_ID='<cloudflare-account-id>'
export CLOUDFLARE_GATEWAY_ID='<gateway-id>' # optional; defaults to default
```

`CLOUDFLARE_API_KEY` remains a lower-precedence compatibility alias for the token. `CLOUDFLARE_API_TOKEN` takes precedence when both are set. Process account and gateway variables override the values saved by `:login`. `URI_AGENT_API_KEY` and `--api-key` can still override the token for the current provider, but they do not supply an account or gateway ID; provide those through `:login` or the Cloudflare-specific variables. A missing account ID fails locally before a provider request.

### Refresh and offline mode

Startup loads the local catalog immediately, then refreshes pi.dev and eligible configured providers in the background after the TUI is available. Pi entries are considered fresh for four hours; provider listings are considered fresh for five minutes. Run `:refresh-catalog`, press `Ctrl+R` in the model selector, or press `r` in Settings to bypass both freshness windows and immediately apply the resulting model configurations. Refreshes are serialized and provider requests run concurrently, so one slow or failing provider does not block cached results from the others.

Live discovery uses each provider's resolved API key or OAuth access token. Providers without a currently usable credential are skipped without a request or warning, including credentials that refer to an unset environment variable. The cache is scoped to a one-way credential fingerprint: raw credentials are never stored in `models-store.json`, and cached models from one account are not exposed after switching credentials. Pi and provider request failures silently retain cached data; catalog probing is opportunistic and never adds errors or warnings to the conversation. Background startup refresh does not execute credentials configured with a leading `!`; an explicit force refresh may execute them through the normal trusted configuration-command path, and discovery is skipped without warning if the command does not produce a usable credential.

Use any of the following to disable all catalog requests and rely on local catalog data:

```text
uri-agent --offline
URI_AGENT_OFFLINE=1 uri-agent
PI_OFFLINE=1 uri-agent
```

Offline mode still loads `models-store.json` and `models.json`, including discovered models cached for the currently resolved credential; it only disables catalog networking.

## Authentication

`:login` accepts API keys and offers OAuth for these provider IDs:

| Provider ID | Login |
| --- | --- |
| `cloudflare-ai-gateway` | Cloudflare API token, account ID, and AI Gateway ID |
| `antigravity` | Experimental Google browser OAuth for the private Antigravity protocol |
| `anthropic` | Claude Pro/Max browser OAuth |
| `workbuddy` | WorkBuddy China browser login through `https://copilot.tencent.com` |
| `openrouter` | Browser PKCE |
| `openai-codex` | Browser or device-code login |
| `github-copilot` | Device-code login, including an optional Enterprise domain |
| `kimi-coding` | Subscription device-code login |
| `xai` | SuperGrok or X Premium device-code login |
| `radius` | Browser or device-code login |
| `parallel` | API key for built-in web search and page extraction |
| `exa` | API key for built-in web search and page extraction |
| `tinyfish` | API key for built-in web search and page extraction |

Stored entries in `auth.json` use `type: "api_key"` or `type: "oauth"`. OAuth entries retain refresh data; URI Agent resolves the active credential before each model request and refreshes it within five minutes of expiry rather than refreshing every stored provider during startup. Credential writes and refreshes use a cross-process transaction lock, so concurrent URI Agent processes re-read and merge the current file instead of overwriting each other's updates. The refreshed access token, new expiry, and rotated refresh token are persisted before the request continues; when a provider omits the refresh token, URI Agent retains the previous one rather than replacing it with an empty value. Kimi refresh retries connection failures, HTTP 429, and server errors up to three times with 1-, 2-, and 4-second delays; authentication failures are not retried. On Unix, URI Agent creates `auth.json` and its lock with mode `0600` and the configuration directory with mode `0700`.

Models using `openai-codex-responses` require the `openai-codex` OAuth entry created by the OpenAI browser or device-code login. An OpenAI Platform API key—including `OPENAI_API_KEY`, `URI_AGENT_API_KEY`, or `--api-key`—is not accepted for this subscription endpoint. Model availability is determined by the signed-in ChatGPT account and its subscription.

### WorkBuddy

The provider ID is `workbuddy`. Run `:login`, choose **WorkBuddy**, then choose **WorkBuddy China**. Login always targets the Chinese WorkBuddy endpoint `https://copilot.tencent.com`; international, Tencent-internal iOA, and custom enterprise-domain browser routes are not offered. URI Agent does not recognize `codebuddy` as an alias: existing `codebuddy` entries in `auth.json` or `models.json` must be removed or recreated under `workbuddy`.

A WorkBuddy login has a five-minute deadline. It creates browser state through `POST /v2/plugin/auth/state?platform=workbuddy`, opens the returned URL with WorkBuddy version `5.3.14`, and polls the token and account routes once per second. Authentication requests use WorkBuddy's `SaaS` product header and Chinese desktop identity `WorkBuddy/5.3.14 WorkBuddy/5.3.14 CLI/2.115.0`. Token code `11217` and account code `12151` mean the browser flow is still pending; account polling retries HTTP 401 or 403 up to five times. The resulting access token, refresh token, endpoint, `internal` network environment, domain, authentication method, and account identity are saved in `auth.json`; access and refresh secrets are not duplicated in metadata. In WorkBuddy's own environment enum, `internal` identifies its Chinese public SaaS domains and is distinct from the unsupported `iOA` environment.

The state and polling requests reuse one HTTP client but deliberately do not enable a cookie store. The inspected WorkBuddy China desktop client uses Axios's Node transport without a cookie jar; the protocol correlates these requests with the returned `state` value.

WorkBuddy refresh sends the old bearer token and `X-Refresh-Token` to `POST /v2/plugin/auth/token/refresh`, then requires a successful `GET /v2/plugin/accounts`. URI Agent saves a rotated refresh token when returned and preserves the current account while enriching it from the returned account snapshot. The account snapshot is optional during initial login, matching the desktop client, so a temporary account-list failure does not discard an otherwise completed browser login. License failures and disallowed-IP failures are returned as login or refresh errors. A request-phase HTTP 401 refreshes a stored OAuth credential and retries once before any streamed event; API keys and custom bearer tokens are never refreshed.

Model requests use OpenAI Chat Completions streaming at `{endpoint}/v2/chat/completions` and always send `Authorization: Bearer ...`, `X-Requested-With: XMLHttpRequest`, and `X-Product: SaaS`. An API key is also sent as `X-API-Key`, matching WorkBuddy. A signed-in account adds WorkBuddy's domain, user, enterprise, department, tenant, authentication-method, identity-source, and base64-encoded `X-Userinfo` headers. If an API key overrides a stored login, the stored account identity is retained; `CODEBUDDY_AUTH_TOKEN` instead supplies a complete custom bearer session without refresh. The WorkBuddy backend owns the endpoint and authentication headers, so catalog and `models.json` transport fields cannot redirect these credentials or replace their identity headers.

WorkBuddy does not expose a generic `/models` API. After credentials are configured, URI Agent sends an authenticated `GET {endpoint}/v3/config` request with the account identity and product headers. It converts runnable chat records from the response's `data.models` and merges them by model ID with the current pi.dev catalog. The cloud record is authoritative for a `workbuddy` model ID; an explicit user `modelOverrides` entry remains the final metadata layer. Personal WorkBuddy accounts may return no `models` array, so this endpoint alone is not guaranteed to supply a model list.

WorkBuddy cloud configuration is cached for eight minutes and scoped to the credential, account identity, and endpoint. Up to 20 account configurations are retained in `models-store.json`. A failed or empty response keeps the last successful configuration for that scope. A new login refreshes the cloud catalog immediately. URI Agent does not currently bundle WorkBuddy's static product model table, so when `/v3/config` omits models, selectable `workbuddy` models must already exist in pi.dev, the matching cache, or `models.json`.

The reference process variables are supported:

```bash
export CODEBUDDY_INTERNET_ENVIRONMENT='internal' # Chinese public SaaS; iOA is unsupported
export CODEBUDDY_BASE_URL='https://custom.example' # model/config endpoint override
export CODEBUDDY_API_KEY='<workbuddy-api-key>'
export CODEBUDDY_AUTH_TOKEN='<custom-bearer-token>'
export CODEBUDDY_REMOTE_CONFIG_DISABLED='true' # optional: do not refresh /v3/config
```

The `CODEBUDDY_*` names above come from the WorkBuddy reference client and remain the supported process-variable contract; they do not create a `codebuddy` provider alias. `CODEBUDDY_BASE_URL` overrides only the cloud-configuration and generation endpoint; it does not add a custom browser-login route or change the refresh endpoint recorded by OAuth. `CODEBUDDY_REMOTE_CONFIG_DISABLED=1|true` disables cloud refresh while leaving a matching cached configuration available. The existing `models.json apiKey` field, including a leading `!command`, is an optional equivalent of WorkBuddy's `apiKeyHelper`. Credential precedence for the `workbuddy` provider from lowest to highest is:

```text
auth.json
< CODEBUDDY_API_KEY
< models.json apiKey
< CODEBUDDY_AUTH_TOKEN
< URI_AGENT_API_KEY
< --api-key
```

### Experimental Antigravity private protocol

> [!WARNING]
> This integration uses undocumented Google Antigravity OAuth and Cloud Code endpoints. It is unsupported, can change without notice, and may conflict with provider terms or trigger account restrictions. Use it only for protocol experiments with an account you can afford to lose after assessing the applicable terms. Do not treat it as a production or stable authentication path.

Like the reference implementation, URI Agent includes the extracted Antigravity OAuth client identity, so `:login` works without environment setup. OAuth requests identify as `vscode/1.X.X (Antigravity/4.3.0)`; generation requests use the corresponding Antigravity 4.3.0 Chrome/Electron fingerprint for the current platform. These unofficial embedded values may stop working or increase the account and provider-terms risks described above. The following process variables remain available as optional overrides:

```bash
export ANTIGRAVITY_OAUTH_CLIENT_ID='<google-oauth-client-id>'
export ANTIGRAVITY_OAUTH_CLIENT_SECRET='<google-oauth-client-secret>'
export ANTIGRAVITY_USER_AGENT='<complete-antigravity-user-agent>'
# Or override the Antigravity version in the default OAuth and generation fingerprints:
export ANTIGRAVITY_USER_AGENT_VERSION='<version>'
uri-agent --cwd /path/to/project
```

Overrides must be process environment variables; values saved through `:set-env` are reserved for Agent commands and do not modify URI Agent's own environment. Run `:login`, choose **Google Antigravity**, and select an `antigravity` model. Login uses Google OAuth with PKCE and the `openid` and Cloud Code scopes, discovers the Cloud AI Companion project through `loadCodeAssist`, and runs `onboardUser` when the account has no project yet. New logins store the OAuth client ID that issued the credential, but never its secret; once that metadata is present, refresh stops with a new-login instruction if the current client ID differs, so it does not send a refresh token to the wrong client. A refresh response reporting `invalid_grant` is confirmed once with the same client after 500 milliseconds before the failure is returned. Control and generation requests try the sandbox, daily, and production Cloud Code endpoints in that order when a transport or retryable server error requires fallback. Only the resulting stored OAuth credential is accepted: `models.json apiKey`, provider API-key variables, `URI_AGENT_API_KEY`, and `--api-key` cannot replace it.

The selector exposes canonical models rather than the private low/medium/high route IDs. The selected effort chooses both the private model and its numeric thinking budget:

| Selector model | Effort routes |
| --- | --- |
| `gemini-3.7-flash` | `low` 1,000; `medium` 4,000; `high` 10,000 |
| `gemini-3.5-flash` | `low` 1,000; `medium` 4,000; `high` 10,000 |
| `gemini-3.1-pro` | `low` 1,001; `medium` or `high` 10,001 |
| `gemini-3.1-flash-lite` | no thinking |
| `claude-sonnet-4-6` | `off`; `low` 8,192; `medium` 16,384; `high` 24,576; `max` 32,768 |
| `claude-opus-4-6` | `low` 8,192; `medium` 16,384; `high` 24,576; `max` 32,768 |

A local `models.json` can overlay individual effort routes when the private service changes:

```json
{
  "providers": {
    "antigravity": {
      "modelOverrides": {
        "gemini-3.7-flash": {
          "compat": {
            "antigravityRoutes": {
              "high": {
                "model": "replacement-private-route",
                "thinkingBudget": 10000,
                "maxOutputTokens": 65536
              }
            }
          }
        }
      }
    }
  }
}
```

Requests use the private `v1internal:streamGenerateContent` SSE operation. URI Agent sends numeric `thinkingBudget` values, preserves and mirrors Gemini thought signatures across tool rounds, normalizes registered tool schemas and `toolConfig`, and adds stable missing Claude tool IDs. The required `read` and `exec` body remains a concrete string schema through normalization. A 401 refreshes the OAuth token once; a project-header 403 retries once without that header. After these transport-specific repairs and endpoint fallbacks are exhausted, failures retain URI Agent's normal model retry classification and budget.

URI Agent does not inject an Antigravity identity prompt by default. Set `ANTIGRAVITY_IDENTITY_PROMPT` before launch only when an experiment explicitly requires a custom prefix.

Known providers use conventional environment variables. Examples include `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `GEMINI_API_KEY`, `OPENROUTER_API_KEY`, and `GROQ_API_KEY`. Anthropic also recognizes `ANTHROPIC_OAUTH_TOKEN` and `ANTHROPIC_AUTH_TOKEN`. Cloudflare AI Gateway's structured variables and compatibility alias are documented [above](#cloudflare-ai-gateway), and WorkBuddy's variables and provider-specific precedence are documented in the [WorkBuddy section](#workbuddy). The built-in `https` protocol recognizes `PARALLEL_API_KEY`, `EXA_API_KEY`, and `TINYFISH_API_KEY` for web search and page extraction.

### Credential precedence

From lowest to highest priority:

```text
models.json apiKey
< auth.json
< provider environment variable
< URI_AGENT_API_KEY
< --api-key
```

The command-line API key is process-only and is not written to `auth.json`.

WorkBuddy follows the provider-specific order in the [WorkBuddy section](#workbuddy), which places the optional `models.json apiKey` helper above its stored credential and `CODEBUDDY_API_KEY` to match the reference client.

Web-provider credentials are independent of the active model. Parallel, Exa,
and TinyFish resolve a key from `auth.json`, overridden by that provider's
process environment variable. `models.json apiKey`, `URI_AGENT_API_KEY`, and
`--api-key` configure model requests and are not web-provider credentials.

## Configuration locations

URI Agent stores global configuration in `~/.config/uri-agent` on macOS and Linux. On Windows this is normally `%AppData%\uri-agent`.

On macOS, earlier releases used `~/Library/Application Support/uri-agent`. If that directory still exists, URI Agent moves its complete contents — including session databases — into `~/.config/uri-agent` before loading settings, then deletes the old directory. Existing files in the new location are kept.

Set `URI_AGENT_CONFIG_DIR` to replace the complete configuration directory path. An explicit override is used as-is and does not migrate files from the previous macOS location.

| Path | Purpose |
| --- | --- |
| `<config>/settings.json` | Global provider, model, model-role, plugin-owned, thinking, output, and terminal settings |
| `<config>/auth.json` | Global provider credentials |
| `<config>/environment.json` | Global environment variables for Agent shell commands and trusted plugins |
| `<config>/models.json` | Custom providers, models, headers, and model overrides |
| `<config>/models-store.json` | Generated pi and credential-scoped provider catalog cache |
| `<config>/keymap.rhai` | Global keymap overrides |
| `<config>/mcp.json` | User-scoped MCP server definitions |
| `<config>/wasm-plugins/` | Trusted WASM modules loaded at startup and on reload |
| `<project>/.agents/mcp.json` | Project-scoped MCP server definitions |
| `<project>/.uri-agent/settings.json` | Optional project settings |
| `<project>/.uri-agent/keymap.rhai` | Optional project keymap overrides |

Sessions live in this configuration directory on macOS after the Application Support cutover. On other platforms, sessions and complete tool outputs use platform data and cache directories rather than this configuration directory. See [Session storage and project boundaries](sessions.md#session-storage-and-project-boundaries) and [Complete output preservation](protocols.md#complete-output-preservation).

The WASM plugin directory follows `URI_AGENT_CONFIG_DIR` but is not a settings field. See [WASM plugins](plugins.md) for loading, installation, and reload behavior.

## Agent environment

Use `:set-env` to add or replace a variable such as `NPM_TOKEN`. The **Agent environment** row in the Agent tab of Settings opens the complete manager: it displays names without values, `Enter` replaces the selected value, `Ctrl+N` adds a variable, and `Delete` removes one. Value prompts mask their input. Names use the portable form `[A-Za-z_][A-Za-z0-9_]*`.

Variables are global rather than project- or session-specific. URI Agent stores them as plain text in the private `environment.json` configuration file; on Unix it enforces mode `0600` and keeps the configuration directory at `0700`. Filesystem permissions are the protection boundary—values are not encrypted.

Each future Agent `bash` or `pwsh` command receives the latest saved values. A saved value overrides an inherited process variable with the same name; other process variables remain inherited normally. The manager does not modify URI Agent's own process environment, and variables are deliberately not injected into the user-controlled `:terminal` PTY.

The dedicated linked Rust and WASM host interface requires an explicitly requested whole-environment capability. The request names no individual variables and has no interactive approval flow; it exists as a sensitive-access marker for source review. See [WASM plugin environment access](plugins.md#agent-environment-access).

## MCP servers

Run `:mcp` to manage URI Agent's own MCP configuration. The panel lists known
status without connecting every server and supports Add, Edit, Test, Reconnect,
Enable/Disable, and Remove. Add defaults to Project scope. Editing can move a
server between User and Project scope; the destination is written before the
source is removed, and a failed removal is rolled back. Existing names are
immutable. The add/edit workflow validates the form, automatically tests the
connection, then shows a review screen; a failed test reports the complete
error but does not prevent saving.

User definitions live in `<config>/mcp.json`; Project definitions live in
`<project>/.agents/mcp.json`. Both files use an independent `servers` object:

```json
{
  "servers": {
    "github": {
      "description": "Search and manage GitHub repositories",
      "enabled": true,
      "transport": "stdio",
      "command": "github-mcp-server",
      "args": ["stdio"],
      "cwd": ".",
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

A Project entry completely replaces the same User name; fields are not merged.
Enable/Disable edits the effective entry, so disabling an effective User entry
changes User scope rather than creating a Project override. Removing a Project
entry removes only that scope and warns when the same User name will become
effective again.

Every server requires a nonempty `description`. `stdio` requires `command` and
keeps `args` as an exact string list rather than splitting a shell command.
`cwd` may be absolute or project-relative and defaults to the project. The
child inherits URI Agent's process environment. Each `environment` entry maps a
child variable name to the name of a global [Agent environment](#agent-environment)
value, which overrides the inherited value; missing references fail when
connecting.

Streamable HTTP requires HTTPS except for loopback HTTP addresses, and URLs
cannot contain username/password credentials. Header templates expand
`${NAME}` from Agent Environment at connection time, and a missing value fails
directly. Credential-bearing headers such as
`Authorization`, `Cookie`, and `X-API-Key` must use such a reference instead of
storing plaintext credentials. MCP OAuth and the deprecated HTTP+SSE transport
are not supported.

New sessions record enabled server identities and required descriptions without
connecting or fully validating transports. Newly saved servers therefore join
only new sessions. Calls from an existing session keep its recorded protocol
set but resolve the latest transport, URL, and Agent Environment values; a
removed or disabled recorded server fails on its next call. See [MCP
protocols](protocols.md#mcp-servers) for routes and argument encoding.

## Settings fields and precedence

`settings.json` and project settings use camel-case JSON fields:

| Field | Meaning | Default |
| --- | --- | --- |
| `defaultProvider` | Provider selected when no higher-priority override exists | unset |
| `defaultModel` | Model selected for that provider | unset |
| `outputLimit` | Maximum bytes returned inline before preserving full output | `32768` |
| `defaultThinkingLevel` | Fallback reasoning effort | `off` |
| `modelThinkingLevels` | Per-model effort keyed by `provider/model` | `{}` |
| `modelRoles` | Named model routes available to linked and WASM plugins | `{}` |
| `pluginSettings` | Plugin-owned JSON key/value settings grouped by namespace | `{}` |
| `terminal` | Command opened by `:terminal` | unset |
| `keyDisplay` | Key-hint style: `auto`, `macos`, or `text` | `auto` |
| `compaction.enabled` | Run threshold and overflow compaction automatically | `true` |
| `compaction.reserveTokens` | Context held back by the automatic-compaction threshold | `16384` |
| `compaction.keepRecentTokens` | Approximate recent replay retained after compaction | `20000` |

Settings are resolved from lowest to highest priority:

```text
built-in default
< global settings.json
< <project>/.uri-agent/settings.json
< environment variable
< command-line flag
```

Relevant environment variables are:

| Variable | Setting |
| --- | --- |
| `URI_AGENT_PROVIDER` | Provider |
| `URI_AGENT_MODEL` | Model |
| `URI_AGENT_OUTPUT_LIMIT` | Inline output bytes |
| `URI_AGENT_THINKING` | Thinking level |
| `URI_AGENT_TERMINAL` | Embedded terminal command |
| `URI_AGENT_KEY_DISPLAY` | Key-hint style |

When the project settings file already exists, changes made through model selection, model-role selection, Settings, `:effort`, and `:set-terminal` are written there. Otherwise they are written to global `settings.json`. Environment and CLI overrides remain in force for the current invocation and are not replaced by those writes.

Compaction fields merge individually across global and project settings. Token values must be greater than zero. For models with small context windows, the effective reserve and recent-history budgets are each capped at one quarter of the model context window. Disabling automatic compaction also disables automatic provider-overflow recovery; `:compact` remains available.

`keyDisplay: "auto"` uses macOS symbols when URI Agent itself runs on macOS and text labels elsewhere. Set it to `"macos"` when a macOS terminal is controlling URI Agent on a remote non-macOS host, or to `"text"` to force labels such as `Ctrl+R` and `Shift+Enter`. The resolved macOS style also adds Command aliases for Settings, paste, undo, and redo without removing the portable Control and Option bindings. A terminal may consume Command shortcuts before URI Agent receives them, so the portable bindings remain available. Keymaps and their display style are loaded when the TUI starts.

```json
{
  "keyDisplay": "macos",
  "compaction": {
    "enabled": true,
    "reserveTokens": 16384,
    "keepRecentTokens": 20000
  }
}
```

## Thinking effort

Thinking defaults to `off`. Supported values are:

```text
off, minimal, low, medium, high, xhigh, max
```

The active model determines which levels are available. Run `:effort` to open a selector containing only supported levels; the current effective level is selected when the panel opens.

`:effort` and the Thinking row in Settings persist the selection in `modelThinkingLevels` under `provider/model`. Switching models restores that model's saved value. `defaultThinkingLevel` is the file-level fallback; `URI_AGENT_THINKING` and `--thinking` override it for the current invocation.

## Model roles for plugins

URI Agent provides one built-in role, `small`, as a semantic route rather than
a hard-coded catalog model. It starts without a model assignment: selecting the
conversation model does not change it. Other roles are custom and exist only
after they are explicitly added.

Use `:model-roles` to open Model Hub's Roles tab. The list shows each role's
assignment, thinking effort, and global or project source; a project override
of a same-named global role is marked in the source column. `Ctrl+N` adds a
custom role without leaving the Hub. `Enter` first selects a runnable model and
then one of that model's supported thinking levels; `Esc` returns one step and
saving returns to the role list. `Delete` asks for confirmation before removing
the explicit assignment and identifies the affected scope. Removing a project
override reveals the same-named global assignment. Unassigning `small` leaves
it in the list; removing the only assignment for a custom role removes it from
the list.

`modelRoles` stores explicit role assignments:

```json
{
  "modelRoles": {
    "review": {
      "provider": "anthropic",
      "model": "claude-sonnet-4-5",
      "thinking": "high"
    },
    "commit": {
      "provider": "openai",
      "model": "gpt-5.2",
      "thinking": "low"
    }
  }
}
```

Role names contain only ASCII letters, digits, `-`, and `_`. A same-named
project role replaces the complete global role. `provider` and `model` are
required and must identify a runnable catalog model. When `thinking` is
omitted, resolution uses that model's `modelThinkingLevels` entry and then
`defaultThinkingLevel`. Lookup is dynamic, returns no credential, and does not
alter the current session.

Plugins store their own settings independently of role assignments. Each
namespace contains arbitrary JSON values keyed by the plugin. For example, the
built-in terminal-title plugin defaults to `small` and stores an explicit role
selection like this:

```json
{
  "pluginSettings": {
    "terminal-title": {
      "role": "review"
    }
  }
}
```

A project value overrides the same global namespace and key without replacing
the plugin's other global keys. Linked plugins choose their namespace through
`PluginHost::settings`; a WASM plugin automatically uses its `.wasm` filename
stem. Keys and namespaces may not contain control characters, `.`, `/`, or
`\\`; each must be nonempty and at most 128 bytes. One encoded value may not
exceed 1 MiB. These values are trusted plugin configuration, not a secret store
or permission boundary.

Linked plugins can register `CommandTarget::ModelRole` to open the generic role
selector and persist the result as one of these string values, or provide their
own settings UI. The built-in `:terminal-title-role` (`:title-role`) command
uses that selector. A plugin decides its own default and missing-role behavior;
terminal-title uses `small` only when the setting is absent and silently skips
generation if the selected role cannot run.

Plugins may resolve a role and use its provider, model, and thinking values to
construct an `AgentSpec`. Agent provider/model and thinking freeze after the
first durably accepted submission, so later role or configuration changes do
not retarget that Agent. See [Agent sessions](sessions.md#agenthost-and-agent-specifications).

## Command-line options

Command-line flags can override the provider, model, process-only API key, project working directory, thinking effort, inline output limit, and offline catalog behavior. They can also resume the latest project session or a project-scoped session ID. Run `uri-agent --help` for the exact names, accepted values, and current syntax.

`--background` omits the TUI and runs opted-in resident plugins while remaining
foreground-blocking for an external supervisor. It does not daemonize, schedule
jobs, or provide a trigger or gateway service. See [Resident plugins](plugins.md#resident-plugins).

`--continue-session` and `--session` conflict. Session selection remains scoped to the canonical `--cwd`; see [Sessions and context](sessions.md).

## Custom providers and model overrides

Add a provider to `models.json` for a local or custom OpenAI-compatible endpoint:

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

Provider entries may also define headers, compatibility values, authentication-header behavior, or model overrides. Local definitions are merged with downloaded catalog records. After editing `models.json`, refresh or reload Settings before selecting the new model.

Only models whose final `api` belongs to a supported API family are runnable.

## Dynamic credential and header values

API keys and configured header values support pi-style environment expansion. They may also begin with `!` to execute a shell command and use its trimmed standard output:

```json
{
  "providers": {
    "example": {
      "apiKey": "!secret-tool read model-key"
    }
  }
}
```

Command values run when the credential or header is first needed, not while URI Agent starts. They time out after 10 seconds and are cached for the process. On Windows they run through PowerShell; on other platforms they run through `sh -c`. Standard input is closed, standard output and error are captured, and unrelated inherited file descriptors are closed on Unix. The command and remaining descendants are terminated when the root command exits or the deadline expires; a timeout is returned only after the root process has been reaped.

> [!WARNING]
> A leading `!` executes with the permissions of URI Agent. Do not load credential or header configuration from an untrusted project.
