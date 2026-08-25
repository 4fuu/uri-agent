# Models and configuration

URI Agent combines the [pi model catalog](https://github.com/earendil-works/pi), local provider definitions, layered settings, and process-specific overrides. This document describes model support, authentication, file locations, precedence, and command-line configuration.

Run `uri-agent --help` for the current CLI syntax. In the TUI, `:settings` shows the active provider and model, credential status and source, thinking level, output limit, and Agent environment manager.

## First-time setup

URI Agent does not choose a default provider or model. After starting it in a project:

1. Run `:login` to save an API key or complete an available OAuth flow.
2. Run `:model` to select a runnable model.
3. Use `:settings` to inspect the effective values.

`:logout` removes a stored credential. Model and credential changes apply to the current session without restarting the application.

## Model catalog

URI Agent downloads provider and model records from pi.dev and merges them with the local `models.json`. The Rust/Rig backend currently runs these API families:

- `openai-responses`
- `openai-codex-responses`
- `openai-completions`
- `anthropic-messages`
- `google-generative-ai`

URI Agent also provides a built-in `antigravity` family for the [experimental private protocol](#experimental-antigravity-private-protocol). It is not part of the pi.dev coverage count.

The model selector shows only models using a supported API family. Catalog model records are cached without dropping unknown fields so that future metadata survives a read/write cycle. A catalog entry does not guarantee account entitlement: availability still depends on the selected provider, credentials, region, and subscription.

`openai-codex-responses` targets the ChatGPT Codex subscription endpoint and uses WebSocket streaming by default. URI Agent supplies the account ID from the OAuth access token, stable session headers and prompt cache key, and the Codex Responses request fields required for reasoning and tool calls. Within a session it reuses an idle connection, retains it for up to five idle minutes or 55 minutes total, and—when request options and history still match—continues from `previous_response_id` while sending only newly appended input. A busy connection is never shared between concurrent requests.

WebSocket setup or transport failure before the first provider event falls back to SSE and disables WebSocket for the rest of that session. A failure after an event is returned instead of replaying the request over SSE, which avoids duplicated text or tool calls. An expired `previous_response_id` is retried once with the full input, and a connection-limit response is retried once on a fresh WebSocket. Requests always set `store: false`.

The active backend applies catalog data relevant to requests and accounting, including:

- context windows, output limits, and tiered prices;
- text and image input modalities;
- `reasoning`, `thinkingLevelMap`, and `samplingParams`;
- request-relevant `compat` values such as token-field names, adaptive thinking, strict role or tool handling, and provider-specific thinking formats.

### Refresh and offline mode

Startup loads the local catalog immediately, then refreshes pi.dev in the background after the TUI is available. Catalog entries are considered fresh for four hours. Run `:refresh-catalog`, press `Ctrl+R` in the model selector, or press `r` in Settings to force a refresh from pi.dev and immediately apply the resulting model configurations.

Use any of the following to disable pi.dev requests and rely on local catalog data:

```text
uri-agent --offline
URI_AGENT_OFFLINE=1 uri-agent
PI_OFFLINE=1 uri-agent
```

Offline mode still loads `models-store.json` and `models.json`; it only disables catalog networking.

## Authentication

`:login` accepts API keys and offers OAuth for these provider IDs:

| Provider ID | Login |
| --- | --- |
| `antigravity` | Experimental Google browser OAuth for the private Antigravity protocol |
| `anthropic` | Claude Pro/Max browser OAuth |
| `openrouter` | Browser PKCE |
| `openai-codex` | Browser or device-code login |
| `github-copilot` | Device-code login, including an optional Enterprise domain |
| `kimi-coding` | Subscription device-code login |
| `xai` | SuperGrok or X Premium device-code login |
| `radius` | Browser or device-code login |
| `parallel` | API key for built-in web search and page extraction |
| `exa` | API key for built-in web search and page extraction |

Stored entries in `auth.json` use `type: "api_key"` or `type: "oauth"`. OAuth entries retain refresh data; URI Agent refreshes an expired entry when its provider is first used rather than refreshing every stored provider during startup. On Unix, URI Agent creates `auth.json` with mode `0600` and the configuration directory with mode `0700`.

Models using `openai-codex-responses` require the `openai-codex` OAuth entry created by the OpenAI browser or device-code login. An OpenAI Platform API key—including `OPENAI_API_KEY`, `URI_AGENT_API_KEY`, or `--api-key`—is not accepted for this subscription endpoint. Model availability is determined by the signed-in ChatGPT account and its subscription.

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

Overrides must be process environment variables; values saved through `:set-env` are reserved for Agent commands and do not modify URI Agent's own environment. Run `:login`, choose **Google Antigravity**, and select an `antigravity` model. Login uses Google OAuth with PKCE and the `openid` and Cloud Code scopes, discovers the Cloud AI Companion project through `loadCodeAssist`, and runs `onboardUser` when the account has no project yet. Control and generation requests try the sandbox, daily, and production Cloud Code endpoints in that order when a transport or retryable server error requires fallback. Only the resulting stored OAuth credential is accepted: `models.json apiKey`, provider API-key variables, `URI_AGENT_API_KEY`, and `--api-key` cannot replace it.

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

Known providers use conventional environment variables. Examples include `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `GEMINI_API_KEY`, `OPENROUTER_API_KEY`, and `GROQ_API_KEY`. Anthropic also recognizes `ANTHROPIC_OAUTH_TOKEN` and `ANTHROPIC_AUTH_TOKEN`. The built-in `https` protocol recognizes `PARALLEL_API_KEY` and `EXA_API_KEY` for web search and page extraction.

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

Web-provider credentials are independent of the active model. Parallel and Exa
resolve a key from `auth.json`, overridden by that provider's process
environment variable. `models.json apiKey`, `URI_AGENT_API_KEY`, and
`--api-key` configure model requests and are not web-provider credentials.

## Configuration locations

URI Agent uses the platform configuration directory followed by `uri-agent`. On Linux this is normally:

```text
~/.config/uri-agent
```

Set `URI_AGENT_CONFIG_DIR` to replace the complete configuration directory path.

| Path | Purpose |
| --- | --- |
| `<config>/settings.json` | Global provider, model, thinking, output, and terminal settings |
| `<config>/auth.json` | Global provider credentials |
| `<config>/environment.json` | Global environment variables for Agent shell commands and trusted plugins |
| `<config>/models.json` | Custom providers, models, headers, and model overrides |
| `<config>/models-store.json` | Generated pi catalog cache |
| `<config>/keymap.rhai` | Global keymap overrides |
| `<config>/wasm-plugins/` | Trusted WASM modules loaded at startup and on reload |
| `<project>/.uri-agent/settings.json` | Optional project settings |
| `<project>/.uri-agent/keymap.rhai` | Optional project keymap overrides |

Sessions and complete tool outputs use platform data and cache directories rather than this configuration directory. See [Session storage and project boundaries](sessions.md#session-storage-and-project-boundaries) and [Complete output preservation](protocols.md#complete-output-preservation).

The WASM plugin directory follows `URI_AGENT_CONFIG_DIR` but is not a settings field. See [WASM plugins](plugins.md) for loading, installation, and reload behavior.

## Agent environment

Use `:set-env` to add or replace a variable such as `NPM_TOKEN`. The **Agent environment** row in `:settings` opens the complete manager: it displays names without values, `Enter` replaces the selected value, `Ctrl+N` adds a variable, and `Delete` removes one. Value prompts mask their input. Names use the portable form `[A-Za-z_][A-Za-z0-9_]*`.

Variables are global rather than project- or session-specific. URI Agent stores them as plain text in the private `environment.json` configuration file; on Unix it enforces mode `0600` and keeps the configuration directory at `0700`. Filesystem permissions are the protection boundary—values are not encrypted.

Each future Agent `bash` or `pwsh` command receives the latest saved values. A saved value overrides an inherited process variable with the same name; other process variables remain inherited normally. The manager does not modify URI Agent's own process environment, and variables are deliberately not injected into the user-controlled `:terminal` PTY.

The dedicated linked Rust and WASM host interface requires an explicitly requested whole-environment capability. The request names no individual variables and has no interactive approval flow; it exists as a sensitive-access marker for source review. See [WASM plugin environment access](plugins.md#agent-environment-access).

## Settings fields and precedence

`settings.json` and project settings use camel-case JSON fields:

| Field | Meaning | Default |
| --- | --- | --- |
| `defaultProvider` | Provider selected when no higher-priority override exists | unset |
| `defaultModel` | Model selected for that provider | unset |
| `outputLimit` | Maximum bytes returned inline before preserving full output | `32768` |
| `defaultThinkingLevel` | Fallback reasoning effort | `off` |
| `modelThinkingLevels` | Per-model effort keyed by `provider/model` | `{}` |
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

When the project settings file already exists, changes made through model selection, Settings, `:effort`, and `:set-terminal` are written there. Otherwise they are written to global `settings.json`. Environment and CLI overrides remain in force for the current invocation and are not replaced by those writes.

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

## Command-line options

Command-line flags can override the provider, model, process-only API key, project working directory, thinking effort, inline output limit, and offline catalog behavior. They can also resume the latest project session or a project-scoped session ID. Run `uri-agent --help` for the exact names, accepted values, and current syntax.

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

Command values run when the credential or header is first needed, not while URI Agent starts. They time out after 10 seconds and are cached for the process. On Windows they run through PowerShell; on other platforms they run through `sh -c`.

> [!WARNING]
> A leading `!` executes with the permissions of URI Agent. Do not load credential or header configuration from an untrusted project.
