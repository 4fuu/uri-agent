# WASM plugins

URI Agent can load trusted [Extism](https://extism.org/) modules as protocols without changing the model-facing `read` and `exec` tool schemas. This document owns the persistent installation, reload lifecycle, ABI, permissions, and reliability limits for those modules.

For the active plugin directory and exact model-facing install and reload contract, read `wasm_plugin://help`. Rust guest authors should also use the [`uri-agent-plugin-sdk` README](../sdk/README.md) and the buildable [`examples/wasm-plugin`](../examples/wasm-plugin/) project.

## Choose an extension path

- Use a WASM plugin for a runtime-loaded third-party protocol with a portable ABI.
- Use a linked Rust extension for a first-party capability compiled into URI Agent. Linked extensions can register protocols, commands, panels, status providers, composer completion providers, and startup prompt fragments; see [Linked Rust extensions](development.md#linked-rust-extensions).

Both paths keep protocol operations behind `read` and `exec`. URI Agent does not load native dynamic libraries.

## Persistent installation

URI Agent loads modules from `<config>/wasm-plugins/`. The built-in `wasm_plugin` protocol exposes exactly two operations:

```text
read("wasm_plugin://help", {"kind":"none","value":""})
exec("wasm_plugin://reload", {"kind":"none","value":""})
```

There are no install, update, remove, or list operations, no `--wasm-plugin` flag, no `wasmPlugins` setting, and no URI Agent package manifest or package manager. The agent uses normal file and shell protocols to discover source, review it, clone it, and build it in a temporary directory. Installation writes a temporary file beside the destination and atomically renames it to `<name>.wasm` before reloading.

Only non-hidden regular `.wasm` files directly inside the persistent directory are loaded. Nested files and temporary suffixes such as `.wasm.tmp` are ignored. Atomically replacing a file updates its plugin at the next reload; removing a file disables it at the next reload.

The directory follows `URI_AGENT_CONFIG_DIR`; see [Configuration locations](configuration.md#configuration-locations).

## Reload lifecycle

Reload constructs a replacement set before changing the active registry:

1. read the complete directory in stable path order;
2. construct fresh Extism runtimes;
3. validate manifests and protocol names;
4. skip invalid modules and collisions with built-ins, Skills, or earlier modules, while retaining diagnostics;
5. atomically replace the complete dynamic protocol set.

A directory-level failure leaves the old set active. Calls that already captured an old protocol keep its runtime until they finish; new calls use the replacement set.

The reload `exec` call returns only after the replacement set is active. Its result lists the active protocol names and tells the model to read each new `<protocol>://help`. Diagnostics are stored as JSON in the session output directory rather than embedded in model-facing text; help reports their count and a readable `file://` address. The TUI protocol list reads the live registry and reflects the replacement set.

Frozen session prompts contain the stable `wasm_plugin` manager, not a dynamic protocol list. New and resumed sessions load the current persistent plugin set. After a change, the reload result and `wasm_plugin://help` describe the active state.

## ABI version 1

ABI version 1 lets a module contribute protocols. It does not contribute system prompt fragments, commands, panels, status providers, or composer completions. Exports use Extism's bytes-in/bytes-out functions and may be implemented with any compatible PDK.

Every module exports `uri_agent_manifest`, which takes no input and returns:

```json
{
  "abi_version": 1,
  "protocols": [
    {
      "name": "example",
      "description": "Read and execute example resources",
      "can_read": true,
      "can_exec": true
    }
  ]
}
```

An optional `permissions` object can request sensitive host capabilities. Existing ABI version 1 manifests without it continue to load with no Agent environment or credential access.

Protocol names must be unique within the module and satisfy the normal registry rules. Descriptions must be nonempty. Every protocol must set `can_read` to `true` and implement `read("<protocol>://help", {"kind":"none","value":""})`; `can_exec` may be `true` or `false`.

A module declaring a protocol also exports `uri_agent_handle`. URI Agent calls it with the selected protocol, operation, original URI, opaque target, and optional body:

```json
{
  "protocol": "example",
  "operation": "read",
  "uri": "example://a://b?x=1",
  "target": "a://b?x=1",
  "body": {"key": "value"}
}
```

`operation` is `read` or `exec`; `body` is `null` when the model-facing body
envelope uses `kind: "none"`. URI Agent decodes `text` and `json` envelopes
before this ABI boundary, so the plugin continues to receive the literal
protocol body. Returned bytes become the protocol result, while an Extism error
fails the call. A module remains instantiated until the next reload, so its
in-memory state survives calls; calls into one module are serialized.

Each plugin implements its protocol help through the same handler. That help must describe every supported address and body shape.

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

WASM is a portable ABI in this feature, not a sandbox. Plugins are trusted code. They receive WASI and unrestricted outbound HTTP; on Unix they also receive writable host filesystem access. SDK `read` and `exec` host calls route to URI Agent's static built-in protocols, including file and available shell protocols, with URI Agent's user permissions.

Host calls reject dynamic WASM protocols and `wasm_plugin` itself, preventing recursive entry into a module runtime.

Each guest call has these limits:

- 30-second wall-clock timeout;
- 100-million fuel limit;
- 16 MiB WebAssembly memory ceiling;
- 1 MiB Extism variable store;
- 16 MiB module and response limit;
- 256 KiB manifest limit;
- 64 protocols per manifest.

## Rust guest SDK

The workspace provides typed manifest and request values, built-in host calls, and a `define_plugin!` macro that generates both ABI exports. Follow the [`uri-agent-plugin-sdk` README](../sdk/README.md) for the Rust dependency, minimal guest, and build command.
