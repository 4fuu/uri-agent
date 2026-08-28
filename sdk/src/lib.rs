//! Rust guest types and host calls for URI Agent WebAssembly protocol plugins.
//!
//! ABI types and constants are available on every target so the host can share
//! the wire format. Extism guest exports and host-call wrappers compile only
//! for WebAssembly guests.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[cfg(target_family = "wasm")]
#[doc(hidden)]
pub use extism_pdk;
#[cfg(target_family = "wasm")]
pub use extism_pdk::{plugin_fn, Error, FnResult, Json};

pub const ABI_VERSION: u32 = 5;
pub const MANIFEST_EXPORT: &str = "uri_agent_manifest";
pub const HANDLE_EXPORT: &str = "uri_agent_handle";
pub const HOST_NAMESPACE: &str = "extism:host/user";
pub const HOST_READ: &str = "uri_agent_read";
pub const HOST_EXEC: &str = "uri_agent_exec";
pub const HOST_ENVIRONMENT: &str = "uri_agent_environment";
pub const HOST_CREDENTIALS: &str = "uri_agent_credentials";
pub const HOST_MODEL_ROLE: &str = "uri_agent_model_role";
pub const HOST_SUBAGENT: &str = "uri_agent_subagent";
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
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SubagentRequest {
    pub role: String,
    pub prompt: String,
    pub system_prompt: SubagentSystemPrompt,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocols: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_directory: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

impl SubagentRequest {
    pub fn new(role: impl Into<String>, prompt: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            prompt: prompt.into(),
            system_prompt: SubagentSystemPrompt::Append(String::new()),
            tools: None,
            protocols: None,
            working_directory: None,
            max_output_tokens: None,
            timeout_ms: None,
        }
    }

    pub fn append_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = SubagentSystemPrompt::Append(prompt.into());
        self
    }

    pub fn replace_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = SubagentSystemPrompt::Replace(prompt.into());
        self
    }

    pub fn with_tools(mut self, tools: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.tools = Some(tools.into_iter().map(Into::into).collect());
        self
    }

    pub fn with_protocols(
        mut self,
        protocols: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.protocols = Some(protocols.into_iter().map(Into::into).collect());
        self
    }

    pub fn with_working_directory(mut self, path: impl Into<String>) -> Self {
        self.working_directory = Some(path.into());
        self
    }

    pub fn with_max_output_tokens(mut self, max_output_tokens: usize) -> Self {
        self.max_output_tokens = Some(max_output_tokens);
        self
    }

    pub fn with_timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.timeout_ms = Some(timeout_ms);
        self
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "mode", content = "content", rename_all = "snake_case")]
pub enum SubagentSystemPrompt {
    Append(String),
    Replace(String),
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

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SubagentUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub cost: f64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SubagentResponse {
    pub text: String,
    pub role: String,
    pub provider: String,
    pub model: String,
    pub thinking: String,
    pub usage: Option<SubagentUsage>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PluginManifest {
    pub abi_version: u32,
    pub protocols: Vec<ProtocolDescriptor>,
    pub model_tools: Vec<ModelToolDescriptor>,
    pub permissions: PluginPermissions,
}

impl PluginManifest {
    pub fn new(protocols: impl IntoIterator<Item = ProtocolDescriptor>) -> Self {
        Self {
            abi_version: ABI_VERSION,
            protocols: protocols.into_iter().collect(),
            model_tools: Vec::new(),
            permissions: PluginPermissions::default(),
        }
    }

    /// Register direct model tools with typed JSON arguments.
    pub fn with_model_tools(
        mut self,
        tools: impl IntoIterator<Item = ModelToolDescriptor>,
    ) -> Self {
        self.model_tools = tools.into_iter().collect();
        self
    }

    /// Request access to user-managed Agent environment variables.
    ///
    /// This explicit declaration is an audit marker for trusted plugin source;
    /// URI Agent does not present an interactive approval prompt.
    pub fn request_environment_access(mut self) -> Self {
        self.permissions.environment = true;
        self
    }

    /// Request read access to saved and provider-environment API keys.
    ///
    /// This explicit declaration is an audit marker for trusted plugin source;
    /// URI Agent does not present an interactive approval prompt.
    pub fn request_credentials_access(mut self) -> Self {
        self.permissions.credentials = true;
        self
    }

    /// Request access to bounded, ephemeral model/tool loops through configured
    /// roles. Plugins cannot select a provider or model directly.
    pub fn request_subagent_access(mut self) -> Self {
        self.permissions.subagents = true;
        self
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PluginPermissions {
    pub environment: bool,
    pub credentials: bool,
    #[serde(default)]
    pub subagents: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProtocolDescriptor {
    pub name: String,
    pub description: String,
    /// Must be true so the agent can read `<protocol>://help` before first use.
    pub can_read: bool,
    pub can_exec: bool,
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
}

/// Result returned by a plugin request handler.
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
        pub fn uri_agent_subagent(input: String) -> String;
        pub fn uri_agent_plugin_setting_get(input: String) -> String;
        pub fn uri_agent_plugin_setting_set(input: String) -> String;
    }
}

/// Read through one of URI Agent's built-in protocols.
#[cfg(target_family = "wasm")]
pub fn read(uri: &str, body: &str) -> Result<String, Error> {
    call_host(uri, body, host::uri_agent_read)
}

/// Execute through one of URI Agent's built-in protocols.
#[cfg(target_family = "wasm")]
pub fn exec(uri: &str, body: &str) -> Result<String, Error> {
    call_host(uri, body, host::uri_agent_exec)
}

/// Read one user-managed environment variable after requesting environment
/// access in the plugin manifest.
#[cfg(target_family = "wasm")]
pub fn environment_variable(name: &str) -> Result<Option<String>, Error> {
    let output = unsafe { host::uri_agent_environment(name.to_string())? };
    Ok(serde_json::from_str(&output)?)
}

/// Resolve a saved or provider-environment API key after requesting credential
/// access in the plugin manifest.
#[cfg(target_family = "wasm")]
pub fn provider_api_key(provider: &str) -> Result<Option<String>, Error> {
    let output = unsafe { host::uri_agent_credentials(provider.to_string())? };
    Ok(serde_json::from_str(&output)?)
}

/// Resolve a configured model role. Model-role access does not expose a
/// credential and requires no manifest permission.
#[cfg(target_family = "wasm")]
pub fn model_role(name: &str) -> Result<Option<ModelRole>, Error> {
    let output = unsafe { host::uri_agent_model_role(name.to_string())? };
    Ok(serde_json::from_str(&output)?)
}

/// Run one bounded, ephemeral model/tool loop through a configured role after
/// requesting subagent access in the plugin manifest.
#[cfg(target_family = "wasm")]
pub fn subagent(request: &SubagentRequest) -> Result<SubagentResponse, Error> {
    let input = serde_json::to_string(request)?;
    let output = unsafe { host::uri_agent_subagent(input)? };
    Ok(serde_json::from_str(&output)?)
}

/// Read one value from this plugin module's persistent settings namespace.
#[cfg(target_family = "wasm")]
pub fn plugin_setting(key: &str) -> Result<Option<Value>, Error> {
    let output = unsafe { host::uri_agent_plugin_setting_get(key.to_string())? };
    let response: PluginSettingGetResponse = serde_json::from_str(&output)?;
    Ok(response.found.then_some(response.value))
}

/// Persist one value in this plugin module's settings namespace.
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
///
/// Guest exports require a WebAssembly target. Native checks keep the manifest
/// and handler referenced so workspace clippy can type-check plugin crates.
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
    fn public_wire_types_use_the_extism_abi_shape() {
        let manifest = PluginManifest::new([ProtocolDescriptor::new(
            "example",
            "Example protocol",
            true,
            false,
        )]);
        let value = serde_json::to_value(manifest).unwrap();
        assert_eq!(value["abi_version"], ABI_VERSION);
        assert_eq!(value["protocols"][0]["name"], "example");
        assert_eq!(value["model_tools"], serde_json::json!([]));
        assert_eq!(value["permissions"]["environment"], false);
        assert_eq!(value["permissions"]["credentials"], false);
        assert_eq!(value["permissions"]["subagents"], false);

        let manifest = PluginManifest::new([]).request_environment_access();
        let value = serde_json::to_value(manifest).unwrap();
        assert_eq!(value["permissions"]["environment"], true);
        assert_eq!(value["permissions"]["credentials"], false);
        assert_eq!(value["permissions"]["subagents"], false);
        let manifest = PluginManifest::new([]).request_credentials_access();
        let value = serde_json::to_value(manifest).unwrap();
        assert_eq!(value["permissions"]["environment"], false);
        assert_eq!(value["permissions"]["credentials"], true);
        assert_eq!(value["permissions"]["subagents"], false);
        let manifest = PluginManifest::new([]).request_subagent_access();
        let value = serde_json::to_value(manifest).unwrap();
        assert_eq!(value["permissions"]["environment"], false);
        assert_eq!(value["permissions"]["credentials"], false);
        assert_eq!(value["permissions"]["subagents"], true);
        let incomplete = serde_json::from_value::<PluginManifest>(serde_json::json!({
            "abi_version": ABI_VERSION,
            "protocols": []
        }));
        assert!(incomplete.is_err());

        let request: HandlerRequest = serde_json::from_value(serde_json::json!({
            "kind": "model_tool",
            "name": "example_tool",
            "arguments": {"answer": 42}
        }))
        .unwrap();
        assert!(matches!(
            request,
            HandlerRequest::ModelTool { name, arguments }
                if name == "example_tool" && arguments["answer"] == 42
        ));

        let role = ModelRole {
            provider: "openai".to_string(),
            model: "gpt-5".to_string(),
            thinking: "low".to_string(),
        };
        assert_eq!(
            serde_json::to_value(role).unwrap(),
            serde_json::json!({
                "provider": "openai",
                "model": "gpt-5",
                "thinking": "low"
            })
        );

        let request = SubagentRequest::new("small", "Fix parser")
            .with_tools(std::iter::empty::<String>())
            .with_protocols(std::iter::empty::<String>())
            .replace_system_prompt("Create a title.");
        assert_eq!(
            serde_json::to_value(request).unwrap(),
            serde_json::json!({
                "role": "small",
                "prompt": "Fix parser",
                "systemPrompt": {"mode": "replace", "content": "Create a title."},
                "tools": [],
                "protocols": []
            })
        );
    }
}
