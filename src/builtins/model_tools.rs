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
                        "description": "Protocol-specific string body. Use an empty string when the protocol takes no body; serialize structured protocol input as complete JSON text."
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

pub(super) struct ProtocolToolsPlugin;

pub(crate) fn register_protocol_tools(
    registry: &mut crate::plugin::ModelToolRegistry,
) -> Result<()> {
    registry.register(ProtocolTool::new(ProtocolOperation::Read))?;
    registry.register(ProtocolTool::new(ProtocolOperation::Exec))
}

impl Plugin for ProtocolToolsPlugin {
    fn model_tool_descriptors(&self) -> Vec<ModelToolDescriptor> {
        [ProtocolOperation::Read, ProtocolOperation::Exec]
            .into_iter()
            .map(|operation| ProtocolTool::new(operation).descriptor())
            .collect()
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

    #[tokio::test]
    async fn protocol_tools_dispatch_string_bodies_without_transforming_empty_input() {
        let (protocols, output) = protocols().await;
        ProtocolTool::new(ProtocolOperation::Read)
            .execute(&json!({"uri": "capture://help", "body": ""}), &protocols)
            .await
            .unwrap();
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
