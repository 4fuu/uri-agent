use crate::catalog::ModelLimits;
use crate::compaction;
use crate::model::{ModelBackend, ModelDelta, ModelRequest, ModelResponse};
use crate::protocol::ProtocolRegistry;
use crate::session::{EventKind, Session};
use crate::task::TaskManager;
use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use rig::completion::Usage;
use rig::message::{
    AssistantContent, ImageMediaType, Message, Text, ToolCall, ToolResultContent, UserContent,
};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::{Mutex, RwLock, mpsc};

const MAX_TOOL_ROUNDS: usize = 32;

pub struct AgentRuntime {
    backend: RwLock<Option<Arc<dyn ModelBackend>>>,
    protocols: Arc<ProtocolRegistry>,
    session: Session,
    system_prompt: String,
    limits: RwLock<ModelLimits>,
    estimated_tokens: AtomicUsize,
    turn: Mutex<()>,
}

impl AgentRuntime {
    pub fn new(
        backend: Option<Arc<dyn ModelBackend>>,
        protocols: Arc<ProtocolRegistry>,
        session: Session,
        system_prompt: String,
        limits: ModelLimits,
    ) -> Self {
        Self {
            backend: RwLock::new(backend),
            protocols,
            session,
            system_prompt,
            limits: RwLock::new(limits),
            estimated_tokens: AtomicUsize::new(0),
            turn: Mutex::new(()),
        }
    }

    pub fn session(&self) -> &Session {
        &self.session
    }

    /// Estimated tokens the next model request would carry, used by the
    /// footer's context meter the same way pi estimates its context usage.
    pub fn estimated_context(&self) -> usize {
        self.estimated_tokens.load(Ordering::Relaxed)
    }

    pub async fn refresh_context_estimate(&self) {
        let history = self.session.model_history().await;
        self.estimated_tokens.store(
            compaction::estimate_tokens(&self.system_prompt, &history),
            Ordering::Relaxed,
        );
    }

    pub async fn set_backend(
        &self,
        backend: Option<Arc<dyn ModelBackend>>,
        limits: Option<ModelLimits>,
    ) {
        *self.backend.write().await = backend;
        if let Some(limits) = limits {
            *self.limits.write().await = limits;
        }
    }

    pub async fn has_backend(&self) -> bool {
        self.backend.read().await.is_some()
    }

    pub async fn compact(&self) -> Result<()> {
        let _turn = self.turn.lock().await;
        let backend = self
            .backend
            .read()
            .await
            .clone()
            .ok_or_else(|| anyhow!("no credential configured; press :login"))?;
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
                let text = "no credential configured; press :login";
                self.session
                    .append(EventKind::Error {
                        text: text.to_string(),
                    })
                    .await?;
                return Err(anyhow!(text));
            }
        };
        let images = match image_attachments(prompt, self.session.directory()).await {
            Ok(images) => images,
            Err(error) => {
                let text = format!("{error:#}");
                self.session
                    .append(EventKind::Error { text: text.clone() })
                    .await?;
                return Err(anyhow!(text));
            }
        };
        if !images.is_empty() && !backend.accepts_image_input() {
            let text = "the active model does not accept image input".to_string();
            self.session
                .append(EventKind::Error { text: text.clone() })
                .await?;
            return Err(anyhow!(text));
        }
        self.session
            .append(EventKind::User {
                text: prompt.to_string(),
            })
            .await?;
        let mut content = vec![UserContent::text(prompt)];
        content.extend(images);
        self.append_model_message(Message::User { content }).await?;

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
            self.record_usage(response.usage).await?;
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
        let context_window = self.limits.read().await.context_window.max(1);
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
        self.record_usage(response.usage).await?;
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
        self.refresh_context_estimate().await;
        Ok(true)
    }

    /// Persist one response's token usage, priced with the active model's
    /// catalog rates. A zero-valued report is the sentinel for missing
    /// metrics and carries no information worth an event.
    async fn record_usage(&self, usage: Option<Usage>) -> Result<()> {
        let Some(usage) = usage else { return Ok(()) };
        if usage.input_tokens == 0 && usage.output_tokens == 0 {
            return Ok(());
        }
        let cost = self.limits.read().await.cost.total(
            usage.input_tokens,
            usage.output_tokens,
            usage.cached_input_tokens,
            usage.cache_creation_input_tokens,
        );
        self.session
            .append(EventKind::Usage {
                input: usage.input_tokens,
                output: usage.output_tokens,
                cache_read: usage.cached_input_tokens,
                cache_write: usage.cache_creation_input_tokens,
                cost,
            })
            .await?;
        Ok(())
    }

    async fn complete_once(&self, backend: &dyn ModelBackend) -> Result<ModelResponse> {
        let history = self.session.model_history().await;
        self.estimated_tokens.store(
            compaction::estimate_tokens(&self.system_prompt, &history),
            Ordering::Relaxed,
        );
        let request = ModelRequest {
            system: self.system_prompt.clone(),
            history,
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

async fn image_attachments(prompt: &str, cwd: &Path) -> Result<Vec<UserContent>> {
    let arguments = shell_words::split(prompt).unwrap_or_else(|_| {
        prompt
            .split_whitespace()
            .map(str::to_string)
            .collect::<Vec<_>>()
    });
    let mut images = Vec::new();
    for argument in arguments {
        let Some(path) = argument.strip_prefix('@').filter(|path| !path.is_empty()) else {
            continue;
        };
        let path = PathBuf::from(path);
        let path = if path.is_absolute() {
            path
        } else {
            cwd.join(path)
        };
        let extension = path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if !matches!(extension.as_str(), "jpg" | "jpeg" | "png" | "gif" | "webp") {
            continue;
        }
        let canonical = tokio::fs::canonicalize(&path)
            .await
            .with_context(|| format!("cannot attach image {}", path.display()))?;
        if !canonical.starts_with(cwd) {
            bail!(
                "image attachment is outside the project boundary: {}",
                canonical.display()
            );
        }
        let bytes = tokio::fs::read(&canonical)
            .await
            .with_context(|| format!("cannot read image {}", canonical.display()))?;
        let media_type = detect_image_type(&bytes).with_context(|| {
            format!("unsupported or invalid image file: {}", canonical.display())
        })?;
        images.push(UserContent::image_base64(
            base64::engine::general_purpose::STANDARD.encode(bytes),
            Some(media_type),
            None,
        ));
    }
    Ok(images)
}

fn detect_image_type(bytes: &[u8]) -> Option<ImageMediaType> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(ImageMediaType::PNG)
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Some(ImageMediaType::JPEG)
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some(ImageMediaType::GIF)
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        Some(ImageMediaType::WEBP)
    } else {
        None
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

    #[derive(Default)]
    struct FakeBackend {
        responses: Mutex<VecDeque<(Vec<AssistantContent>, Option<Usage>)>>,
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
            let (content, usage) = self.responses.lock().await.pop_front().unwrap();
            for part in &content {
                if let AssistantContent::Text(text) = part {
                    let _ = deltas.send(ModelDelta::Text(text.text.clone()));
                }
            }
            Ok(ModelResponse { content, usage })
        }
    }

    fn fake_usage() -> Usage {
        Usage {
            input_tokens: 1_000,
            output_tokens: 500,
            cached_input_tokens: 100,
            cache_creation_input_tokens: 50,
            ..Usage::new()
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
    async fn at_image_paths_become_binary_model_attachments() {
        let workspace = tempfile::tempdir().unwrap();
        let path = workspace.path().join("screen.png");
        tokio::fs::write(&path, b"\x89PNG\r\n\x1a\nimage-data")
            .await
            .unwrap();
        let attachments = image_attachments("inspect @screen.png", workspace.path())
            .await
            .unwrap();
        assert_eq!(attachments.len(), 1);
        assert!(matches!(
            &attachments[0],
            UserContent::Image(image)
                if image.media_type == Some(ImageMediaType::PNG)
                    && matches!(&image.data, rig::message::DocumentSourceKind::Base64(data)
                        if base64::engine::general_purpose::STANDARD.decode(data).unwrap().starts_with(b"\x89PNG"))
        ));
    }

    #[tokio::test]
    async fn image_attachments_cannot_escape_the_project_boundary() {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::with_suffix(".png").unwrap();
        tokio::fs::write(outside.path(), b"\x89PNG\r\n\x1a\nimage-data")
            .await
            .unwrap();
        let error = image_attachments(
            &format!("inspect @{}", outside.path().display()),
            workspace.path(),
        )
        .await
        .unwrap_err();
        assert!(format!("{error:#}").contains("outside the project boundary"));
    }

    #[tokio::test]
    async fn text_only_backends_reject_images_before_recording_the_turn() {
        let workspace = tempfile::tempdir().unwrap();
        tokio::fs::write(
            workspace.path().join("screen.png"),
            b"\x89PNG\r\n\x1a\nimage-data",
        )
        .await
        .unwrap();
        let session_id = format!("test{}", uuid::Uuid::now_v7().simple());
        let session = crate::session::Session::open_at(
            workspace.path().join("sessions.db"),
            Some(&session_id),
            workspace.path(),
            "fake",
            "text-only",
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
        let output_directory = output.directory().to_path_buf();
        let backend = Arc::new(FakeBackend::default());
        let runtime = AgentRuntime::new(
            Some(backend.clone()),
            Arc::new(ProtocolRegistry::new(output, TaskManager::new())),
            session.clone(),
            "system".to_string(),
            ModelLimits::default(),
        );

        let error = runtime
            .run_turn("inspect @screen.png".to_string())
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("does not accept image input"));
        assert!(backend.requests.lock().await.is_empty());
        assert!(session.model_history().await.is_empty());

        drop(runtime);
        drop(session);
        let _ = tokio::fs::remove_dir_all(output_directory).await;
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
            ModelLimits::default(),
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
            matches!(events.last().map(|event| &event.kind), Some(EventKind::Error { text }) if text.contains(":login"))
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
        let mut commands = crate::plugin::CommandRegistry::with_core_commands();
        let mut tui = crate::plugin::TuiRegistry::default();
        crate::builtins::plugins(workspace.path())
            .install(&mut crate::plugin::PluginHost {
                protocols: &mut protocols,
                commands: &mut commands,
                tui: &mut tui,
            })
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
                (vec![AssistantContent::ToolCall(call)], None),
                (vec![AssistantContent::text("Done")], Some(fake_usage())),
            ])),
            requests: Mutex::new(Vec::new()),
        });
        let runtime = AgentRuntime::new(
            Some(backend),
            Arc::new(protocols),
            session.clone(),
            "test system prompt".to_string(),
            ModelLimits {
                context_window: 128_000,
                max_tokens: 8_192,
                cost: crate::catalog::ModelCost {
                    input: 3.0,
                    output: 15.0,
                    cache_read: 0.3,
                    cache_write: 3.75,
                    tiers: Vec::new(),
                },
            },
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
        let usage = events
            .iter()
            .find_map(|event| match &event.kind {
                EventKind::Usage {
                    input,
                    output,
                    cache_read,
                    cache_write,
                    cost,
                } => Some((*input, *output, *cache_read, *cache_write, *cost)),
                _ => None,
            })
            .expect("a reported usage becomes a session event");
        assert_eq!(usage.0, 1_000);
        assert_eq!(usage.1, 500);
        assert_eq!(usage.2, 100);
        assert_eq!(usage.3, 50);
        let expected = (1_000.0 * 3.0 + 500.0 * 15.0 + 100.0 * 0.3 + 50.0 * 3.75) / 1_000_000.0;
        assert!((usage.4 - expected).abs() < f64::EPSILON);
        assert!(runtime.estimated_context() > 0);

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
            responses: Mutex::new(VecDeque::from([(
                vec![AssistantContent::text(
                    "The first task is complete; continue the current task.",
                )],
                None,
            )])),
            requests: Mutex::new(Vec::new()),
        });
        let runtime = AgentRuntime::new(
            Some(backend.clone()),
            Arc::new(ProtocolRegistry::new(output, TaskManager::new())),
            session.clone(),
            "frozen system".to_string(),
            ModelLimits::default(),
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
                (vec![AssistantContent::text("Old work is complete.")], None),
                (vec![AssistantContent::text("Current answer")], None),
            ])),
            requests: Mutex::new(Vec::new()),
        });
        let runtime = AgentRuntime::new(
            Some(backend.clone()),
            Arc::new(ProtocolRegistry::new(output, TaskManager::new())),
            session.clone(),
            "frozen system".to_string(),
            ModelLimits {
                context_window: 64,
                ..ModelLimits::default()
            },
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
