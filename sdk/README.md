# URI Agent plugin SDK

This crate provides Rust guest types, ABI exports, and host calls for trusted URI Agent Extism WebAssembly protocol plugins. Runtime installation, reload behavior, permissions, and limits are documented in [WASM plugins](../docs/plugins.md).

## Use the SDK

Configure a `cdylib` guest that depends on this repository:

```toml
[lib]
crate-type = ["cdylib"]

[dependencies]
uri-agent-plugin-sdk = { git = "https://github.com/4fuu/uri-agent" }
```

Define a manifest and handler, then use `define_plugin!` to generate `uri_agent_manifest` and `uri_agent_handle`:

```rust
use uri_agent_plugin_sdk::{
    HandlerRequest, HandlerResult, PluginManifest, ProtocolDescriptor,
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
    match request.target.as_str() {
        "help" => Ok(b"# example\n\nDescribe every supported address here.\n".to_vec()),
        _ => Err(format!("unsupported address: {}", request.uri)),
    }
}

define_plugin!(manifest(), handle);
```

Every declared protocol must implement its `<protocol>://help` route. The SDK also exposes `uri_agent_plugin_sdk::read` and `uri_agent_plugin_sdk::exec` for calling URI Agent's static built-in protocols.

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

The request grants direct access to any variable in URI Agent's Agent environment manager; variable names are not declared in the manifest. It is a source-audit marker for trusted plugins, not an interactive approval or sandbox boundary. The host rejects `environment_variable` calls from a plugin whose manifest omitted the request. Runtime storage and trust details are in [WASM plugins](../docs/plugins.md#agent-environment-access).

Build the module for WASI:

```bash
rustup target add wasm32-wasip1
cargo build --release --target wasm32-wasip1
```

The buildable [`examples/wasm-plugin`](../examples/wasm-plugin/) project contains a complete guest. Read `wasm_plugin://help` in URI Agent for the active installation directory and exact model-facing workflow.
