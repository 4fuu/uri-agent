# WASM plugins

URI Agent loads trusted Extism modules as protocols and typed direct model
tools. `wasm_plugin://help`, `/load`, and `/author` are the model-facing
references. Rust authors should use the [SDK guide](../sdk/README.md) and
[buildable example](../examples/wasm-plugin/).

## Installation and reload

Place non-hidden regular `.wasm` files directly in
`<config>/wasm-plugins/`, then run `exec("wasm_plugin://reload", "")`. There is
no package manager, plugin setting, or CLI installation flag. Reload validates
the complete directory in stable order and atomically replaces the dynamic
protocol/tool set; a directory failure leaves the old set active, while invalid
or colliding modules are skipped with diagnostics. Existing calls finish on
their captured runtime. While resident lifecycle is active, reload shuts down
old resident instances and starts their replacements before future wakes. The
directory follows `URI_AGENT_CONFIG_DIR`.

## ABI version 6

Only ABI v6 is accepted. Earlier ABI and the former subagent API have no
compatibility path. Every module exports `uri_agent_manifest`; a module with a
protocol, tool, resident callback, or compaction callback also handles tagged
requests through `uri_agent_handle`.

The manifest declares protocol and strict typed-tool descriptors, permissions,
and resident opt-in. Protocols must implement `<protocol>://help`; bodies are
always strings. Typed tool schemas must be top-level objects with `properties`
and `additionalProperties: false`. Calls into one module are serialized and its
memory survives until reload.

```rust
PluginManifest::new(protocols)
    .with_model_tools(tools)
    .request_agent_access()
    .request_state_access()
    .with_resident()
```

Permission methods are source-audit markers, not interactive grants.
`request_environment_access()` permits `environment_variable`; and
`request_credentials_access()` permits `provider_api_key`. `model_role` and
project-overridable `plugin_setting` / `set_plugin_setting` require no manifest
permission.

## Agent API

`request_agent_access()` enables the typed opaque `AgentHandle` API:
`AgentHandle::create`, `open`, `submit`, `status`, `result`, `cancel`, and
`close`. `AgentSpec` selects provider, model, thinking, working directory,
required parent session ID, system-prompt mode, tool/protocol selection, and an
optional output cap. `SubmitKind` is `Prompt` or `Steer`.
Steer targets the next model boundary while the Agent is active and is accepted
as Prompt when the Agent is idle.

Plugin Agents use ordinary `sessions-v3.db` conversations. They require a
persisted same-project depth-1 parent and are always depth 2; no depth-2 Agent
may create another. Provider/model and thinking freeze after the first durably
accepted submission. Prompt, tools, and protocols are fixed at creation except
through an optional compaction callback. Set the callback boolean on `create`
or `open`; after summary generation the handler receives
`PluginEvent::Compacted` and may return an optional `AgentSpecPatch`. Only
prompt/tools/protocols may change, and the update and checkpoint commit
atomically. Full lifecycle and delivery semantics are in
[Sessions and context](sessions.md#agenthost-and-agent-specifications).

## Plugin state

Call `request_state_access()` before using typed `plugin_state_get`, `put`,
`delete`, `list`, or `compare_and_set`. State is namespaced by plugin and stored
in a separate SQLite database, not session events or `settings.json`.
`PluginStateScope::Global` shares state across projects; `Project` binds it to
the canonical project. Entries carry monotonically changing revisions and CAS
returns `None` on a revision mismatch. Encoded JSON values are limited to 1
MiB. Plugin settings remain a separate project-overridable configuration API.

## Resident plugins

`.with_resident()` opts into `PluginEvent::Resident` callbacks for `Start`,
`Wake`, and `Shutdown`. Return `ResidentResponse { wake_after_ms: Some(...) }`
to request a later wake. Plugins without this opt-in remain request-driven.
`uri-agent --background` runs residents without a TUI and remains
foreground-blocking for external supervision. This is intentionally not a
trigger, scheduler, gateway, or daemon framework.

## Trust and limits

WASM is portability, not a sandbox. Plugins are trusted code with WASI,
outbound HTTP, host filesystem access where supported, and static built-in
`read`/`exec` host calls. Dynamic WASM protocols and `wasm_plugin` cannot be
called recursively. Each guest call is limited to 60 minutes, 100 million fuel,
16 MiB WebAssembly memory, 1 MiB Extism variables, 16 MiB module/response size,
and a 256 KiB manifest; a manifest may declare at most 64 protocols and 64
tools. Plugin-state values have their separate 1 MiB limit.
