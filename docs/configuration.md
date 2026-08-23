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
- `openai-completions`
- `anthropic-messages`
- `google-generative-ai`

The model selector shows only models using a supported API family. Catalog model records are cached without dropping unknown fields so that future metadata survives a read/write cycle.

The active backend applies catalog data relevant to requests and accounting, including:

- context windows, output limits, and tiered prices;
- text and image input modalities;
- `reasoning`, `thinkingLevelMap`, and `samplingParams`;
- request-relevant `compat` values such as token-field names, adaptive thinking, strict role or tool handling, and provider-specific thinking formats.

### Refresh and offline mode

Catalog entries are considered fresh for four hours. Press `Ctrl+R` in the model selector or `r` in Settings to force a refresh.

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
| `anthropic` | Claude Pro/Max browser OAuth |
| `openrouter` | Browser PKCE |
| `openai-codex` | Browser or device-code login |
| `github-copilot` | Device-code login, including an optional Enterprise domain |
| `kimi-coding` | Subscription device-code login |
| `xai` | SuperGrok or X Premium device-code login |
| `radius` | Browser or device-code login |

Stored entries in `auth.json` use `type: "api_key"` or `type: "oauth"`. OAuth entries retain refresh data; URI Agent attempts to refresh expired entries that include a refresh token. On Unix, URI Agent creates `auth.json` with mode `0600` and the configuration directory with mode `0700`.

Known providers use conventional environment variables. Examples include `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `GEMINI_API_KEY`, `OPENROUTER_API_KEY`, and `GROQ_API_KEY`. Anthropic also recognizes `ANTHROPIC_OAUTH_TOKEN` and `ANTHROPIC_AUTH_TOKEN`.

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

Sessions and complete tool outputs use platform data and cache directories rather than this configuration directory. See [Terminal interface and sessions](interface.md#session-storage-and-project-boundaries) and [Complete output preservation](protocols.md#complete-output-preservation).

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

When the project settings file already exists, changes made through model selection, Settings, `:effort`, and `:set-terminal` are written there. Otherwise they are written to global `settings.json`. Environment and CLI overrides remain in force for the current invocation and are not replaced by those writes.

## Thinking effort

Thinking defaults to `off`. Supported values are:

```text
off, minimal, low, medium, high, xhigh, max
```

The active model determines which levels are available. Run `:effort` to open a selector containing only supported levels; the current effective level is selected when the panel opens.

`:effort` and the Thinking row in Settings persist the selection in `modelThinkingLevels` under `provider/model`. Switching models restores that model's saved value. `defaultThinkingLevel` is the file-level fallback; `URI_AGENT_THINKING` and `--thinking` override it for the current invocation.

## Command-line options

Command-line flags can override the provider, model, process-only API key, project working directory, thinking effort, inline output limit, and offline catalog behavior. They can also resume the latest project session or a project-scoped session ID. Run `uri-agent --help` for the exact names, accepted values, and current syntax.

`--continue-session` and `--session` conflict. Session selection remains scoped to the canonical `--cwd`; see [Sessions and context](interface.md#sessions-and-context).

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

Command values time out after 10 seconds and are cached for the process. On Windows they run through PowerShell; on other platforms they run through `sh -c`.

> [!WARNING]
> A leading `!` executes with the permissions of URI Agent. Do not load credential or header configuration from an untrusted project.
