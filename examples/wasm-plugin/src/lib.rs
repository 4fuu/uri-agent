use uri_agent_plugin_sdk::{
    define_plugin, HandlerOutput, HandlerRequest, HandlerResult, ModelToolDescriptor, Operation,
    PluginEvent, PluginManifest, ProtocolDescriptor, ResidentEvent, ResidentResponse,
};

fn manifest() -> PluginManifest {
    PluginManifest::new([ProtocolDescriptor::new(
        "example",
        "Example Rust WASM plugin; load its help page before use",
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
        } if target == "help" => Ok(r#"# example

Read `example://echo` with an `input` object to echo it back; the input returns
as pretty JSON in the text and unchanged as structured `json`:

{"read": "example://echo", "input": {"any": ["nested", {"value": true}]}}

Omit `input` to echo an empty object.
"#
        .into()),
        HandlerRequest::Protocol {
            operation: Operation::Read,
            target,
            input,
            ..
        } if target == "echo" => {
            let text = serde_json::to_string_pretty(&input).map_err(|error| error.to_string())?;
            Ok(HandlerOutput {
                text,
                json: Some(serde_json::Value::Object(input)),
            })
        }
        HandlerRequest::ModelTool { name, arguments } if name == "example_greeting" => {
            let name = arguments
                .get("name")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| "name must be a string".to_string())?;
            Ok(format!("Hello, {name}!\n").into())
        }
        HandlerRequest::Event {
            event: PluginEvent::Resident { event },
        } => resident(event),
        HandlerRequest::Event {
            event: PluginEvent::Compacted { .. },
        } => Ok("null".into()),
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
    serde_json::to_string(&response)
        .map_err(|error| error.to_string())
        .map(HandlerOutput::from)
}

define_plugin!(manifest(), handle);
