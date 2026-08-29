use uri_agent_plugin_sdk::{
    define_plugin, HandlerRequest, HandlerResult, ModelToolDescriptor, Operation, PluginEvent,
    PluginManifest, ProtocolDescriptor, ResidentEvent, ResidentResponse,
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
    .request_state_access()
    .with_resident()
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
        HandlerRequest::Event {
            event: PluginEvent::Resident { event },
        } => resident(event),
        HandlerRequest::Event {
            event: PluginEvent::Compacted { .. },
        } => Ok(b"null".to_vec()),
        _ => Err("unsupported plugin request".to_string()),
    }
}

fn resident(event: ResidentEvent) -> HandlerResult {
    #[cfg(target_family = "wasm")]
    {
        let entry = uri_agent_plugin_sdk::plugin_state_get(
            uri_agent_plugin_sdk::PluginStateScope::Global,
            "resident-events",
        )
        .map_err(|error| error.to_string())?;
        let count = entry
            .as_ref()
            .and_then(|entry| entry.value.as_u64())
            .unwrap_or(0)
            + 1;
        uri_agent_plugin_sdk::plugin_state_compare_and_set(
            uri_agent_plugin_sdk::PluginStateScope::Global,
            "resident-events",
            entry.map(|entry| entry.revision),
            serde_json::json!(count),
        )
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "resident event counter changed concurrently".to_string())?;
    }

    let response = ResidentResponse {
        wake_after_ms: (event == ResidentEvent::Start).then_some(60_000),
    };
    serde_json::to_vec(&response).map_err(|error| error.to_string())
}

define_plugin!(manifest(), handle);
