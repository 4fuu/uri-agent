use crate::plugin::{ModelTool, ModelToolDescriptor, ModelToolOutput, Plugin, PluginHost};
use crate::prompts;
use crate::protocol::ProtocolRegistry;
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Clone, Copy)]
enum ProtocolOperation {
    Read,
    Exec,
}

#[derive(Clone)]
struct ProtocolTool {
    operation: ProtocolOperation,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProtocolArguments {
    uri: String,
    body: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HelpArguments {
    protocols: Vec<String>,
}

struct HelpTool;

impl ProtocolTool {
    fn new(operation: ProtocolOperation) -> Self {
        Self { operation }
    }

    fn name(&self) -> &'static str {
        match self.operation {
            ProtocolOperation::Read => "read",
            ProtocolOperation::Exec => "exec",
        }
    }
}

#[async_trait]
impl ModelTool for ProtocolTool {
    fn descriptor(&self) -> ModelToolDescriptor {
        ModelToolDescriptor {
            name: self.name().to_string(),
            description: match self.operation {
                ProtocolOperation::Read => prompts::READ_TOOL_DESCRIPTION,
                ProtocolOperation::Exec => prompts::EXEC_TOOL_DESCRIPTION,
            }
            .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "uri": {
                        "type": "string",
                        "description": "Protocol address in the custom form <protocol>://<opaque-target>. It is not an RFC URL and is passed to the selected protocol unchanged."
                    },
                    "body": {
                        "type": "string",
                        "description": "Protocol-specific string body. Use an empty string when the operation takes no body, plain text for textual input such as a command or a search pattern, and complete serialized JSON text when the protocol requires structured input."
                    }
                },
                "required": ["uri", "body"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(
        &self,
        arguments: &Value,
        protocols: &ProtocolRegistry,
    ) -> Result<ModelToolOutput> {
        let arguments: ProtocolArguments = serde_json::from_value(arguments.clone())
            .map_err(|error| anyhow!("invalid {} arguments: {error}", self.name()))?;
        match self.operation {
            ProtocolOperation::Read => {
                let result = protocols
                    .read_for_model(&arguments.uri, &arguments.body)
                    .await?;
                Ok(ModelToolOutput::new(result.output, result.images))
            }
            ProtocolOperation::Exec => protocols
                .exec(&arguments.uri, &arguments.body)
                .await
                .map(Into::into),
        }
    }
}

#[async_trait]
impl ModelTool for HelpTool {
    fn descriptor(&self) -> ModelToolDescriptor {
        ModelToolDescriptor {
            name: "help".to_string(),
            description: prompts::HELP_TOOL_DESCRIPTION.to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "protocols": {
                        "type": "array",
                        "items": { "type": "string" },
                        "minItems": 1,
                        "description": "Names of protocols to load from the Available protocols list, for example [\"file\", \"search\"]. Shared prerequisites such as the MCP routing page are included automatically."
                    }
                },
                "required": ["protocols"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(
        &self,
        arguments: &Value,
        protocols: &ProtocolRegistry,
    ) -> Result<ModelToolOutput> {
        let arguments: HelpArguments = serde_json::from_value(arguments.clone())
            .map_err(|error| anyhow!("invalid help arguments: {error}"))?;
        Ok(protocols.load_help(&arguments.protocols).await?.into())
    }
}

pub(super) struct ProtocolToolsPlugin;

pub(crate) fn register_protocol_tools(
    registry: &mut crate::plugin::ModelToolRegistry,
) -> Result<()> {
    registry.register(ProtocolTool::new(ProtocolOperation::Read))?;
    registry.register(ProtocolTool::new(ProtocolOperation::Exec))?;
    registry.register(HelpTool)
}

impl Plugin for ProtocolToolsPlugin {
    fn model_tool_descriptors(&self) -> Vec<ModelToolDescriptor> {
        [
            ProtocolTool::new(ProtocolOperation::Read).descriptor(),
            ProtocolTool::new(ProtocolOperation::Exec).descriptor(),
            HelpTool.descriptor(),
        ]
        .to_vec()
    }

    fn register(&self, host: &mut PluginHost<'_>) -> Result<()> {
        register_protocol_tools(host.model_tools)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::OutputStore;
    use crate::protocol::{Protocol, ProtocolContext, ProtocolDescriptor, ProtocolRequest};
    use crate::task::TaskManager;

    struct CaptureProtocol;

    #[async_trait]
    impl Protocol for CaptureProtocol {
        fn descriptor(&self) -> ProtocolDescriptor {
            ProtocolDescriptor {
                name: "capture".to_string(),
                description: "Capture test protocol".to_string(),
                can_read: true,
                can_exec: true,
            }
        }

        async fn read(
            &self,
            request: ProtocolRequest<'_>,
            _context: ProtocolContext,
        ) -> Result<Vec<u8>> {
            Ok(format!("read:{}", request.body).into_bytes())
        }

        async fn exec(
            &self,
            request: ProtocolRequest<'_>,
            _context: ProtocolContext,
        ) -> Result<Vec<u8>> {
            Ok(format!("exec:{}", request.body).into_bytes())
        }
    }

    async fn protocols() -> (ProtocolRegistry, std::path::PathBuf) {
        let session_id = format!("model-tools-{}", uuid::Uuid::now_v7().simple());
        let output = std::sync::Arc::new(OutputStore::new(&session_id, 1024).await.unwrap());
        let directory = output.directory().to_path_buf();
        let mut protocols = ProtocolRegistry::new(output, TaskManager::new());
        protocols.register(CaptureProtocol).unwrap();
        (protocols, directory)
    }

    #[test]
    fn protocol_tools_require_a_plain_string_body() {
        for operation in [ProtocolOperation::Read, ProtocolOperation::Exec] {
            let descriptor = ProtocolTool::new(operation).descriptor();
            assert_eq!(descriptor.parameters["required"], json!(["uri", "body"]));
            assert_eq!(
                descriptor.parameters["properties"]["body"]["type"],
                "string"
            );
        }
    }

    #[test]
    fn help_tool_describes_a_nonempty_protocol_list() {
        let descriptor = HelpTool.descriptor();
        assert_eq!(descriptor.name, "help");
        assert_eq!(descriptor.parameters["required"], json!(["protocols"]));
        assert_eq!(
            descriptor.parameters["properties"]["protocols"]["minItems"],
            json!(1)
        );
    }

    #[tokio::test]
    async fn help_tool_loads_protocols_and_unlocks_model_calls() {
        let (protocols, output) = protocols().await;
        let loaded = HelpTool
            .execute(&json!({"protocols": ["capture"]}), &protocols)
            .await
            .unwrap();
        assert!(loaded.output().contains("Loaded protocols stay loaded"));

        let read = ProtocolTool::new(ProtocolOperation::Read)
            .execute(&json!({"uri": "capture://value", "body": ""}), &protocols)
            .await
            .unwrap();
        let exec = ProtocolTool::new(ProtocolOperation::Exec)
            .execute(
                &json!({"uri": "capture://value", "body": "{\"answer\":42}"}),
                &protocols,
            )
            .await
            .unwrap();

        assert_eq!(read.output(), "read:");
        assert_eq!(exec.output(), "exec:{\"answer\":42}");
        let _ = tokio::fs::remove_dir_all(output).await;
    }

    #[tokio::test]
    async fn help_tool_rejects_malformed_and_unknown_requests() {
        let (protocols, output) = protocols().await;
        let oversized: Vec<String> = ('a'..='j').map(|c| c.to_string()).collect();
        let error = HelpTool
            .execute(&json!({"protocols": oversized}), &protocols)
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("unknown protocol: a"));

        let error = HelpTool
            .execute(&json!({"protocols": "capture"}), &protocols)
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("invalid help arguments"));
        let _ = tokio::fs::remove_dir_all(output).await;
    }

    #[tokio::test]
    async fn protocol_tools_reject_non_string_body_arguments() {
        let (protocols, output) = protocols().await;
        let error = ProtocolTool::new(ProtocolOperation::Read)
            .execute(
                &json!({"uri": "capture://value", "body": {"answer": 42}}),
                &protocols,
            )
            .await
            .unwrap_err();

        assert!(format!("{error:#}").contains("invalid read arguments"));
        assert!(format!("{error:#}").contains("string"));
        let _ = tokio::fs::remove_dir_all(output).await;
    }
}
