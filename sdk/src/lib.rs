//! Rust guest types and host calls for URI Agent WebAssembly plugins.
//!
//! Wire types and constants compile on every target. Extism guest exports and
//! host-call wrappers compile only for WebAssembly guests.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[cfg(target_family = "wasm")]
#[doc(hidden)]
pub use extism_pdk;
#[cfg(target_family = "wasm")]
pub use extism_pdk::{plugin_fn, Error, FnResult, Json};

pub const ABI_VERSION: u32 = 6;
pub const MANIFEST_EXPORT: &str = "uri_agent_manifest";
pub const HANDLE_EXPORT: &str = "uri_agent_handle";
pub const HOST_NAMESPACE: &str = "extism:host/user";
pub const HOST_READ: &str = "uri_agent_read";
pub const HOST_EXEC: &str = "uri_agent_exec";
pub const HOST_ENVIRONMENT: &str = "uri_agent_environment";
pub const HOST_CREDENTIALS: &str = "uri_agent_credentials";
pub const HOST_MODEL_ROLE: &str = "uri_agent_model_role";
pub const HOST_AGENT: &str = "uri_agent_agent";
pub const HOST_PLUGIN_STATE: &str = "uri_agent_plugin_state";
pub const HOST_PLUGIN_SETTING_GET: &str = "uri_agent_plugin_setting_get";
pub const HOST_PLUGIN_SETTING_SET: &str = "uri_agent_plugin_setting_set";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelRole {
    pub provider: String,
    pub model: String,
    pub thinking: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "mode", content = "content", rename_all = "snake_case")]
pub enum SystemPromptSelection {
    Inherit,
    Append(String),
    Replace(String),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "mode", content = "names", rename_all = "snake_case")]
pub enum CapabilitySelection {
    All,
    Only(Vec<String>),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentSpec {
    pub provider: String,
    pub model: String,
    pub thinking: String,
    pub working_directory: String,
    pub parent_session_id: String,
    pub system_prompt: SystemPromptSelection,
    pub tools: CapabilitySelection,
    pub protocols: CapabilitySelection,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<usize>,
}

impl AgentSpec {
    pub fn new(
        provider: impl Into<String>,
        model: impl Into<String>,
        thinking: impl Into<String>,
        working_directory: impl Into<String>,
        parent_session_id: impl Into<String>,
    ) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
            thinking: thinking.into(),
            working_directory: working_directory.into(),
            parent_session_id: parent_session_id.into(),
            system_prompt: SystemPromptSelection::Inherit,
            tools: CapabilitySelection::All,
            protocols: CapabilitySelection::All,
            max_output_tokens: None,
        }
    }

    pub fn append_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = SystemPromptSelection::Append(prompt.into());
        self
    }

    pub fn replace_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = SystemPromptSelection::Replace(prompt.into());
        self
    }

    pub fn with_tools(mut self, tools: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.tools = CapabilitySelection::Only(tools.into_iter().map(Into::into).collect());
        self
    }

    pub fn with_protocols(
        mut self,
        protocols: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.protocols = CapabilitySelection::Only(protocols.into_iter().map(Into::into).collect());
        self
    }

    pub fn with_max_output_tokens(mut self, max_output_tokens: usize) -> Self {
        self.max_output_tokens = Some(max_output_tokens);
        self
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SubmitKind {
    Prompt,
    Steer,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Idle,
    Running,
    Closed,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum AgentRequest {
    Create {
        spec: AgentSpec,
        #[serde(default)]
        compaction_callback: bool,
    },
    Open {
        session_id: String,
        #[serde(default)]
        compaction_callback: bool,
    },
    Submit {
        handle: u64,
        text: String,
        kind: SubmitKind,
    },
    Status {
        handle: u64,
    },
    Result {
        handle: u64,
    },
    Cancel {
        handle: u64,
    },
    Close {
        handle: u64,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentOpenResponse {
    pub handle: u64,
    pub session_id: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentSubmitResponse {
    pub submission_id: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentResultResponse {
    pub text: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentHandle {
    handle: u64,
    session_id: String,
}

impl AgentHandle {
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    #[cfg(target_family = "wasm")]
    pub fn create(spec: AgentSpec, compaction_callback: bool) -> Result<Self, Error> {
        let response: AgentOpenResponse = agent_call(&AgentRequest::Create {
            spec,
            compaction_callback,
        })?;
        Ok(Self {
            handle: response.handle,
            session_id: response.session_id,
        })
    }

    #[cfg(target_family = "wasm")]
    pub fn open(session_id: impl Into<String>, compaction_callback: bool) -> Result<Self, Error> {
        let response: AgentOpenResponse = agent_call(&AgentRequest::Open {
            session_id: session_id.into(),
            compaction_callback,
        })?;
        Ok(Self {
            handle: response.handle,
            session_id: response.session_id,
        })
    }

    #[cfg(target_family = "wasm")]
    pub fn submit(&self, text: impl Into<String>, kind: SubmitKind) -> Result<u64, Error> {
        let response: AgentSubmitResponse = agent_call(&AgentRequest::Submit {
            handle: self.handle,
            text: text.into(),
            kind,
        })?;
        Ok(response.submission_id)
    }

    #[cfg(target_family = "wasm")]
    pub fn status(&self) -> Result<AgentStatus, Error> {
        agent_call(&AgentRequest::Status {
            handle: self.handle,
        })
    }

    #[cfg(target_family = "wasm")]
    pub fn result(&self) -> Result<Option<String>, Error> {
        let response: AgentResultResponse = agent_call(&AgentRequest::Result {
            handle: self.handle,
        })?;
        Ok(response.text)
    }

    #[cfg(target_family = "wasm")]
    pub fn cancel(&self) -> Result<bool, Error> {
        agent_call(&AgentRequest::Cancel {
            handle: self.handle,
        })
    }

    #[cfg(target_family = "wasm")]
    pub fn close(self) -> Result<(), Error> {
        agent_call(&AgentRequest::Close {
            handle: self.handle,
        })
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentSpecPatch {
    pub system_prompt: Option<SystemPromptUpdate>,
    pub tools: Option<CapabilitySelection>,
    pub protocols: Option<CapabilitySelection>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "mode", content = "content", rename_all = "snake_case")]
pub enum SystemPromptUpdate {
    Append(String),
    Replace(String),
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResidentEvent {
    Start,
    Wake,
    Shutdown,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum PluginEvent {
    Compacted {
        session_id: String,
        summary: String,
        manual: bool,
        spec: Box<AgentSpec>,
    },
    Resident {
        event: ResidentEvent,
    },
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResidentResponse {
    pub wake_after_ms: Option<u64>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginStateScope {
    Global,
    Project,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginStateEntry {
    pub key: String,
    pub value: Value,
    pub revision: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum PluginStateRequest {
    Get {
        scope: PluginStateScope,
        key: String,
    },
    Put {
        scope: PluginStateScope,
        key: String,
        value: Value,
    },
    Delete {
        scope: PluginStateScope,
        key: String,
    },
    List {
        scope: PluginStateScope,
        prefix: String,
        limit: usize,
    },
    CompareAndSet {
        scope: PluginStateScope,
        key: String,
        expected_revision: Option<u64>,
        value: Value,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginSettingSetRequest {
    pub key: String,
    pub value: Value,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginSettingGetResponse {
    pub found: bool,
    pub value: Value,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PluginManifest {
    pub abi_version: u32,
    pub protocols: Vec<ProtocolDescriptor>,
    pub model_tools: Vec<ModelToolDescriptor>,
    pub permissions: PluginPermissions,
    #[serde(default)]
    pub resident: bool,
}

impl PluginManifest {
    pub fn new(protocols: impl IntoIterator<Item = ProtocolDescriptor>) -> Self {
        Self {
            abi_version: ABI_VERSION,
            protocols: protocols.into_iter().collect(),
            model_tools: Vec::new(),
            permissions: PluginPermissions::default(),
            resident: false,
        }
    }

    pub fn with_model_tools(
        mut self,
        tools: impl IntoIterator<Item = ModelToolDescriptor>,
    ) -> Self {
        self.model_tools = tools.into_iter().collect();
        self
    }

    pub fn request_environment_access(mut self) -> Self {
        self.permissions.environment = true;
        self
    }

    pub fn request_credentials_access(mut self) -> Self {
        self.permissions.credentials = true;
        self
    }

    pub fn request_agent_access(mut self) -> Self {
        self.permissions.agents = true;
        self
    }

    pub fn request_state_access(mut self) -> Self {
        self.permissions.state = true;
        self
    }

    pub fn with_resident(mut self) -> Self {
        self.resident = true;
        self
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PluginPermissions {
    pub environment: bool,
    pub credentials: bool,
    #[serde(default)]
    pub agents: bool,
    #[serde(default)]
    pub state: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProtocolDescriptor {
    pub name: String,
    pub description: String,
    pub can_read: bool,
    pub can_exec: bool,
}

impl ProtocolDescriptor {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        can_read: bool,
        can_exec: bool,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            can_read,
            can_exec,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelToolDescriptor {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

impl ModelToolDescriptor {
    pub fn new(name: impl Into<String>, description: impl Into<String>, parameters: Value) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    Read,
    Exec,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HandlerRequest {
    Protocol {
        protocol: String,
        operation: Operation,
        uri: String,
        target: String,
        body: String,
    },
    ModelTool {
        name: String,
        arguments: Value,
    },
    Event {
        event: PluginEvent,
    },
}

pub type HandlerResult = Result<Vec<u8>, String>;

#[cfg(target_family = "wasm")]
#[derive(Serialize)]
struct HostRequest<'a> {
    uri: &'a str,
    body: &'a str,
}

#[cfg(target_family = "wasm")]
mod host {
    use extism_pdk::*;

    #[host_fn("extism:host/user")]
    extern "ExtismHost" {
        pub fn uri_agent_read(input: String) -> String;
        pub fn uri_agent_exec(input: String) -> String;
        pub fn uri_agent_environment(input: String) -> String;
        pub fn uri_agent_credentials(input: String) -> String;
        pub fn uri_agent_model_role(input: String) -> String;
        pub fn uri_agent_agent(input: String) -> String;
        pub fn uri_agent_plugin_state(input: String) -> String;
        pub fn uri_agent_plugin_setting_get(input: String) -> String;
        pub fn uri_agent_plugin_setting_set(input: String) -> String;
    }
}

#[cfg(target_family = "wasm")]
pub fn read(uri: &str, body: &str) -> Result<String, Error> {
    call_host(uri, body, host::uri_agent_read)
}

#[cfg(target_family = "wasm")]
pub fn exec(uri: &str, body: &str) -> Result<String, Error> {
    call_host(uri, body, host::uri_agent_exec)
}

#[cfg(target_family = "wasm")]
pub fn environment_variable(name: &str) -> Result<Option<String>, Error> {
    let output = unsafe { host::uri_agent_environment(name.to_string())? };
    Ok(serde_json::from_str(&output)?)
}

#[cfg(target_family = "wasm")]
pub fn provider_api_key(provider: &str) -> Result<Option<String>, Error> {
    let output = unsafe { host::uri_agent_credentials(provider.to_string())? };
    Ok(serde_json::from_str(&output)?)
}

#[cfg(target_family = "wasm")]
pub fn model_role(name: &str) -> Result<Option<ModelRole>, Error> {
    let output = unsafe { host::uri_agent_model_role(name.to_string())? };
    Ok(serde_json::from_str(&output)?)
}

#[cfg(target_family = "wasm")]
fn agent_call<T: for<'de> Deserialize<'de>>(request: &AgentRequest) -> Result<T, Error> {
    let input = serde_json::to_string(request)?;
    let output = unsafe { host::uri_agent_agent(input)? };
    Ok(serde_json::from_str(&output)?)
}

#[cfg(target_family = "wasm")]
fn plugin_state_call<T: for<'de> Deserialize<'de>>(
    request: &PluginStateRequest,
) -> Result<T, Error> {
    let input = serde_json::to_string(request)?;
    let output = unsafe { host::uri_agent_plugin_state(input)? };
    Ok(serde_json::from_str(&output)?)
}

#[cfg(target_family = "wasm")]
pub fn plugin_state_get(
    scope: PluginStateScope,
    key: impl Into<String>,
) -> Result<Option<PluginStateEntry>, Error> {
    plugin_state_call(&PluginStateRequest::Get {
        scope,
        key: key.into(),
    })
}

#[cfg(target_family = "wasm")]
pub fn plugin_state_put(
    scope: PluginStateScope,
    key: impl Into<String>,
    value: Value,
) -> Result<PluginStateEntry, Error> {
    plugin_state_call(&PluginStateRequest::Put {
        scope,
        key: key.into(),
        value,
    })
}

#[cfg(target_family = "wasm")]
pub fn plugin_state_delete(scope: PluginStateScope, key: impl Into<String>) -> Result<bool, Error> {
    plugin_state_call(&PluginStateRequest::Delete {
        scope,
        key: key.into(),
    })
}

#[cfg(target_family = "wasm")]
pub fn plugin_state_list(
    scope: PluginStateScope,
    prefix: impl Into<String>,
    limit: usize,
) -> Result<Vec<PluginStateEntry>, Error> {
    plugin_state_call(&PluginStateRequest::List {
        scope,
        prefix: prefix.into(),
        limit,
    })
}

#[cfg(target_family = "wasm")]
pub fn plugin_state_compare_and_set(
    scope: PluginStateScope,
    key: impl Into<String>,
    expected_revision: Option<u64>,
    value: Value,
) -> Result<Option<PluginStateEntry>, Error> {
    plugin_state_call(&PluginStateRequest::CompareAndSet {
        scope,
        key: key.into(),
        expected_revision,
        value,
    })
}

#[cfg(target_family = "wasm")]
pub fn plugin_setting(key: &str) -> Result<Option<Value>, Error> {
    let output = unsafe { host::uri_agent_plugin_setting_get(key.to_string())? };
    let response: PluginSettingGetResponse = serde_json::from_str(&output)?;
    Ok(response.found.then_some(response.value))
}

#[cfg(target_family = "wasm")]
pub fn set_plugin_setting(key: &str, value: Value) -> Result<(), Error> {
    let input = serde_json::to_string(&PluginSettingSetRequest {
        key: key.to_string(),
        value,
    })?;
    unsafe { host::uri_agent_plugin_setting_set(input)? };
    Ok(())
}

#[cfg(target_family = "wasm")]
fn call_host(
    uri: &str,
    body: &str,
    call: unsafe fn(String) -> Result<String, Error>,
) -> Result<String, Error> {
    let input = serde_json::to_string(&HostRequest { uri, body })?;
    unsafe { call(input) }
}

/// Export the URI Agent plugin ABI for a manifest expression and request handler.
#[macro_export]
macro_rules! define_plugin {
    ($manifest:expr, $handler:path) => {
        #[cfg(target_family = "wasm")]
        use $crate::extism_pdk;

        #[cfg(target_family = "wasm")]
        #[$crate::plugin_fn]
        pub fn uri_agent_manifest() -> $crate::FnResult<$crate::Json<$crate::PluginManifest>> {
            Ok($crate::Json($manifest))
        }

        #[cfg(target_family = "wasm")]
        #[$crate::plugin_fn]
        pub fn uri_agent_handle(
            request: $crate::Json<$crate::HandlerRequest>,
        ) -> $crate::FnResult<Vec<u8>> {
            Ok($handler(request.0).map_err($crate::Error::msg)?)
        }

        #[cfg(not(target_family = "wasm"))]
        #[allow(dead_code)]
        fn _uri_agent_define_plugin_native_ref() {
            let _: $crate::PluginManifest = $manifest;
            let _: fn($crate::HandlerRequest) -> $crate::HandlerResult = $handler;
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_wire_types_use_the_v6_shape() {
        let manifest = PluginManifest::new([ProtocolDescriptor::new(
            "example",
            "Example protocol",
            true,
            false,
        )])
        .request_agent_access()
        .request_state_access()
        .with_resident();
        let value = serde_json::to_value(manifest).unwrap();
        assert_eq!(value["abi_version"], ABI_VERSION);
        assert_eq!(value["protocols"][0]["name"], "example");
        assert_eq!(value["permissions"]["agents"], true);
        assert_eq!(value["permissions"]["state"], true);
        assert_eq!(value["resident"], true);

        let spec = AgentSpec::new("openai", "gpt-5", "high", "/work", "parent")
            .replace_system_prompt("Review only.")
            .with_tools(["replace"])
            .with_protocols(std::iter::empty::<String>())
            .with_max_output_tokens(4096);
        assert_eq!(
            serde_json::to_value(&spec).unwrap(),
            serde_json::json!({
                "provider": "openai",
                "model": "gpt-5",
                "thinking": "high",
                "workingDirectory": "/work",
                "parentSessionId": "parent",
                "systemPrompt": {"mode": "replace", "content": "Review only."},
                "tools": {"mode": "only", "names": ["replace"]},
                "protocols": {"mode": "only", "names": []},
                "maxOutputTokens": 4096
            })
        );

        assert_eq!(
            serde_json::to_value(AgentRequest::Submit {
                handle: 7,
                text: "continue".to_string(),
                kind: SubmitKind::Steer,
            })
            .unwrap(),
            serde_json::json!({
                "operation": "submit",
                "handle": 7,
                "text": "continue",
                "kind": "steer"
            })
        );

        let event = HandlerRequest::Event {
            event: PluginEvent::Compacted {
                session_id: "child".to_string(),
                summary: "summary".to_string(),
                manual: false,
                spec: Box::new(spec),
            },
        };
        let value = serde_json::to_value(event).unwrap();
        assert_eq!(value["kind"], "event");
        assert_eq!(value["event"]["type"], "compacted");
        assert_eq!(value["event"]["session_id"], "child");
        assert_eq!(value["event"]["spec"]["model"], "gpt-5");
    }
}
