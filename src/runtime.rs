use crate::model::{ModelBackend, ModelDelta, ModelRequest, ModelResponse};
use crate::protocol::ProtocolRegistry;
use crate::session::{EventKind, Session};
use crate::task::TaskManager;
use anyhow::{Context, Result, anyhow, bail};
use rig::message::{AssistantContent, Message, Text, ToolCall, ToolResultContent, UserContent};
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};

const MAX_TOOL_ROUNDS: usize = 32;

pub struct AgentRuntime {
    backend: Arc<dyn ModelBackend>,
    protocols: Arc<ProtocolRegistry>,
    session: Session,
    system_prompt: String,
    turn: Mutex<()>,
}

impl AgentRuntime {
    pub fn new(
        backend: Arc<dyn ModelBackend>,
        protocols: Arc<ProtocolRegistry>,
        session: Session,
        system_prompt: String,
    ) -> Self {
        Self {
            backend,
            protocols,
            session,
            system_prompt,
            turn: Mutex::new(()),
        }
    }

    pub fn session(&self) -> &Session {
        &self.session
    }

    pub async fn run_turn(&self, prompt: String) -> Result<()> {
        let _turn = self.turn.lock().await;
        let prompt = prompt.trim();
        if prompt.is_empty() {
            return Ok(());
        }
        self.session
            .append(EventKind::User {
                text: prompt.to_string(),
            })
            .await?;
        self.append_model_message(Message::user(prompt)).await?;

        let result = self.run_tool_loop().await;
        match result {
            Ok(()) => {
                self.session.append(EventKind::TurnFinished).await?;
                Ok(())
            }
            Err(error) => {
                let text = format!("{error:#}");
                self.session
                    .append(EventKind::Error { text: text.clone() })
                    .await?;
                Err(anyhow!(text))
            }
        }
    }

    async fn run_tool_loop(&self) -> Result<()> {
        for _ in 0..MAX_TOOL_ROUNDS {
            let response = self.complete_once().await?;
            let assistant_message = Message::Assistant {
                id: None,
                content: response.content.clone(),
            };
            self.append_model_message(assistant_message).await?;

            let tool_calls = response
                .content
                .iter()
                .filter_map(|content| match content {
                    AssistantContent::ToolCall(call) => Some(call.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            if tool_calls.is_empty() {
                return Ok(());
            }
            for call in tool_calls {
                self.execute_tool(call).await?;
            }
        }
        bail!("model exceeded {MAX_TOOL_ROUNDS} consecutive tool rounds")
    }

    async fn complete_once(&self) -> Result<ModelResponse> {
        let request = ModelRequest {
            system: self.system_prompt.clone(),
            history: self.session.model_history().await,
        };
        let (deltas, mut receiver) = mpsc::unbounded_channel();
        let completion = self.backend.complete(request, deltas);
        tokio::pin!(completion);
        loop {
            tokio::select! {
                response = &mut completion => {
                    while let Ok(delta) = receiver.try_recv() {
                        self.append_delta(delta).await?;
                    }
                    return response;
                }
                delta = receiver.recv() => {
                    if let Some(delta) = delta {
                        self.append_delta(delta).await?;
                    }
                }
            }
        }
    }

    async fn append_delta(&self, delta: ModelDelta) -> Result<()> {
        self.session
            .append(match delta {
                ModelDelta::Text(text) => EventKind::AssistantText { text },
                ModelDelta::Reasoning(text) => EventKind::AssistantReasoning { text },
            })
            .await?;
        Ok(())
    }

    async fn execute_tool(&self, call: ToolCall) -> Result<()> {
        let name = call.function.name.clone();
        let call_id = call.id.to_string();
        self.session
            .append(EventKind::ToolCall {
                call_id: call_id.clone(),
                name: name.clone(),
                arguments: call.function.arguments.clone(),
            })
            .await?;

        let result = self.dispatch(&name, &call.function.arguments).await;
        let (output, failed) = match result {
            Ok(output) => (output, false),
            Err(error) => (format!("Error: {error:#}"), true),
        };
        self.session
            .append(EventKind::ToolResult {
                call_id,
                name: name.clone(),
                output: output.clone(),
                failed,
            })
            .await?;
        let result = UserContent::tool_result_for(
            call.id,
            call.provider,
            name,
            vec![ToolResultContent::Text(Text::new(output))],
        );
        self.append_model_message(Message::User {
            content: vec![result],
        })
        .await
    }

    async fn dispatch(&self, name: &str, arguments: &Value) -> Result<String> {
        let object = arguments
            .as_object()
            .ok_or_else(|| anyhow!("{name} arguments must be an object"))?;
        let uri = object
            .get("uri")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("{name} requires a uri string"))?;
        let body = object.get("body");
        match name {
            "read" => self.protocols.read(uri, body).await,
            "exec" => self.protocols.exec(uri, body).await,
            _ => bail!("unknown model tool: {name}"),
        }
    }

    async fn append_model_message(&self, message: Message) -> Result<()> {
        self.session
            .append(EventKind::ModelMessage { message })
            .await
            .context("cannot persist model history")?;
        Ok(())
    }
}

pub fn forward_task_notices(session: Session, tasks: TaskManager) {
    let mut notices = tasks.subscribe();
    tokio::spawn(async move {
        loop {
            match notices.recv().await {
                Ok(notice) => {
                    let _ = session
                        .append(EventKind::Task {
                            id: notice.id,
                            protocol: notice.protocol,
                            label: notice.label,
                            status: notice.status,
                        })
                        .await;
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ModelDelta;
    use async_trait::async_trait;
    use rig::message::{ToolCallId, ToolFunction};
    use std::collections::VecDeque;

    struct FakeBackend {
        responses: Mutex<VecDeque<Vec<AssistantContent>>>,
    }

    #[async_trait]
    impl ModelBackend for FakeBackend {
        async fn complete(
            &self,
            _request: ModelRequest,
            deltas: mpsc::UnboundedSender<ModelDelta>,
        ) -> Result<ModelResponse> {
            let content = self.responses.lock().await.pop_front().unwrap();
            for part in &content {
                if let AssistantContent::Text(text) = part {
                    let _ = deltas.send(ModelDelta::Text(text.text.clone()));
                }
            }
            Ok(ModelResponse { content })
        }
    }

    #[test]
    fn arbitrary_body_values_are_not_reencoded_by_argument_parsing() {
        let value = serde_json::json!({"uri": "mock://target", "body": [1, {"raw": true}, null]});
        let body = value.as_object().unwrap().get("body").unwrap();
        assert_eq!(body, &serde_json::json!([1, {"raw": true}, null]));
    }

    #[test]
    fn fake_tool_call_can_retain_provider_correlation() {
        let call = ToolCall::new(
            ToolCallId::new("call-1").unwrap(),
            ToolFunction::new(
                "read".to_string(),
                serde_json::json!({"uri": "file://help"}),
            ),
        );
        assert_eq!(call.id.as_str(), "call-1");
    }

    #[tokio::test]
    async fn fake_backend_completes_a_read_tool_loop_end_to_end() {
        let workspace = tempfile::tempdir().unwrap();
        let session_id = format!("test{}", uuid::Uuid::now_v7().simple());
        let session = crate::session::Session::open(
            Some(&session_id),
            workspace.path(),
            "fake",
            "fake-model",
        )
        .await
        .unwrap();
        let output = Arc::new(
            crate::output::OutputStore::new(&session_id, 32 * 1024)
                .await
                .unwrap(),
        );
        let output_directory = output.directory().to_path_buf();
        let tasks = TaskManager::new();
        let mut protocols = ProtocolRegistry::new(output, tasks);
        protocols
            .register(crate::builtins::FileProtocol::new(workspace.path()))
            .unwrap();
        let call = ToolCall::new(
            ToolCallId::new("read-help").unwrap(),
            ToolFunction::new(
                "read".to_string(),
                serde_json::json!({"uri": "file://help"}),
            ),
        );
        let backend = Arc::new(FakeBackend {
            responses: Mutex::new(VecDeque::from([
                vec![AssistantContent::ToolCall(call)],
                vec![AssistantContent::text("Done")],
            ])),
        });
        let runtime = AgentRuntime::new(
            backend,
            Arc::new(protocols),
            session.clone(),
            "test system prompt".to_string(),
        );

        runtime.run_turn("Read the help".to_string()).await.unwrap();

        let events = session.snapshot().await;
        assert!(events.iter().any(|event| matches!(
            &event.kind,
            EventKind::ToolResult { name, output, failed: false, .. }
                if name == "read" && output.contains("# file")
        )));
        assert!(matches!(
            events.last().map(|event| &event.kind),
            Some(EventKind::TurnFinished)
        ));
        assert_eq!(session.model_history().await.len(), 4);

        let session_directory = session.directory().to_path_buf();
        drop(runtime);
        drop(session);
        let _ = tokio::fs::remove_dir_all(session_directory).await;
        let _ = tokio::fs::remove_dir_all(output_directory).await;
    }
}
