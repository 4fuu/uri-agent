# WASM plugins

URI Agent can load trusted [Extism](https://extism.org/) modules as protocols
and typed direct model tools. This document owns the persistent installation,
reload lifecycle, ABI, permissions, and reliability limits for those modules.

`wasm_plugin://help` reports active capabilities and links to separate model-facing guidance: `wasm_plugin://help/load` owns loading and reloading, while `wasm_plugin://help/author` owns authoring. Rust guest authors should also use the [`uri-agent-plugin-sdk` README](../sdk/README.md) and the buildable [`examples/wasm-plugin`](../examples/wasm-plugin/) project.

## Choose an extension path

- Use a WASM plugin for a runtime-loaded third-party protocol or direct tool with a portable ABI.
- Use a linked Rust extension for a first-party capability compiled into URI Agent. Linked extensions can register protocols, direct tools, commands, panels, status providers, composer completion providers, and startup prompt fragments; see [Linked Rust extensions](development.md#linked-rust-extensions).

Prefer a protocol for a capability with simple string input. Register a typed
direct tool when structured or escape-heavy arguments would otherwise require
nested serialization. URI Agent does not load native dynamic libraries.

## Persistent installation

URI Agent loads modules from `<config>/wasm-plugins/`. The built-in `wasm_plugin` protocol exposes three help addresses and one reload action:

```text
read("wasm_plugin://help", "")
read("wasm_plugin://help/load", "")
read("wasm_plugin://help/author", "")
exec("wasm_plugin://reload", "")
```

There are no install, update, remove, or list operations, no `--wasm-plugin` flag, no `wasmPlugins` setting, and no URI Agent package manifest or package manager. The agent uses normal file and shell protocols to discover source, review it, clone it, and build it in a temporary directory. Installation writes a temporary file beside the destination and atomically renames it to `<name>.wasm` before reloading.

Only non-hidden regular `.wasm` files directly inside the persistent directory are loaded. Nested files and temporary suffixes such as `.wasm.tmp` are ignored. Atomically replacing a file updates its plugin at the next reload; removing a file disables it at the next reload.

The directory follows `URI_AGENT_CONFIG_DIR`; see [Configuration locations](configuration.md#configuration-locations). Initial compilation and loading begin in the background after session context is ready. The first dynamic protocol lookup waits for that initial set when necessary, while the stable `wasm_plugin` manager remains available immediately.

## Reload lifecycle

Reload constructs a replacement set before changing the active registry:

1. read the complete directory in stable path order;
2. construct fresh Extism runtimes;
3. validate manifests, protocol names, direct-tool names, and tool schemas;
4. skip invalid modules and collisions with built-ins, Skills, or earlier modules, while retaining diagnostics;
5. atomically replace the complete dynamic protocol and direct-tool set.

A directory-level failure leaves the old set active. Calls that already captured an old protocol keep its runtime until they finish; new calls use the replacement set.

The reload `exec` call returns only after the replacement set is active. Its
result lists active protocol and direct-tool names and tells the model to read
each new `<protocol>://help`. Diagnostics are stored as JSON in the session
output directory rather than embedded in model-facing text; help reports their
count and a readable `file://` address. The TUI protocol list reads the live
protocol registry and reflects that part of the replacement set.

Frozen session prompts contain the stable `wasm_plugin` manager, not a dynamic protocol list. New and resumed sessions load the current persistent plugin set. After a change, the reload result and `wasm_plugin://help` describe the active state; the detailed loading and authoring pages remain separately addressable.

## ABI version 5

ABI version 5 adds bounded role-based subagent inference and plugin-owned
persistent settings to the protocols, typed direct model tools, and read-only
model-role lookup available in version 4. WASM plugins do not contribute system
prompt fragments, commands, panels, status providers, or composer completions.
Exports use Extism's bytes-in/bytes-out functions and may be implemented with
any compatible PDK.
ABI versions 3 and 4 remain supported; rebuild with the version 5 SDK to call a
subagent or use plugin settings. ABI version 2 is intentionally unsupported.

Every module exports `uri_agent_manifest`, which takes no input and returns:

```json
{
  "abi_version": 5,
  "protocols": [
    {
      "name": "example",
      "description": "Read and execute example resources",
      "can_read": true,
      "can_exec": true
    }
  ],
  "model_tools": [
    {
      "name": "example_greeting",
      "description": "Create a greeting from a typed name argument",
      "parameters": {
        "type": "object",
        "properties": {"name": {"type": "string"}},
        "required": ["name"],
        "additionalProperties": false
      }
    }
  ],
  "permissions": {
    "environment": false,
    "credentials": false,
    "subagents": false
  }
}
```

Every version 5 manifest declares `protocols`, `model_tools`, and all permission fields;
use an empty array when it contributes no capability of one kind. Set a
permission to `true` to request the corresponding audited host capability.

Protocol names must be unique within the module and satisfy the normal registry
rules. Descriptions must be nonempty. Every protocol must set `can_read` to
`true` and implement `read("<protocol>://help", "")`; `can_exec` may be `true`
or `false`. Direct-tool names must also be unique and must not collide with a
linked or earlier WASM tool. `parameters` is the JSON Schema object sent to the
model. Its top level must declare `type: "object"`, provide a `properties` map,
and set `additionalProperties: false`; every `required` entry must name one of
those properties.

A module declaring a protocol or direct tool also exports `uri_agent_handle`.
Protocol calls use this tagged request:

```json
{
  "kind": "protocol",
  "protocol": "example",
  "operation": "read",
  "uri": "example://a://b?x=1",
  "target": "a://b?x=1",
  "body": "{\"key\":\"value\"}"
}
```

`operation` is `read` or `exec`. `body` is always a string and is `""` when the
protocol has no body. Direct tools receive the exact typed argument object:

```json
{
  "kind": "model_tool",
  "name": "example_greeting",
  "arguments": {"name": "Ada"}
}
```

To register a direct tool with the Rust SDK, construct a
`ModelToolDescriptor`, pass it to `PluginManifest::with_model_tools`, and match
`HandlerRequest::ModelTool` in the handler. Returned bytes become the tool or
protocol result, while an Extism error fails the call. A module remains
instantiated until the next reload, so its in-memory state survives calls;
calls into one module are serialized.

Each plugin implements its protocol help through the same handler. That help must describe every supported address and body shape.

## Plugin settings

ABI version 5 gives every WASM module a persistent JSON key/value namespace.
The namespace is the module's `.wasm` filename stem and needs no manifest
permission:

```rust
let role = uri_agent_plugin_sdk::plugin_setting("role")?
    .and_then(|value| value.as_str().map(str::to_owned))
    .unwrap_or_else(|| "small".to_string());
uri_agent_plugin_sdk::set_plugin_setting("role", serde_json::json!("review"))?;
```

Values live under `pluginSettings` in global or project `settings.json`. A
project value overrides the same global key independently of other keys. One
encoded value may not exceed 1 MiB. The store is trusted plugin configuration,
not a secret store or permission boundary; see
[Model roles for plugins](configuration.md#model-roles-for-plugins) for the
complete precedence and naming contract.

## Model-role lookup

Model roles are named model routes configured in global or project
`settings.json`; see [Model roles for plugins](configuration.md#model-roles-for-plugins).
An ABI version 4 or 5 Rust guest resolves one dynamically without declaring a
manifest permission:

```rust
let role = uri_agent_plugin_sdk::model_role("review")?
    .ok_or("model role review is not configured")?;
```

The result contains `provider`, `model`, and the resolved `thinking` value.
Lookup returns `None` for an unconfigured role and fails when the role name or
configured model is invalid. It returns no API key, does not perform inference,
and does not change the conversation model. A settings reload or project
override is reflected without reloading the plugin.

## Subagent inference

ABI version 5 plugins can run a bounded, ephemeral model/tool loop through a
configured role. Request the capability in source:

```rust
fn manifest() -> PluginManifest {
    PluginManifest::new(protocols).request_subagent_access()
}
```

Then call a role without naming its provider, model, or credential:

```rust
let request = uri_agent_plugin_sdk::SubagentRequest::new(
    "small",
    "Fix parser recovery after an incomplete expression",
)
.with_tools(std::iter::empty::<String>())
.with_protocols(std::iter::empty::<String>())
.replace_system_prompt("Create a short terminal title. Return only the title.")
.with_max_output_tokens(32)
.with_timeout_ms(10_000);
let response = uri_agent_plugin_sdk::subagent(&request)?;
```

The request fields are `role`, `prompt`, `systemPrompt`, `tools`, `protocols`,
`workingDirectory`, `maxOutputTokens`, and `timeoutMs`. Omitting `tools` or
`protocols` inherits that complete registered set; providing a list is an exact
override, including `[]`. Unknown or repeated names fail the request. Append
mode adds `systemPrompt` after URI Agent's generated capability and plugin
instructions. Replace mode uses it as the whole prompt and is accepted only
when both effective capability sets are empty.

A WASM caller's own declared protocols and direct tools are excluded from
inheritance and rejected when selected explicitly. The module is still active
on the host-call stack, so re-entering that same runtime would deadlock. Other
registered capabilities remain available.

Subagent depth is exactly one. A linked or WASM plugin reached through a
subagent may use its other capabilities, but an attempt to start another
subagent fails before a model request is made. Independent top-level subagent
calls may still run concurrently.

The default working directory is the active project. A different existing path
rebuilds the linked built-ins, project instructions, and Skills rooted there;
it does not carry the active dynamic WASM set into that alternate toolbox. The
prompt must be nonempty; the combined system and user input is limited to 16
MiB. Timeouts default to 60 minutes and may not exceed 60 minutes. WASM module
calls have the same wall-clock limit.

Each call starts with only the supplied user prompt, runs tool calls until the
model returns text, and aggregates usage and estimated cost across rounds. It
uses the role's configured thinking, normal credential, provider request
transforms, and transient retry policy without modifying the active
conversation or session history. The response contains `text`, the serving
`role`, `provider`, `model`, and `thinking`, plus optional usage and `cost`.
The capability is a source-audit marker with no interactive approval flow.
Calls from a manifest that omitted it are rejected. Plugins cannot bypass
roles to submit a provider or model ID.

## Agent environment access

A plugin that directly reads values saved in URI Agent's [Agent environment manager](configuration.md#agent-environment) must request one whole-environment capability in its manifest. The Rust SDK keeps the request visible in source:

```rust
fn manifest() -> PluginManifest {
    PluginManifest::new(protocols).request_environment_access()
}
```

After that request, `uri_agent_plugin_sdk::environment_variable(name)` can dynamically read any valid variable name. Plugins do not declare names in advance, and URI Agent does not maintain per-variable grants or show an approval prompt. A plugin without the manifest request receives an error from this host API.

This declaration is deliberately an audit marker, not a sandbox boundary. Install only source you trust and review direct environment requests alongside the plugin's existing filesystem, HTTP, WASI, and built-in protocol use. The host API returns only values saved in the Agent environment manager; it does not treat the URI Agent process environment as part of that store.

## Provider credential access

A plugin that needs a provider API key can request the credential capability in
its manifest:

```rust
fn manifest() -> PluginManifest {
    PluginManifest::new(protocols).request_credentials_access()
}
```

`uri_agent_plugin_sdk::provider_api_key(provider)` then resolves an API key
saved through `:login` or supplied through that provider's conventional process
environment variable. Resolution is dynamic, so a login or logout takes effect
without reloading the plugin. The host API returns `None` when no key is
configured and rejects calls from a plugin that omitted the manifest request.

The capability grants read access to API keys for every provider; manifests do
not declare provider IDs in advance. It does not expose OAuth refresh data or
the Agent environment manager. Like environment access, this is a source-audit
marker for trusted code rather than an interactive grant or sandbox boundary.

## Trust and reliability

WASM is a portable ABI in this feature, not a sandbox. Plugins are trusted
code. They receive WASI and unrestricted outbound HTTP; on Unix they also
receive writable host filesystem access. SDK `read` and `exec` host calls take
the same required string body as the model tools and route to URI Agent's
static built-in protocols, including file and available shell protocols, with
URI Agent's user permissions.

Host calls reject dynamic WASM protocols and `wasm_plugin` itself, preventing recursive entry into a module runtime.

Each guest call has these limits:

- 60-minute wall-clock timeout;
- 100-million fuel limit;
- 16 MiB WebAssembly memory ceiling;
- 1 MiB Extism variable store;
- 16 MiB module and response limit;
- 256 KiB manifest limit;
- 64 protocols per manifest;
- 64 direct model tools per manifest.

Subagent input and timeout limits apply in addition to these module limits.

## Rust guest SDK

The workspace provides typed manifest and request values, built-in host calls, and a `define_plugin!` macro that generates both ABI exports. Follow the [`uri-agent-plugin-sdk` README](../sdk/README.md) for the Rust dependency, minimal guest, and build command.
