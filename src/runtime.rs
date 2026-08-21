use crate::compaction;
use crate::model::{ModelBackend, ModelDelta, ModelRequest, ModelResponse};
use crate::protocol::ProtocolRegistry;
use crate::session::{EventKind, Session};
use crate::task::TaskManager;
use anyhow::{Context, Result, anyhow, bail};
use rig::message::{AssistantContent, Message, Text, ToolCall, ToolResultContent, UserContent};
use serde_json::Value;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::{Mutex, RwLock, mpsc};

const MAX_TOOL_ROUNDS: usize = 32;

pub struct AgentRuntime {
    backend: RwLock<Option<Arc<dyn ModelBackend>>>,
    protocols: Arc<ProtocolRegistry>,
    session: Session,
    system_prompt: String,
    context_window: AtomicUsize,
    turn: Mutex<()>,
}

impl AgentRuntime {
    pub fn new(
        backend: Option<Arc<dyn ModelBackend>>,
        protocols: Arc<ProtocolRegistry>,
        session: Session,
        system_prompt: String,
        context_window: usize,
    ) -> Self {
        Self {
            backend: RwLock::new(backend),
            protocols,
            session,
            system_prompt,
            context_window: AtomicUsize::new(context_window),
            turn: Mutex::new(()),
        }
    }

    pub fn session(&self) -> &Session {
        &self.session
    }

    pub async fn set_backend(&self, backend: Option<Arc<dyn ModelBackend>>, context_window: usize) {
        *self.backend.write().await = backend;
        self.context_window
            .store(context_window.max(1), Ordering::Relaxed);
    }

    pub async fn has_backend(&self) -> bool {
        self.backend.read().await.is_some()
    }

    pub async fn compact(&self) -> Result<()> {
        let _turn = self.turn.lock().await;
        let backend = self.backend.read().await.clone().ok_or_else(|| {
            anyhow!("no API key configured; open Settings with F2 or enter /login")
        })?;
        if !self.compact_with(backend.as_ref(), true).await? {
            bail!("not enough completed history to compact")
        }
        Ok(())
    }

    pub async fn run_turn(&self, prompt: String) -> Result<()> {
        let _turn = self.turn.lock().await;
        let prompt = prompt.trim();
        if prompt.is_empty() {
            return Ok(());
        }
        let backend = match self.backend.read().await.clone() {
            Some(backend) => backend,
            None => {
                let text = "no API key configured; open Settings with F2 or enter /login";
                self.session
                    .append(EventKind::Error {
                        text: text.to_string(),
                    })
                    .await?;
                return Err(anyhow!(text));
            }
        };
        self.session
            .append(EventKind::User {
                text: prompt.to_string(),
            })
            .await?;
        self.append_model_message(Message::user(prompt)).await?;

        let result = self.run_tool_loop(backend).await;
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

    async fn run_tool_loop(&self, backend: Arc<dyn ModelBackend>) -> Result<()> {
        for _ in 0..MAX_TOOL_ROUNDS {
            self.compact_with(backend.as_ref(), false).await?;
            let response = self.complete_once(backend.as_ref()).await?;
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

    async fn compact_with(&self, backend: &dyn ModelBackend, force: bool) -> Result<bool> {
        let history = self.session.model_history().await;
        let context_window = self.context_window.load(Ordering::Relaxed).max(1);
        if !force && !compaction::should_compact(&self.system_prompt, &history, context_window) {
            return Ok(false);
        }
        let Some(preparation) =
            compaction::prepare(&self.system_prompt, &history, context_window, force)
        else {
            return Ok(false);
        };
        let request = ModelRequest {
            system: format!(
                "{}\n\nYou are producing a context checkpoint. Follow the final checkpoint request and return only its summary.",
                self.system_prompt
            ),
            history: compaction::summary_history(&preparation),
            tools: false,
        };
        let (deltas, _receiver) = mpsc::unbounded_channel();
        let response = backend
            .complete(request, deltas)
            .await
            .context("context compaction model request failed")?;
        let summary = response
            .content
            .iter()
            .filter_map(|content| match content {
                AssistantContent::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        if summary.trim().is_empty() {
            bail!("context compaction returned no summary text");
        }
        let replacement = compaction::replacement_history(&summary, &preparation.retained);
        self.session
            .append_compaction(summary, preparation.tokens_before, replacement, force)
            .await?;
        Ok(true)
    }

    async fn complete_once(&self, backend: &dyn ModelBackend) -> Result<ModelResponse> {
        let request = ModelRequest {
            system: self.system_prompt.clone(),
            history: self.session.model_history().await,
            tools: true,
        };
        let (deltas, mut receiver) = mpsc::unbounded_channel();
        let completion = backend.complete(request, deltas);
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
    use crate::session::SessionContext;
    use async_trait::async_trait;
    use rig::message::{ToolCallId, ToolFunction};
    use std::collections::VecDeque;

    struct FakeBackend {
        responses: Mutex<VecDeque<Vec<AssistantContent>>>,
        requests: Mutex<Vec<ModelRequest>>,
    }

    #[async_trait]
    impl ModelBackend for FakeBackend {
        async fn complete(
            &self,
            request: ModelRequest,
            deltas: mpsc::UnboundedSender<ModelDelta>,
        ) -> Result<ModelResponse> {
            self.requests.lock().await.push(request);
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
    async fn missing_backend_reports_an_error_without_polluting_model_history() {
        let workspace = tempfile::tempdir().unwrap();
        let session_id = format!("test{}", uuid::Uuid::now_v7().simple());
        let session = crate::session::Session::open_at(
            workspace.path().join("sessions.db"),
            Some(&session_id),
            workspace.path(),
            "openai",
            "gpt-5.2",
            SessionContext {
                system_prompt: "system".to_string(),
                skills: Vec::new(),
            },
        )
        .await
        .unwrap();
        let output = Arc::new(
            crate::output::OutputStore::new(session.id(), 32 * 1024)
                .await
                .unwrap(),
        );
        let protocols = ProtocolRegistry::new(output.clone(), TaskManager::new());
        let runtime = AgentRuntime::new(
            None,
            Arc::new(protocols),
            session.clone(),
            "system".to_string(),
            128_000,
        );

        assert!(runtime.run_turn("hello".to_string()).await.is_err());
        assert!(session.model_history().await.is_empty());
        let events = session.snapshot().await;
        assert!(
            !events
                .iter()
                .any(|event| matches!(event.kind, EventKind::User { .. }))
        );
        assert!(
            matches!(events.last().map(|event| &event.kind), Some(EventKind::Error { text }) if text.contains("F2"))
        );

        let _ = tokio::fs::remove_dir_all(output.directory()).await;
    }

    #[tokio::test]
    async fn fake_backend_completes_a_read_tool_loop_end_to_end() {
        let workspace = tempfile::tempdir().unwrap();
        let session_id = format!("test{}", uuid::Uuid::now_v7().simple());
        let session = crate::session::Session::open_at(
            workspace.path().join("sessions.db"),
            Some(&session_id),
            workspace.path(),
            "fake",
            "fake-model",
            SessionContext {
                system_prompt: "test system prompt".to_string(),
                skills: Vec::new(),
            },
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
            requests: Mutex::new(Vec::new()),
        });
        let runtime = AgentRuntime::new(
            Some(backend),
            Arc::new(protocols),
            session.clone(),
            "test system prompt".to_string(),
            128_000,
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

        drop(runtime);
        drop(session);
        let _ = tokio::fs::remove_dir_all(output_directory).await;
    }

    #[tokio::test]
    async fn manual_compaction_persists_a_checkpoint_and_keeps_raw_history() {
        let workspace = tempfile::tempdir().unwrap();
        let session_id = format!("test{}", uuid::Uuid::now_v7().simple());
        let session = crate::session::Session::open_at(
            workspace.path().join("sessions.db"),
            Some(&session_id),
            workspace.path(),
            "fake",
            "fake-model",
            SessionContext {
                system_prompt: "frozen system".to_string(),
                skills: Vec::new(),
            },
        )
        .await
        .unwrap();
        for message in [
            Message::user("first task"),
            Message::assistant("first answer"),
            Message::user("current task"),
            Message::assistant("current answer"),
        ] {
            session
                .append(EventKind::ModelMessage { message })
                .await
                .unwrap();
        }
        let output = Arc::new(
            crate::output::OutputStore::new(&session_id, 32 * 1024)
                .await
                .unwrap(),
        );
        let output_directory = output.directory().to_path_buf();
        let backend = Arc::new(FakeBackend {
            responses: Mutex::new(VecDeque::from([vec![AssistantContent::text(
                "The first task is complete; continue the current task.",
            )]])),
            requests: Mutex::new(Vec::new()),
        });
        let runtime = AgentRuntime::new(
            Some(backend.clone()),
            Arc::new(ProtocolRegistry::new(output, TaskManager::new())),
            session.clone(),
            "frozen system".to_string(),
            128_000,
        );

        runtime.compact().await.unwrap();

        let events = session.snapshot().await;
        assert!(events.iter().any(|event| matches!(
            &event.kind,
            EventKind::Compaction { summary, tokens_before, .. }
                if summary.contains("first task") && *tokens_before > 0
        )));
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(&event.kind, EventKind::ModelMessage { .. }))
                .count(),
            4
        );
        let replay = session.model_history().await;
        assert_eq!(replay.len(), 3);
        assert!(
            serde_json::to_string(&replay[0])
                .unwrap()
                .contains("first task")
        );
        assert!(!backend.requests.lock().await[0].tools);

        let _ = tokio::fs::remove_dir_all(output_directory).await;
    }

    #[tokio::test]
    async fn a_turn_compacts_automatically_before_the_overflowing_model_request() {
        let workspace = tempfile::tempdir().unwrap();
        let session_id = format!("test{}", uuid::Uuid::now_v7().simple());
        let session = crate::session::Session::open_at(
            workspace.path().join("sessions.db"),
            Some(&session_id),
            workspace.path(),
            "fake",
            "fake-model",
            SessionContext {
                system_prompt: "frozen system".to_string(),
                skills: Vec::new(),
            },
        )
        .await
        .unwrap();
        for message in [Message::user("old task"), Message::assistant("old answer")] {
            session
                .append(EventKind::ModelMessage { message })
                .await
                .unwrap();
        }
        let output = Arc::new(
            crate::output::OutputStore::new(&session_id, 32 * 1024)
                .await
                .unwrap(),
        );
        let output_directory = output.directory().to_path_buf();
        let backend = Arc::new(FakeBackend {
            responses: Mutex::new(VecDeque::from([
                vec![AssistantContent::text("Old work is complete.")],
                vec![AssistantContent::text("Current answer")],
            ])),
            requests: Mutex::new(Vec::new()),
        });
        let runtime = AgentRuntime::new(
            Some(backend.clone()),
            Arc::new(ProtocolRegistry::new(output, TaskManager::new())),
            session.clone(),
            "frozen system".to_string(),
            64,
        );

        runtime.run_turn("current task".to_string()).await.unwrap();

        let requests = backend.requests.lock().await;
        assert_eq!(requests.len(), 2);
        assert!(!requests[0].tools);
        assert!(requests[1].tools);
        assert!(
            session
                .snapshot()
                .await
                .iter()
                .any(|event| matches!(event.kind, EventKind::Compaction { .. }))
        );

        let _ = tokio::fs::remove_dir_all(output_directory).await;
    }
}
