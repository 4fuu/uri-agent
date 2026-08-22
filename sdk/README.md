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

Build the module for WASI:

```bash
rustup target add wasm32-wasip1
cargo build --release --target wasm32-wasip1
```

The buildable [`examples/wasm-plugin`](../examples/wasm-plugin/) project contains a complete guest. Read `wasm_plugin://help` in URI Agent for the active installation directory and exact model-facing workflow.
