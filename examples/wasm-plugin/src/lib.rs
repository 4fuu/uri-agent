use uri_agent_plugin_sdk::{
    define_plugin, HandlerRequest, HandlerResult, Operation, PluginManifest, ProtocolDescriptor,
};

fn manifest() -> PluginManifest {
    PluginManifest::new([ProtocolDescriptor::new(
        "example",
        "Example Rust WASM plugin; read example://help before use",
        true,
        false,
    )])
}

fn handle(request: HandlerRequest) -> HandlerResult {
    match (request.operation, request.target.as_str()) {
        (Operation::Read, "help") => Ok(
            b"# example\n\nRead `example://echo` with an optional JSON body to echo the request.\n"
                .to_vec(),
        ),
        (Operation::Read, "echo") => {
            serde_json::to_vec(&request).map_err(|error| format!("cannot encode request: {error}"))
        }
        _ => Err(format!("unsupported address: {}", request.uri)),
    }
}

define_plugin!(manifest(), handle);
