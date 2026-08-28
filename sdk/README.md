# URI Agent plugin SDK

This crate provides Rust guest types, ABI version 4 exports, and host calls for
trusted URI Agent Extism WebAssembly protocol and direct-tool plugins. URI Agent
continues to load ABI version 3 modules, but model-role lookup requires
rebuilding with this SDK. Runtime installation, reload behavior, permissions,
and limits are documented in
[WASM plugins](https://github.com/4fuu/uri-agent/blob/main/docs/plugins.md).

## Use the SDK

Configure a `cdylib` guest that depends on the SDK:

```toml
[lib]
crate-type = ["cdylib"]

[dependencies]
uri-agent-plugin-sdk = "2026.828.0"
```

Define a manifest and handler, then use `define_plugin!` to generate `uri_agent_manifest` and `uri_agent_handle`:

```rust
use uri_agent_plugin_sdk::{
    HandlerRequest, HandlerResult, Operation, PluginManifest, ProtocolDescriptor,
    define_plugin,
};

fn manifest() -> PluginManifest {
    PluginManifest::new([ProtocolDescriptor::new(
        "example",
        "Read example://help before use",
        true,
        false,
    )])
}

fn handle(request: HandlerRequest) -> HandlerResult {
    match request {
        HandlerRequest::Protocol {
            operation: Operation::Read,
            target,
            ..
        } if target == "help" =>
            Ok(b"# example\n\nDescribe every supported address here.\n".to_vec()),
        _ => Err("unsupported plugin request".to_string()),
    }
}

define_plugin!(manifest(), handle);
```

Every declared protocol must implement its `<protocol>://help` route. Protocol
bodies are always strings; use `""` for no body and serialize structured input
as complete JSON text. The SDK also exposes `uri_agent_plugin_sdk::read` and
`uri_agent_plugin_sdk::exec` with `(uri: &str, body: &str)` for calling URI
Agent's static built-in protocols.

## Register a typed direct tool

Use a direct tool when complex arguments would otherwise require nested string
escaping. Add a strict JSON Schema descriptor to the manifest, then handle the
tagged model-tool request:

```rust
use uri_agent_plugin_sdk::{
    HandlerRequest, HandlerResult, ModelToolDescriptor, PluginManifest,
};

fn manifest() -> PluginManifest {
    PluginManifest::new([/* optional protocol descriptors */])
        .with_model_tools([ModelToolDescriptor::new(
            "example_greeting",
            "Create a greeting from a typed name argument.",
            serde_json::json!({
                "type": "object",
                "properties": {"name": {"type": "string"}},
                "required": ["name"],
                "additionalProperties": false
            }),
        )])
}

fn handle(request: HandlerRequest) -> HandlerResult {
    match request {
        HandlerRequest::ModelTool { name, arguments }
            if name == "example_greeting" =>
        {
            let name = arguments["name"]
                .as_str()
                .ok_or_else(|| "name must be a string".to_string())?;
            Ok(format!("Hello, {name}!\n").into_bytes())
        }
        _ => Err("unsupported plugin request".to_string()),
    }
}
```

Add `serde_json = "1"` to the guest dependencies when constructing schemas this
way. The top-level schema must declare `type: "object"`, include a `properties`
map, and set `additionalProperties: false`; every `required` name must be present
in that map. URI Agent validates names and this strict schema shape, exposes the
descriptor to the model, and sends typed arguments as
`HandlerRequest::ModelTool` without protocol-body serialization.

## Resolve a configured model role

Model roles let the user choose model routes for plugin-owned work without the
plugin hard-coding a provider or changing the conversation model:

```rust
#[cfg(target_family = "wasm")]
fn review_model() -> Result<uri_agent_plugin_sdk::ModelRole, String> {
    uri_agent_plugin_sdk::model_role("review")
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "model role review is not configured".to_string())
}
```

The result contains `provider`, `model`, and `thinking`. Lookup is dynamic,
returns no credential, performs no model request, and requires no manifest
permission. Role configuration and precedence are documented under
[Model roles for plugins](https://github.com/4fuu/uri-agent/blob/main/docs/configuration.md#model-roles-for-plugins).

## Request Agent environment access

To read user-managed Agent environment variables, request the single capability on the manifest and then look up names dynamically:

```rust
fn manifest() -> PluginManifest {
    PluginManifest::new([/* protocol descriptors */])
        .request_environment_access()
}

#[cfg(target_family = "wasm")]
fn use_token_without_exposing_it() -> Result<(), String> {
    let token = uri_agent_plugin_sdk::environment_variable("NPM_TOKEN")
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "NPM_TOKEN is not configured".to_string())?;
    // Pass `token` to the intended service; do not include it in plugin output.
    Ok(())
}
```

The request grants direct access to any variable in URI Agent's Agent environment manager; variable names are not declared in the manifest. It is a source-audit marker for trusted plugins, not an interactive approval or sandbox boundary. The host rejects `environment_variable` calls from a plugin whose manifest omitted the request. Runtime storage and trust details are in [WASM plugins](https://github.com/4fuu/uri-agent/blob/main/docs/plugins.md#agent-environment-access).

## Request provider credential access

To resolve API keys saved through `:login` or supplied through conventional
provider process environment variables, request the credential capability and
look up provider IDs dynamically:

```rust
fn manifest() -> PluginManifest {
    PluginManifest::new([/* protocol descriptors */])
        .request_credentials_access()
}

#[cfg(target_family = "wasm")]
fn use_provider_key_without_exposing_it() -> Result<(), String> {
    let key = uri_agent_plugin_sdk::provider_api_key("parallel")
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "Parallel is not logged in".to_string())?;
    // Pass `key` to the intended provider; do not include it in plugin output.
    Ok(())
}
```

The request grants API-key reads for any provider and is a source-audit marker,
not an interactive approval. It exposes neither OAuth refresh data nor Agent
environment values. See [Provider credential access](https://github.com/4fuu/uri-agent/blob/main/docs/plugins.md#provider-credential-access).

Build the module for WASI:

```bash
rustup target add wasm32-wasip1
cargo build --release --target wasm32-wasip1
```

The buildable [`examples/wasm-plugin`](https://github.com/4fuu/uri-agent/tree/main/examples/wasm-plugin) project contains a complete guest. Read `wasm_plugin://help/author` for model-facing authoring guidance and `wasm_plugin://help/load` before loading or reloading the completed plugin.
