//! Rust guest types and host calls for URI Agent WebAssembly protocol plugins.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[doc(hidden)]
pub use extism_pdk;
pub use extism_pdk::{plugin_fn, Error, FnResult, Json};

pub const ABI_VERSION: u32 = 1;
pub const MANIFEST_EXPORT: &str = "uri_agent_manifest";
pub const HANDLE_EXPORT: &str = "uri_agent_handle";
pub const HOST_NAMESPACE: &str = "extism:host/user";
pub const HOST_READ: &str = "uri_agent_read";
pub const HOST_EXEC: &str = "uri_agent_exec";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PluginManifest {
    pub abi_version: u32,
    #[serde(default)]
    pub protocols: Vec<ProtocolDescriptor>,
}

impl PluginManifest {
    pub fn new(protocols: impl IntoIterator<Item = ProtocolDescriptor>) -> Self {
        Self {
            abi_version: ABI_VERSION,
            protocols: protocols.into_iter().collect(),
        }
    }
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
#[serde(deny_unknown_fields)]
pub struct HandlerRequest {
    pub protocol: String,
    pub operation: Operation,
    pub uri: String,
    pub target: String,
    pub body: Option<Value>,
}

/// Result returned by a plugin request handler.
pub type HandlerResult = Result<Vec<u8>, String>;

#[derive(Serialize)]
struct HostRequest<'a> {
    uri: &'a str,
    body: Option<&'a Value>,
}

mod host {
    use extism_pdk::*;

    #[host_fn("extism:host/user")]
    extern "ExtismHost" {
        pub fn uri_agent_read(input: String) -> String;
        pub fn uri_agent_exec(input: String) -> String;
    }
}

/// Read through one of URI Agent's built-in protocols.
pub fn read(uri: &str, body: Option<&Value>) -> Result<String, Error> {
    call_host(uri, body, host::uri_agent_read)
}

/// Execute through one of URI Agent's built-in protocols.
pub fn exec(uri: &str, body: Option<&Value>) -> Result<String, Error> {
    call_host(uri, body, host::uri_agent_exec)
}

fn call_host(
    uri: &str,
    body: Option<&Value>,
    call: unsafe fn(String) -> Result<String, Error>,
) -> Result<String, Error> {
    let input = serde_json::to_string(&HostRequest { uri, body })?;
    unsafe { call(input) }
}

/// Export the URI Agent plugin ABI for a manifest expression and request handler.
#[macro_export]
macro_rules! define_plugin {
    ($manifest:expr, $handler:path) => {
        use $crate::extism_pdk;

        #[$crate::plugin_fn]
        pub fn uri_agent_manifest() -> $crate::FnResult<$crate::Json<$crate::PluginManifest>> {
            Ok($crate::Json($manifest))
        }

        #[$crate::plugin_fn]
        pub fn uri_agent_handle(
            request: $crate::Json<$crate::HandlerRequest>,
        ) -> $crate::FnResult<Vec<u8>> {
            Ok($handler(request.0).map_err($crate::Error::msg)?)
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

        let request: HandlerRequest = serde_json::from_value(serde_json::json!({
            "protocol": "example",
            "operation": "read",
            "uri": "example://a://b",
            "target": "a://b",
            "body": {"answer": 42}
        }))
        .unwrap();
        assert_eq!(request.operation, Operation::Read);
        assert_eq!(request.body.unwrap()["answer"], 42);
    }
}
