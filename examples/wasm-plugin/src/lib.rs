use uri_agent_plugin_sdk::{
    define_plugin, HandlerRequest, HandlerResult, ModelToolDescriptor, Operation, PluginManifest,
    ProtocolDescriptor,
};

fn manifest() -> PluginManifest {
    PluginManifest::new([ProtocolDescriptor::new(
        "example",
        "Example Rust WASM plugin; read example://help before use",
        true,
        false,
    )])
    .with_model_tools([ModelToolDescriptor::new(
        "example_greeting",
        "Create an example greeting from a typed name argument.",
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
        HandlerRequest::Protocol {
            operation: Operation::Read,
            target,
            ..
        } if target == "help" => Ok(
            b"# example\n\nRead `example://echo` with a string body to echo the request. Pass an empty string when no content is needed.\n"
                .to_vec(),
        ),
        HandlerRequest::Protocol {
            operation: Operation::Read,
            target,
            body,
            ..
        } if target == "echo" => Ok(body.into_bytes()),
        HandlerRequest::ModelTool { name, arguments } if name == "example_greeting" => {
            let name = arguments
                .get("name")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| "name must be a string".to_string())?;
            Ok(format!("Hello, {name}!\n").into_bytes())
        }
        _ => Err("unsupported plugin request".to_string()),
    }
}

define_plugin!(manifest(), handle);
