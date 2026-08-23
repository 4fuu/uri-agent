use crate::catalog::ModelLimits;
use crate::compaction;
use crate::config::{display_path, path_is_within};
use crate::model::{
    ModelBackend, ModelDelta, ModelFailure, ModelFailureKind, ModelFailurePhase, ModelRequest,
    ModelResponse, looks_like_context_overflow,
};
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
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{Mutex, RwLock, mpsc, watch};
use tokio::task::JoinHandle;

const MAX_TOOL_ROUNDS: usize = 32;
const MAX_RETRY_AFTER: Duration = Duration::from_secs(60);
const TURN_INTERRUPTED_BY_USER: &str = "turn interrupted by user";
const TURN_INTERRUPTED_BY_SHUTDOWN: &str = "turn interrupted by shutdown";

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ImageAttachment {
    bytes: Vec<u8>,
}

impl ImageAttachment {
    pub(crate) fn png(bytes: Vec<u8>) -> Self {
        Self { bytes }
    }
}

#[derive(Clone, Copy)]
struct ModelRetryPolicy {
    max_retries: usize,
    base_delay: Duration,
    max_delay: Duration,
    reason: &'static str,
}

#[derive(Clone, Copy)]
enum TurnCancellation {
    User,
    Shutdown,
}

impl TurnCancellation {
    fn message(self) -> &'static str {
        match self {
            Self::User => TURN_INTERRUPTED_BY_USER,
            Self::Shutdown => TURN_INTERRUPTED_BY_SHUTDOWN,
        }
    }
}

struct ActiveTurn {
    cancel: watch::Sender<Option<TurnCancellation>>,
    handle: JoinHandle<()>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PendingMessageKind {
    Queued,
    Guidance,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingMessage {
    pub id: u64,
    pub text: String,
    pub kind: PendingMessageKind,
}

#[derive(Clone)]
struct PendingMessageEntry {
    message: PendingMessage,
    content: Vec<UserContent>,
    clipboard_images: Vec<ImageAttachment>,
}

#[derive(Default)]
struct PendingState {
    accepting: bool,
    next_id: u64,
    messages: VecDeque<PendingMessageEntry>,
}

pub struct AgentRuntime {
    backend: RwLock<Option<Arc<dyn ModelBackend>>>,
    protocols: Arc<ProtocolRegistry>,
    session: Session,
    system_prompt: String,
    limits: RwLock<ModelLimits>,
    estimated_tokens: AtomicUsize,
    turn: Mutex<()>,
    active_turn: Mutex<Option<ActiveTurn>>,
    pending: Mutex<PendingState>,
    pending_updates: watch::Sender<Vec<PendingMessage>>,
}

impl AgentRuntime {
    pub fn new(
        backend: Option<Arc<dyn ModelBackend>>,
        protocols: Arc<ProtocolRegistry>,
        session: Session,
        system_prompt: String,
        limits: ModelLimits,
    ) -> Self {
        let (pending_updates, _) = watch::channel(Vec::new());
        Self {
            backend: RwLock::new(backend),
            protocols,
            session,
            system_prompt,
            limits: RwLock::new(limits),
            estimated_tokens: AtomicUsize::new(0),
            turn: Mutex::new(()),
            active_turn: Mutex::new(None),
            pending: Mutex::new(PendingState::default()),
            pending_updates,
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

    pub async fn turn_running(&self) -> bool {
        self.active_turn
            .lock()
            .await
            .as_ref()
            .is_some_and(|turn| !turn.handle.is_finished())
    }

    pub fn subscribe_pending_messages(&self) -> watch::Receiver<Vec<PendingMessage>> {
        self.pending_updates.subscribe()
    }

    pub async fn pending_messages(&self) -> Vec<PendingMessage> {
        self.pending
            .lock()
            .await
            .messages
            .iter()
            .map(|entry| entry.message.clone())
            .collect()
    }

    pub async fn enqueue_message(
        &self,
        prompt: String,
        kind: PendingMessageKind,
    ) -> Result<PendingMessage> {
        self.enqueue_message_with_images(prompt, Vec::new(), kind)
            .await
    }

    pub(crate) async fn enqueue_message_with_images(
        &self,
        prompt: String,
        clipboard_images: Vec<ImageAttachment>,
        kind: PendingMessageKind,
    ) -> Result<PendingMessage> {
        let prompt = prompt.trim();
        if prompt.is_empty() {
            bail!("message is empty")
        }
        let backend = self
            .backend
            .read()
            .await
            .clone()
            .ok_or_else(|| anyhow!("no credential configured; press :login"))?;
        let content = self
            .user_content(prompt, &clipboard_images, backend.as_ref())
            .await?;
        let mut pending = self.pending.lock().await;
        if !pending.accepting {
            bail!("the active turn has already finished")
        }
        let message = PendingMessage {
            id: pending.next_id,
            text: prompt.to_string(),
            kind,
        };
        pending.next_id = pending.next_id.saturating_add(1);
        pending.messages.push_back(PendingMessageEntry {
            message: message.clone(),
            content,
            clipboard_images,
        });
        self.publish_pending(&pending);
        Ok(message)
    }

    pub(crate) async fn cancel_latest_pending(
        &self,
    ) -> Option<(PendingMessage, Vec<ImageAttachment>)> {
        let mut pending = self.pending.lock().await;
        let entry = pending.messages.pop_back()?;
        self.publish_pending(&pending);
        Some((entry.message, entry.clipboard_images))
    }

    pub async fn upgrade_latest_queued(&self) -> Option<PendingMessage> {
        let mut pending = self.pending.lock().await;
        let entry = pending
            .messages
            .iter_mut()
            .rev()
            .find(|entry| entry.message.kind == PendingMessageKind::Queued)?;
        entry.message.kind = PendingMessageKind::Guidance;
        let message = entry.message.clone();
        self.publish_pending(&pending);
        Some(message)
    }

    fn publish_pending(&self, pending: &PendingState) {
        self.pending_updates.send_replace(
            pending
                .messages
                .iter()
                .map(|entry| entry.message.clone())
                .collect(),
        );
    }

    pub async fn compact(&self) -> Result<()> {
        let _turn = self.turn.lock().await;
        let history = self.session.model_history().await;
        let context_window = self.limits.read().await.context_window.max(1);
        let context_tokens = compaction::estimate_tokens(&self.system_prompt, &history);
        if !compaction::manual_compaction_allowed(context_tokens, context_window) {
            bail!("manual compaction requires context usage above 20%")
        }
        let backend = self
            .backend
            .read()
            .await
            .clone()
            .ok_or_else(|| anyhow!("no credential configured; press :login"))?;
        let (_cancel_tx, mut cancel) = watch::channel(None);
        if !self
            .compact_with(backend.as_ref(), true, true, &mut cancel)
            .await?
        {
            bail!("not enough completed history to compact")
        }
        Ok(())
    }

    /// Start a detached turn owned by this runtime. The handle remains
    /// available for orderly process shutdown instead of becoming fire-and-forget.
    pub async fn start_turn(self: &Arc<Self>, prompt: String) -> Result<()> {
        self.start_turn_with_images(prompt, Vec::new()).await
    }

    pub(crate) async fn start_turn_with_images(
        self: &Arc<Self>,
        prompt: String,
        images: Vec<ImageAttachment>,
    ) -> Result<()> {
        let mut active = self.active_turn.lock().await;
        if let Some(previous) = active.take() {
            if !previous.handle.is_finished() {
                *active = Some(previous);
                bail!("a turn is already running")
            }
            let _ = previous.handle.await;
        }
        {
            let mut pending = self.pending.lock().await;
            if !pending.messages.is_empty() {
                bail!("restore pending messages before starting a new turn")
            }
            pending.accepting = true;
        }
        let (cancel, receiver) = watch::channel(None);
        let runtime = self.clone();
        let handle = tokio::spawn(async move {
            runtime.run_active_turn(prompt, images, receiver).await;
        });
        *active = Some(ActiveTurn { cancel, handle });
        Ok(())
    }

    async fn run_active_turn(
        self: Arc<Self>,
        prompt: String,
        images: Vec<ImageAttachment>,
        mut cancel: watch::Receiver<Option<TurnCancellation>>,
    ) {
        let mut input = PendingMessageEntry {
            message: PendingMessage {
                id: u64::MAX,
                text: prompt,
                kind: PendingMessageKind::Queued,
            },
            content: Vec::new(),
            clipboard_images: images,
        };
        let mut prepared = false;
        let mut pending_index = None;
        loop {
            let mut input_delivered = false;
            let result = self
                .run_turn_with_cancel(
                    input.message.text.clone(),
                    prepared.then_some(input.content.clone()),
                    if prepared {
                        &[]
                    } else {
                        &input.clipboard_images
                    },
                    !prepared,
                    &mut input_delivered,
                    &mut cancel,
                )
                .await;
            if result.is_err() {
                if prepared && !input_delivered {
                    self.restore_pending_entry(pending_index.unwrap_or_default(), input)
                        .await;
                }
                self.stop_accepting_pending().await;
                return;
            }
            let Some((index, next)) = self.take_next_pending_or_stop().await else {
                return;
            };
            input = next;
            pending_index = Some(index);
            prepared = true;
        }
    }

    /// Request cancellation of the active turn without blocking the caller
    /// while its interrupted terminal boundary is persisted.
    pub async fn interrupt_turn(&self) -> bool {
        let active = self.active_turn.lock().await;
        let Some(active) = active
            .as_ref()
            .filter(|active| !active.handle.is_finished())
        else {
            return false;
        };
        active.cancel.send(Some(TurnCancellation::User)).is_ok()
    }

    /// Cancel the active request/tool operation, then wait until the worker has
    /// durably recorded its interrupted terminal boundary.
    pub async fn shutdown(&self) {
        if let Some(active) = self.active_turn.lock().await.take() {
            let _ = active.cancel.send(Some(TurnCancellation::Shutdown));
            let _ = active.handle.await;
        }
        self.restore_pending_to_draft().await;
    }

    pub async fn run_turn(&self, prompt: String) -> Result<()> {
        self.run_turn_with_images(prompt, Vec::new()).await
    }

    pub(crate) async fn run_turn_with_images(
        &self,
        prompt: String,
        images: Vec<ImageAttachment>,
    ) -> Result<()> {
        let (_cancel_tx, mut cancel) = watch::channel(None);
        let mut input_delivered = false;
        self.run_turn_with_cancel(
            prompt,
            None,
            &images,
            false,
            &mut input_delivered,
            &mut cancel,
        )
        .await
    }

    async fn run_turn_with_cancel(
        &self,
        prompt: String,
        prepared_content: Option<Vec<UserContent>>,
        clipboard_images: &[ImageAttachment],
        take_initial_guidance: bool,
        input_delivered: &mut bool,
        cancel: &mut watch::Receiver<Option<TurnCancellation>>,
    ) -> Result<()> {
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
        let content = match prepared_content {
            Some(content) => {
                if content
                    .iter()
                    .any(|item| matches!(item, UserContent::Image(_)))
                    && !backend.accepts_image_input()
                {
                    let text = "the active model does not accept image input".to_string();
                    self.session
                        .append(EventKind::Error { text: text.clone() })
                        .await?;
                    return Err(anyhow!(text));
                }
                content
            }
            None => match self
                .user_content(prompt, clipboard_images, backend.as_ref())
                .await
            {
                Ok(content) => content,
                Err(error) => {
                    let text = format!("{error:#}");
                    self.session
                        .append(EventKind::Error { text: text.clone() })
                        .await?;
                    return Err(anyhow!(text));
                }
            },
        };
        self.append_user_input(prompt.to_string(), content, false)
            .await?;
        *input_delivered = true;

        let result = self
            .run_tool_loop(backend, take_initial_guidance, cancel)
            .await;
        match result {
            Ok(()) => {
                self.session.append(EventKind::TurnFinished).await?;
                Ok(())
            }
            Err(error) => {
                let text = format!("{error:#}");
                self.session
                    .append_batch(vec![
                        EventKind::Error { text: text.clone() },
                        EventKind::TurnFinished,
                    ])
                    .await?;
                Err(anyhow!(text))
            }
        }
    }

    async fn user_content(
        &self,
        prompt: &str,
        clipboard_images: &[ImageAttachment],
        backend: &dyn ModelBackend,
    ) -> Result<Vec<UserContent>> {
        let mut images = memory_image_attachments(clipboard_images);
        images.extend(image_attachments(prompt, self.session.project_directory()).await?);
        if !images.is_empty() && !backend.accepts_image_input() {
            bail!("the active model does not accept image input")
        }
        let mut content = vec![UserContent::text(prompt)];
        content.extend(images);
        Ok(content)
    }

    async fn append_user_input(
        &self,
        text: String,
        content: Vec<UserContent>,
        finish_previous: bool,
    ) -> Result<()> {
        let mut events = Vec::with_capacity(3);
        if finish_previous {
            events.push(EventKind::TurnFinished);
        }
        events.extend([
            EventKind::User { text },
            EventKind::ModelMessage {
                message: Message::User { content },
            },
        ]);
        self.session
            .append_batch(events)
            .await
            .context("cannot persist user turn boundary")?;
        Ok(())
    }

    async fn take_guidance(&self) -> Option<(usize, PendingMessageEntry)> {
        let mut pending = self.pending.lock().await;
        let index = pending
            .messages
            .iter()
            .position(|entry| entry.message.kind == PendingMessageKind::Guidance)?;
        let entry = pending.messages.remove(index)?;
        self.publish_pending(&pending);
        Some((index, entry))
    }

    async fn take_next_pending_or_stop(&self) -> Option<(usize, PendingMessageEntry)> {
        let mut pending = self.pending.lock().await;
        let index = pending
            .messages
            .iter()
            .position(|entry| entry.message.kind == PendingMessageKind::Guidance)
            .or_else(|| (!pending.messages.is_empty()).then_some(0));
        let Some(index) = index else {
            pending.accepting = false;
            return None;
        };
        let entry = pending.messages.remove(index)?;
        self.publish_pending(&pending);
        Some((index, entry))
    }

    async fn append_pending_input(
        &self,
        index: usize,
        entry: PendingMessageEntry,
        finish_previous: bool,
    ) -> Result<()> {
        if let Err(error) = self
            .append_user_input(
                entry.message.text.clone(),
                entry.content.clone(),
                finish_previous,
            )
            .await
        {
            self.restore_pending_entry(index, entry).await;
            return Err(error);
        }
        Ok(())
    }

    async fn restore_pending_entry(&self, index: usize, entry: PendingMessageEntry) {
        let mut pending = self.pending.lock().await;
        pending.accepting = false;
        let index = index.min(pending.messages.len());
        pending.messages.insert(index, entry);
        self.publish_pending(&pending);
    }

    async fn stop_accepting_pending(&self) {
        self.pending.lock().await.accepting = false;
    }

    async fn restore_pending_to_draft(&self) {
        let messages = {
            let mut pending = self.pending.lock().await;
            pending.accepting = false;
            let messages = pending
                .messages
                .drain(..)
                .map(|entry| entry.message.text)
                .collect::<Vec<_>>();
            self.publish_pending(&pending);
            messages
        };
        if messages.is_empty() {
            return;
        }
        let draft = self.session.draft().await;
        let restored = messages
            .into_iter()
            .chain((!draft.trim().is_empty()).then_some(draft))
            .collect::<Vec<_>>()
            .join("\n\n");
        let _ = self.session.save_draft(&restored).await;
    }

    async fn run_tool_loop(
        &self,
        backend: Arc<dyn ModelBackend>,
        take_initial_guidance: bool,
        cancel: &mut watch::Receiver<Option<TurnCancellation>>,
    ) -> Result<()> {
        let mut overflow_retried = false;
        let mut has_model_response = false;
        let mut guidance_ready = false;
        if take_initial_guidance && let Some((index, guidance)) = self.take_guidance().await {
            self.append_pending_input(index, guidance, false).await?;
            guidance_ready = true;
        }
        for _ in 0..MAX_TOOL_ROUNDS {
            self.compact_with(backend.as_ref(), false, false, cancel)
                .await?;
            if !guidance_ready && let Some((index, guidance)) = self.take_guidance().await {
                self.append_pending_input(index, guidance, has_model_response)
                    .await?;
            }
            let mut model_retries = HashMap::new();
            let response = loop {
                match self.complete_once(backend.as_ref(), cancel).await {
                    Ok(response) => break response,
                    Err(error) if !overflow_retried && is_context_overflow(&error) => {
                        overflow_retried = true;
                        if !self
                            .compact_with(backend.as_ref(), true, false, cancel)
                            .await?
                        {
                            return Err(error);
                        }
                    }
                    Err(error) => {
                        if self
                            .retry_model_failure(&error, &mut model_retries, cancel)
                            .await?
                        {
                            continue;
                        }
                        return Err(error);
                    }
                }
            };
            guidance_ready = false;
            let assistant_message = Message::Assistant {
                id: None,
                content: response.content.clone(),
            };
            let mut events = self.assistant_events(&response.content);
            if let Some(usage) = self.usage_event(response.usage).await {
                events.insert(0, usage);
            }
            events.push(EventKind::ModelMessage {
                message: assistant_message,
            });
            self.session
                .append_batch(events)
                .await
                .context("cannot persist assistant turn boundary")?;
            has_model_response = true;

            let tool_calls = response
                .content
                .iter()
                .filter_map(|content| match content {
                    AssistantContent::ToolCall(call) => Some(call.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            for call in tool_calls {
                self.execute_tool(call, cancel).await?;
            }
            if let Some((index, guidance)) = self.take_guidance().await {
                self.append_pending_input(index, guidance, true).await?;
                guidance_ready = true;
                continue;
            }
            if response
                .content
                .iter()
                .all(|content| !matches!(content, AssistantContent::ToolCall(_)))
            {
                return Ok(());
            }
        }
        bail!("model exceeded {MAX_TOOL_ROUNDS} consecutive tool rounds")
    }

    async fn compact_with(
        &self,
        backend: &dyn ModelBackend,
        force: bool,
        manual: bool,
        cancel: &mut watch::Receiver<Option<TurnCancellation>>,
    ) -> Result<bool> {
        let history = self.session.model_history().await;
        let context_window = self.limits.read().await.context_window.max(1);
        if !force && !compaction::should_compact(&self.system_prompt, &history, context_window) {
            return Ok(false);
        }
        let preparation = if force && !manual {
            compaction::prepare(&self.system_prompt, &history, context_window, force)
        } else {
            compaction::prepare_preserving_latest_turn(
                &self.system_prompt,
                &history,
                context_window,
                force,
            )
        };
        let Some(preparation) = preparation else {
            return Ok(false);
        };
        let request = ModelRequest {
            system: compaction::SUMMARY_SYSTEM_PROMPT.to_string(),
            history: compaction::summary_history(&preparation),
            tools: false,
        };
        let mut model_retries = HashMap::new();
        let (response, summary) = loop {
            let (deltas, _receiver) = mpsc::unbounded_channel();
            let completion = backend.complete(request.clone(), deltas);
            tokio::pin!(completion);
            let result = tokio::select! {
                response = &mut completion => response,
                changed = cancel.changed() => {
                    if changed.is_ok()
                        && let Some(cancellation) = *cancel.borrow()
                    {
                        bail!(cancellation.message())
                    }
                    completion.await
                }
            };
            match result {
                Ok(response) => {
                    let summary = response
                        .content
                        .iter()
                        .filter_map(|content| match content {
                            AssistantContent::Text(text) => Some(text.text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    if !summary.trim().is_empty() {
                        break (response, summary);
                    }
                    let error = anyhow::Error::new(ModelFailure::empty_response());
                    if self
                        .retry_model_failure(&error, &mut model_retries, cancel)
                        .await?
                    {
                        continue;
                    }
                    return Err(error).context("context compaction model request failed");
                }
                Err(error) => {
                    if self
                        .retry_model_failure(&error, &mut model_retries, cancel)
                        .await?
                    {
                        continue;
                    }
                    return Err(error).context("context compaction model request failed");
                }
            }
        };
        self.record_usage(response.usage).await?;
        let replacement = compaction::replacement_history(&summary, &preparation.retained);
        self.session
            .append_compaction(summary, preparation.tokens_before, replacement, manual)
            .await?;
        self.refresh_context_estimate().await;
        Ok(true)
    }

    /// Persist one response's token usage, priced with the active model's
    /// catalog rates. A zero-valued report is the sentinel for missing
    /// metrics and carries no information worth an event.
    async fn record_usage(&self, usage: Option<Usage>) -> Result<()> {
        let Some(event) = self.usage_event(usage).await else {
            return Ok(());
        };
        self.session.append(event).await?;
        Ok(())
    }

    async fn usage_event(&self, usage: Option<Usage>) -> Option<EventKind> {
        let usage = usage?;
        if usage.input_tokens == 0 && usage.output_tokens == 0 {
            return None;
        }
        let cost = self.limits.read().await.cost.total(
            usage.input_tokens,
            usage.output_tokens,
            usage.cached_input_tokens,
            usage.cache_creation_input_tokens,
        );
        Some(EventKind::Usage {
            input: usage.input_tokens,
            output: usage.output_tokens,
            cache_read: usage.cached_input_tokens,
            cache_write: usage.cache_creation_input_tokens,
            cost,
        })
    }

    async fn complete_once(
        &self,
        backend: &dyn ModelBackend,
        cancel: &mut watch::Receiver<Option<TurnCancellation>>,
    ) -> Result<ModelResponse> {
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
                        self.publish_delta(delta);
                    }
                    return response;
                }
                delta = receiver.recv() => {
                    if let Some(delta) = delta {
                        self.publish_delta(delta);
                    }
                }
                changed = cancel.changed() => {
                    if changed.is_ok()
                        && let Some(cancellation) = *cancel.borrow()
                    {
                        bail!(cancellation.message())
                    }
                }
            }
        }
    }

    async fn retry_model_failure(
        &self,
        error: &anyhow::Error,
        retries: &mut HashMap<ModelFailureKind, usize>,
        cancel: &mut watch::Receiver<Option<TurnCancellation>>,
    ) -> Result<bool> {
        let Some(failure) = error.downcast_ref::<ModelFailure>() else {
            return Ok(false);
        };
        let Some(policy) = model_retry_policy(failure.kind()) else {
            return Ok(false);
        };
        let attempt = {
            let retries = retries.entry(failure.kind()).or_default();
            if *retries >= policy.max_retries {
                return Ok(false);
            }
            *retries += 1;
            *retries
        };
        let delay = model_retry_delay(failure, policy, attempt);
        self.session
            .append(EventKind::ModelRetry {
                attempt,
                max_retries: policy.max_retries,
                delay_ms: delay.as_millis().try_into().unwrap_or(u64::MAX),
                reason: model_retry_reason(failure, policy),
            })
            .await?;
        let sleep = tokio::time::sleep(delay);
        tokio::pin!(sleep);
        tokio::select! {
            () = &mut sleep => {}
            changed = cancel.changed() => {
                if changed.is_ok()
                    && let Some(cancellation) = *cancel.borrow()
                {
                    bail!(cancellation.message())
                }
                sleep.await;
            }
        }
        Ok(true)
    }

    fn publish_delta(&self, delta: ModelDelta) {
        self.session.publish_transient(match delta {
            ModelDelta::Text(text) => EventKind::AssistantText { text },
            ModelDelta::Reasoning(text) => EventKind::AssistantReasoning { text },
        });
    }

    async fn execute_tool(
        &self,
        call: ToolCall,
        cancel: &mut watch::Receiver<Option<TurnCancellation>>,
    ) -> Result<()> {
        let name = call.function.name.clone();
        let call_id = call.id.to_string();
        let dispatch = self.dispatch(&name, &call.function.arguments);
        tokio::pin!(dispatch);
        let result = tokio::select! {
            result = &mut dispatch => result,
            changed = cancel.changed() => {
                if changed.is_ok()
                    && let Some(cancellation) = *cancel.borrow()
                {
                    bail!(cancellation.message())
                }
                dispatch.await
            }
        };
        let (output, failed) = match result {
            Ok(output) => (output, false),
            Err(error) => (format!("Error: {error:#}"), true),
        };
        let result = UserContent::tool_result_for(
            call.id,
            call.provider,
            name.clone(),
            vec![ToolResultContent::Text(Text::new(output.clone()))],
        );
        self.session
            .append_batch(vec![
                EventKind::ToolResult {
                    call_id,
                    name: name.clone(),
                    output,
                    failed,
                },
                EventKind::ModelMessage {
                    message: Message::User {
                        content: vec![result],
                    },
                },
            ])
            .await
            .context("cannot persist tool result boundary")?;
        Ok(())
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

    fn assistant_events(&self, content: &[AssistantContent]) -> Vec<EventKind> {
        content
            .iter()
            .filter_map(|content| match content {
                AssistantContent::Text(text) => Some(EventKind::AssistantText {
                    text: text.text.clone(),
                }),
                AssistantContent::Reasoning(reasoning) => {
                    let text = reasoning.display_text();
                    (!text.is_empty()).then_some(EventKind::AssistantReasoning { text })
                }
                AssistantContent::ToolCall(call) => Some(EventKind::ToolCall {
                    call_id: call.id.to_string(),
                    name: call.function.name.clone(),
                    arguments: call.function.arguments.clone(),
                }),
                AssistantContent::Image(_) => None,
            })
            .collect()
    }
}

fn is_context_overflow(error: &anyhow::Error) -> bool {
    if let Some(failure) = error.downcast_ref::<ModelFailure>() {
        return failure.kind() == ModelFailureKind::ContextOverflow;
    }
    looks_like_context_overflow(&format!("{error:#}"))
}

fn model_retry_policy(kind: ModelFailureKind) -> Option<ModelRetryPolicy> {
    match kind {
        ModelFailureKind::RateLimit => Some(ModelRetryPolicy {
            max_retries: 6,
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(30),
            reason: "rate limit",
        }),
        ModelFailureKind::Network => Some(ModelRetryPolicy {
            max_retries: 5,
            base_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(8),
            reason: "network error",
        }),
        ModelFailureKind::Server => Some(ModelRetryPolicy {
            max_retries: 5,
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(15),
            reason: "server error",
        }),
        ModelFailureKind::Timeout => Some(ModelRetryPolicy {
            max_retries: 4,
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(10),
            reason: "timeout",
        }),
        ModelFailureKind::Conflict => Some(ModelRetryPolicy {
            max_retries: 4,
            base_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(8),
            reason: "request conflict",
        }),
        ModelFailureKind::EmptyResponse => Some(ModelRetryPolicy {
            max_retries: 4,
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(8),
            reason: "empty response",
        }),
        ModelFailureKind::ContextOverflow
        | ModelFailureKind::Authentication
        | ModelFailureKind::Quota
        | ModelFailureKind::Client
        | ModelFailureKind::Other => None,
    }
}

fn model_retry_delay(failure: &ModelFailure, policy: ModelRetryPolicy, retry: usize) -> Duration {
    if let Some(requested) = failure.retry_after() {
        return requested.min(MAX_RETRY_AFTER);
    }
    let multiplier = 1_u32 << retry.saturating_sub(1).min(31);
    let backoff = policy
        .base_delay
        .saturating_mul(multiplier)
        .min(policy.max_delay);
    let jitter_limit_ms = (backoff / 4).as_millis();
    let jitter_ms = if jitter_limit_ms == 0 {
        0
    } else {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            % (jitter_limit_ms + 1)
    };
    backoff
        .saturating_add(Duration::from_millis(
            jitter_ms.try_into().unwrap_or(u64::MAX),
        ))
        .min(policy.max_delay)
}

fn model_retry_reason(failure: &ModelFailure, policy: ModelRetryPolicy) -> String {
    let phase = match failure.phase() {
        ModelFailurePhase::Request => "request",
        ModelFailurePhase::Stream => "stream",
        ModelFailurePhase::Response => "response",
    };
    let mut reason = failure.status().map_or_else(
        || format!("{} during {phase}", policy.reason),
        |status| {
            format!(
                "{} during {phase} (HTTP {})",
                policy.reason,
                status.as_u16()
            )
        },
    );
    if let Some(request_id) = failure.provider_request_id() {
        reason.push_str(&format!("; request id {request_id}"));
    }
    reason
}

fn memory_image_attachments(images: &[ImageAttachment]) -> Vec<UserContent> {
    images
        .iter()
        .map(|image| {
            UserContent::image_base64(
                base64::engine::general_purpose::STANDARD.encode(&image.bytes),
                Some(ImageMediaType::PNG),
                None,
            )
        })
        .collect()
}

async fn image_attachments(prompt: &str, cwd: &Path) -> Result<Vec<UserContent>> {
    let arguments = prompt_arguments(prompt);
    let root = tokio::fs::canonicalize(cwd)
        .await
        .with_context(|| format!("cannot resolve project directory {}", display_path(cwd)))?;
    let mut images = Vec::new();
    for argument in arguments {
        let Some(path) = argument.strip_prefix('@').filter(|path| !path.is_empty()) else {
            continue;
        };
        let path = path.strip_prefix("file://").unwrap_or(path);
        let path = PathBuf::from(path);
        let path = if path.is_absolute() {
            path
        } else {
            root.join(path)
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
            .with_context(|| format!("cannot attach image {}", display_path(&path)))?;
        if !path_is_within(&canonical, &root) {
            bail!(
                "image attachment is outside the project boundary: {}",
                display_path(&canonical)
            );
        }
        let bytes = tokio::fs::read(&canonical)
            .await
            .with_context(|| format!("cannot read image {}", display_path(&canonical)))?;
        let media_type = detect_image_type(&bytes).with_context(|| {
            format!(
                "unsupported or invalid image file: {}",
                display_path(&canonical)
            )
        })?;
        images.push(UserContent::image_base64(
            base64::engine::general_purpose::STANDARD.encode(bytes),
            Some(media_type),
            None,
        ));
    }
    Ok(images)
}

fn prompt_arguments(prompt: &str) -> Vec<String> {
    let mut arguments = Vec::new();
    let mut chars = prompt.chars().peekable();
    while chars.peek().is_some() {
        while chars.peek().is_some_and(|ch| ch.is_whitespace()) {
            chars.next();
        }
        if chars.peek().is_none() {
            break;
        }
        arguments.push(take_prompt_argument(&mut chars));
    }
    arguments
}

fn take_prompt_argument(chars: &mut std::iter::Peekable<impl Iterator<Item = char>>) -> String {
    let mut argument = String::new();
    if chars.peek() == Some(&'@') {
        argument.push('@');
        chars.next();
    }
    if let Some(&quote) = chars.peek().filter(|ch| **ch == '"' || **ch == '\'') {
        chars.next();
        for ch in chars.by_ref() {
            if ch == quote {
                break;
            }
            argument.push(ch);
        }
        return argument;
    }
    while let Some(&ch) = chars.peek() {
        if ch.is_whitespace() {
            break;
        }
        argument.push(ch);
        chars.next();
    }
    argument
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
        accepts_images: bool,
    }

    struct ScriptedBackend {
        responses: Mutex<VecDeque<Result<ModelResponse>>>,
        requests: Mutex<Vec<ModelRequest>>,
    }

    struct BlockingBackend {
        started: tokio::sync::Notify,
        release: tokio::sync::Notify,
    }

    struct GatedBackend {
        responses: Mutex<VecDeque<Result<ModelResponse>>>,
        requests: Mutex<Vec<ModelRequest>>,
        started: mpsc::UnboundedSender<()>,
        release: tokio::sync::Semaphore,
    }

    fn scripted_failure(
        kind: ModelFailureKind,
        retry_after: Option<Duration>,
        message: &str,
    ) -> Result<ModelResponse> {
        Err(ModelFailure::for_test(kind, retry_after, message).into())
    }

    #[async_trait]
    impl ModelBackend for FakeBackend {
        fn accepts_image_input(&self) -> bool {
            self.accepts_images
        }

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

    #[async_trait]
    impl ModelBackend for ScriptedBackend {
        async fn complete(
            &self,
            request: ModelRequest,
            _deltas: mpsc::UnboundedSender<ModelDelta>,
        ) -> Result<ModelResponse> {
            self.requests.lock().await.push(request);
            self.responses.lock().await.pop_front().unwrap()
        }
    }

    #[async_trait]
    impl ModelBackend for BlockingBackend {
        async fn complete(
            &self,
            _request: ModelRequest,
            _deltas: mpsc::UnboundedSender<ModelDelta>,
        ) -> Result<ModelResponse> {
            self.started.notify_one();
            self.release.notified().await;
            Ok(ModelResponse {
                content: vec![AssistantContent::text("released")],
                usage: None,
            })
        }
    }

    #[async_trait]
    impl ModelBackend for GatedBackend {
        fn accepts_image_input(&self) -> bool {
            true
        }

        async fn complete(
            &self,
            request: ModelRequest,
            _deltas: mpsc::UnboundedSender<ModelDelta>,
        ) -> Result<ModelResponse> {
            self.requests.lock().await.push(request);
            let _ = self.started.send(());
            self.release.acquire().await.unwrap().forget();
            self.responses.lock().await.pop_front().unwrap()
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

    async fn test_runtime(
        workspace: &Path,
        backend: Arc<dyn ModelBackend>,
        limits: ModelLimits,
    ) -> (Arc<AgentRuntime>, Session, PathBuf) {
        let session_id = format!("test{}", uuid::Uuid::now_v7().simple());
        let session = crate::session::Session::open_at(
            workspace.join("sessions.db"),
            Some(&session_id),
            workspace,
            "fake",
            "fake-model",
            SessionContext {
                system_prompt: "system".to_string(),
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
        let runtime = Arc::new(AgentRuntime::new(
            Some(backend),
            Arc::new(ProtocolRegistry::new(output, TaskManager::new())),
            session.clone(),
            "system".to_string(),
            limits,
        ));
        (runtime, session, output_directory)
    }

    fn tool_call_response(id: &str) -> Result<ModelResponse> {
        Ok(ModelResponse {
            content: vec![AssistantContent::ToolCall(ToolCall::new(
                ToolCallId::new(id).unwrap(),
                ToolFunction::new(
                    "read".to_string(),
                    serde_json::json!({"uri": "missing://help"}),
                ),
            ))],
            usage: None,
        })
    }

    fn text_response(text: &str) -> Result<ModelResponse> {
        Ok(ModelResponse {
            content: vec![AssistantContent::text(text)],
            usage: None,
        })
    }

    async fn wait_for_turn(runtime: &AgentRuntime) {
        tokio::time::timeout(Duration::from_secs(1), async {
            while runtime.turn_running().await {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("turn should settle");
    }

    #[tokio::test]
    async fn guidance_reaches_the_next_model_boundary_before_queued_follow_up() {
        let workspace = tempfile::tempdir().unwrap();
        let (started, mut requests_started) = mpsc::unbounded_channel();
        let backend = Arc::new(GatedBackend {
            responses: Mutex::new(VecDeque::from([
                tool_call_response("first-call"),
                text_response("current turn complete"),
                text_response("queued turn complete"),
            ])),
            requests: Mutex::new(Vec::new()),
            started,
            release: tokio::sync::Semaphore::new(0),
        });
        let (runtime, session, output_directory) =
            test_runtime(workspace.path(), backend.clone(), ModelLimits::default()).await;

        runtime.start_turn("initial".into()).await.unwrap();
        requests_started.recv().await.unwrap();
        let queued = runtime
            .enqueue_message("follow up".into(), PendingMessageKind::Queued)
            .await
            .unwrap();
        let guidance = runtime
            .enqueue_message("change direction".into(), PendingMessageKind::Guidance)
            .await
            .unwrap();
        assert_eq!(runtime.pending_messages().await, [queued.clone(), guidance]);

        backend.release.add_permits(1);
        requests_started.recv().await.unwrap();
        {
            let requests = backend.requests.lock().await;
            assert_eq!(
                requests[1].history.last(),
                Some(&Message::user("change direction"))
            );
            assert!(!requests[1].history.contains(&Message::user("follow up")));
        }
        assert_eq!(runtime.pending_messages().await, [queued]);

        backend.release.add_permits(1);
        requests_started.recv().await.unwrap();
        {
            let requests = backend.requests.lock().await;
            assert_eq!(
                requests[2].history.last(),
                Some(&Message::user("follow up"))
            );
        }
        assert!(runtime.pending_messages().await.is_empty());

        backend.release.add_permits(1);
        wait_for_turn(runtime.as_ref()).await;
        let user_messages = session
            .snapshot()
            .await
            .into_iter()
            .filter_map(|event| match event.kind {
                EventKind::User { text } => Some(text),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(user_messages, ["initial", "change direction", "follow up"]);
        runtime.shutdown().await;
        let _ = tokio::fs::remove_dir_all(output_directory).await;
    }

    #[tokio::test]
    async fn guidance_queued_during_compaction_reaches_the_following_model_request() {
        let workspace = tempfile::tempdir().unwrap();
        let (started, mut requests_started) = mpsc::unbounded_channel();
        let backend = Arc::new(GatedBackend {
            responses: Mutex::new(VecDeque::from([
                text_response("summary"),
                text_response("guided answer"),
            ])),
            requests: Mutex::new(Vec::new()),
            started,
            release: tokio::sync::Semaphore::new(0),
        });
        let (runtime, session, output_directory) = test_runtime(
            workspace.path(),
            backend.clone(),
            ModelLimits {
                context_window: 64,
                ..ModelLimits::default()
            },
        )
        .await;
        let old_context = "old context ".repeat(100);
        session
            .append_batch(vec![
                EventKind::User {
                    text: old_context.clone(),
                },
                EventKind::ModelMessage {
                    message: Message::user(old_context),
                },
                EventKind::AssistantText {
                    text: "old answer".into(),
                },
                EventKind::ModelMessage {
                    message: Message::assistant("old answer"),
                },
                EventKind::TurnFinished,
            ])
            .await
            .unwrap();

        runtime.start_turn("current task".into()).await.unwrap();
        requests_started.recv().await.unwrap();
        assert!(!backend.requests.lock().await[0].tools);
        runtime
            .enqueue_message("new constraint".into(), PendingMessageKind::Guidance)
            .await
            .unwrap();

        backend.release.add_permits(1);
        requests_started.recv().await.unwrap();
        {
            let requests = backend.requests.lock().await;
            assert!(requests[1].tools);
            assert_eq!(
                requests[1].history.last(),
                Some(&Message::user("new constraint"))
            );
        }
        assert!(runtime.pending_messages().await.is_empty());

        backend.release.add_permits(1);
        wait_for_turn(runtime.as_ref()).await;
        runtime.shutdown().await;
        let _ = tokio::fs::remove_dir_all(output_directory).await;
    }

    #[tokio::test]
    async fn queued_message_can_be_upgraded_until_guidance_is_delivered() {
        let workspace = tempfile::tempdir().unwrap();
        let (started, mut requests_started) = mpsc::unbounded_channel();
        let backend = Arc::new(GatedBackend {
            responses: Mutex::new(VecDeque::from([
                tool_call_response("first-call"),
                text_response("guided"),
            ])),
            requests: Mutex::new(Vec::new()),
            started,
            release: tokio::sync::Semaphore::new(0),
        });
        let (runtime, _session, output_directory) =
            test_runtime(workspace.path(), backend.clone(), ModelLimits::default()).await;

        runtime.start_turn("initial".into()).await.unwrap();
        requests_started.recv().await.unwrap();
        let queued = runtime
            .enqueue_message("urgent correction".into(), PendingMessageKind::Queued)
            .await
            .unwrap();
        let upgraded = runtime.upgrade_latest_queued().await.unwrap();
        assert_eq!(upgraded.id, queued.id);
        assert_eq!(upgraded.kind, PendingMessageKind::Guidance);

        backend.release.add_permits(1);
        requests_started.recv().await.unwrap();
        assert!(runtime.pending_messages().await.is_empty());
        assert!(runtime.cancel_latest_pending().await.is_none());
        assert_eq!(
            backend.requests.lock().await[1].history.last(),
            Some(&Message::user("urgent correction"))
        );

        backend.release.add_permits(1);
        wait_for_turn(runtime.as_ref()).await;
        runtime.shutdown().await;
        let _ = tokio::fs::remove_dir_all(output_directory).await;
    }

    #[tokio::test]
    async fn queued_and_guidance_messages_can_be_restored_before_delivery() {
        let workspace = tempfile::tempdir().unwrap();
        let (started, mut requests_started) = mpsc::unbounded_channel();
        let backend = Arc::new(GatedBackend {
            responses: Mutex::new(VecDeque::from([text_response("done")])),
            requests: Mutex::new(Vec::new()),
            started,
            release: tokio::sync::Semaphore::new(0),
        });
        let (runtime, _session, output_directory) =
            test_runtime(workspace.path(), backend.clone(), ModelLimits::default()).await;

        runtime.start_turn("initial".into()).await.unwrap();
        requests_started.recv().await.unwrap();
        runtime
            .enqueue_message("queued".into(), PendingMessageKind::Queued)
            .await
            .unwrap();
        runtime
            .enqueue_message("guidance".into(), PendingMessageKind::Guidance)
            .await
            .unwrap();

        assert_eq!(
            runtime.cancel_latest_pending().await.unwrap().0.text,
            "guidance"
        );
        assert_eq!(
            runtime.cancel_latest_pending().await.unwrap().0.text,
            "queued"
        );
        assert!(runtime.pending_messages().await.is_empty());

        backend.release.add_permits(1);
        wait_for_turn(runtime.as_ref()).await;
        assert_eq!(backend.requests.lock().await.len(), 1);
        runtime.shutdown().await;
        let _ = tokio::fs::remove_dir_all(output_directory).await;
    }

    #[tokio::test]
    async fn queued_clipboard_images_are_delivered_and_restored_with_their_messages() {
        let workspace = tempfile::tempdir().unwrap();
        let (started, mut requests_started) = mpsc::unbounded_channel();
        let backend = Arc::new(GatedBackend {
            responses: Mutex::new(VecDeque::from([
                text_response("current turn complete"),
                text_response("follow-up complete"),
            ])),
            requests: Mutex::new(Vec::new()),
            started,
            release: tokio::sync::Semaphore::new(0),
        });
        let (runtime, _session, output_directory) =
            test_runtime(workspace.path(), backend.clone(), ModelLimits::default()).await;
        let delivered_image = ImageAttachment::png(b"queued-image".to_vec());
        let restored_image = ImageAttachment::png(b"restored-image".to_vec());

        runtime.start_turn("initial".into()).await.unwrap();
        requests_started.recv().await.unwrap();
        runtime
            .enqueue_message_with_images(
                "inspect queued image".into(),
                vec![delivered_image],
                PendingMessageKind::Queued,
            )
            .await
            .unwrap();
        runtime
            .enqueue_message_with_images(
                "restore this image".into(),
                vec![restored_image.clone()],
                PendingMessageKind::Queued,
            )
            .await
            .unwrap();

        let (restored, images) = runtime.cancel_latest_pending().await.unwrap();
        assert_eq!(restored.text, "restore this image");
        assert_eq!(images, [restored_image]);

        backend.release.add_permits(1);
        requests_started.recv().await.unwrap();
        let requests = backend.requests.lock().await;
        let Message::User { content } = requests[1].history.last().unwrap() else {
            panic!("queued user message should be last")
        };
        assert_eq!(
            content.first(),
            Some(&UserContent::text("inspect queued image"))
        );
        assert!(matches!(
            content.get(1),
            Some(UserContent::Image(image))
                if matches!(&image.data, rig::message::DocumentSourceKind::Base64(data)
                    if base64::engine::general_purpose::STANDARD.decode(data).unwrap()
                        == b"queued-image")
        ));
        drop(requests);

        backend.release.add_permits(1);
        wait_for_turn(runtime.as_ref()).await;
        runtime.shutdown().await;
        let _ = tokio::fs::remove_dir_all(output_directory).await;
    }

    #[tokio::test]
    async fn failed_follow_up_delivery_returns_the_message_to_the_pending_queue() {
        let workspace = tempfile::tempdir().unwrap();
        let (started, mut requests_started) = mpsc::unbounded_channel();
        let backend = Arc::new(GatedBackend {
            responses: Mutex::new(VecDeque::from([text_response("current turn complete")])),
            requests: Mutex::new(Vec::new()),
            started,
            release: tokio::sync::Semaphore::new(0),
        });
        let (runtime, session, output_directory) =
            test_runtime(workspace.path(), backend.clone(), ModelLimits::default()).await;
        session.save_draft("existing draft").await.unwrap();

        runtime.start_turn("initial".into()).await.unwrap();
        requests_started.recv().await.unwrap();
        runtime
            .enqueue_message("not delivered".into(), PendingMessageKind::Queued)
            .await
            .unwrap();
        runtime.set_backend(None, None).await;
        backend.release.add_permits(1);
        wait_for_turn(runtime.as_ref()).await;

        assert_eq!(runtime.pending_messages().await[0].text, "not delivered");
        runtime.shutdown().await;
        assert!(runtime.pending_messages().await.is_empty());
        assert_eq!(session.draft().await, "not delivered\n\nexisting draft");
        let _ = tokio::fs::remove_dir_all(output_directory).await;
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

    #[test]
    fn prompt_arguments_keep_windows_paths_and_quoted_names() {
        assert_eq!(
            prompt_arguments(r"inspect @C:\Users\4fu\screen.png"),
            ["inspect", r"@C:\Users\4fu\screen.png"]
        );
        assert_eq!(
            prompt_arguments(r#"inspect @"my shot.png""#),
            ["inspect", "@my shot.png"]
        );
        assert_eq!(
            prompt_arguments(r#"inspect "@my shot.png""#),
            ["inspect", "@my shot.png"]
        );
        assert_eq!(
            prompt_arguments(r#"inspect @"file://my shot.png""#),
            ["inspect", "@file://my shot.png"]
        );
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

        let uri_attachments = image_attachments("inspect @file://screen.png", workspace.path())
            .await
            .unwrap();
        assert_eq!(uri_attachments.len(), 1);

        let absolute = image_attachments(&format!("inspect @{}", path.display()), workspace.path())
            .await
            .unwrap();
        assert_eq!(absolute.len(), 1);
    }

    #[tokio::test]
    async fn clipboard_and_path_images_share_the_user_message() {
        let workspace = tempfile::tempdir().unwrap();
        tokio::fs::write(
            workspace.path().join("screen.png"),
            b"\x89PNG\r\n\x1a\npath-image",
        )
        .await
        .unwrap();
        let backend = Arc::new(FakeBackend {
            responses: Mutex::new(VecDeque::from([(
                vec![AssistantContent::text("Done")],
                None,
            )])),
            requests: Mutex::new(Vec::new()),
            accepts_images: true,
        });
        let (runtime, session, output_directory) =
            test_runtime(workspace.path(), backend.clone(), ModelLimits::default()).await;

        runtime
            .run_turn_with_images(
                "inspect @screen.png".to_string(),
                vec![ImageAttachment::png(
                    b"\x89PNG\r\n\x1a\nclipboard-image".to_vec(),
                )],
            )
            .await
            .unwrap();

        let requests = backend.requests.lock().await;
        let images = requests[0]
            .history
            .iter()
            .find_map(|message| match message {
                Message::User { content } => Some(
                    content
                        .iter()
                        .filter_map(|content| match content {
                            UserContent::Image(image) => Some(image),
                            _ => None,
                        })
                        .collect::<Vec<_>>(),
                ),
                _ => None,
            })
            .unwrap();
        assert_eq!(images.len(), 2);
        assert!(
            images
                .iter()
                .all(|image| image.media_type == Some(ImageMediaType::PNG))
        );
        assert!(matches!(
            &images[0].data,
            rig::message::DocumentSourceKind::Base64(data)
                if base64::engine::general_purpose::STANDARD.decode(data).unwrap()
                    == b"\x89PNG\r\n\x1a\nclipboard-image"
        ));

        drop(requests);
        drop(runtime);
        drop(session);
        let _ = tokio::fs::remove_dir_all(output_directory).await;
    }

    #[tokio::test]
    async fn text_only_backends_reject_clipboard_images_before_recording_the_turn() {
        let workspace = tempfile::tempdir().unwrap();
        let backend = Arc::new(FakeBackend::default());
        let (runtime, session, output_directory) =
            test_runtime(workspace.path(), backend.clone(), ModelLimits::default()).await;

        let error = runtime
            .run_turn_with_images(
                "inspect this".to_string(),
                vec![ImageAttachment::png(
                    b"\x89PNG\r\n\x1a\nclipboard-image".to_vec(),
                )],
            )
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
        let project = workspace.path().join("project");
        tokio::fs::create_dir(&project).await.unwrap();
        tokio::fs::write(project.join("screen.png"), b"\x89PNG\r\n\x1a\nimage-data")
            .await
            .unwrap();
        let session_id = format!("test{}", uuid::Uuid::now_v7().simple());
        let session = crate::session::Session::open_at(
            workspace.path().join("session-data/sessions.db"),
            Some(&session_id),
            &project,
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
    async fn image_attachments_in_the_session_directory_are_outside_the_project_boundary() {
        let workspace = tempfile::tempdir().unwrap();
        let project = workspace.path().join("project");
        tokio::fs::create_dir(&project).await.unwrap();
        let session_id = format!("test{}", uuid::Uuid::now_v7().simple());
        let session = crate::session::Session::open_at(
            workspace.path().join("session-data/sessions.db"),
            Some(&session_id),
            &project,
            "fake",
            "text-only",
            SessionContext {
                system_prompt: "system".to_string(),
                skills: Vec::new(),
            },
        )
        .await
        .unwrap();
        let outside = session.directory().join("outside.png");
        tokio::fs::write(&outside, b"\x89PNG\r\n\x1a\nimage-data")
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
            .run_turn(format!("inspect @{}", outside.display()))
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("outside the project boundary"));
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
        let environment = Arc::new(
            crate::config::AgentEnvironment::load(workspace.path())
                .await
                .unwrap(),
        );
        let manager = crate::config::ConfigManager::load_for_test(
            &workspace.path().join("config"),
            workspace.path(),
        )
        .await
        .unwrap();
        crate::builtins::plugins(workspace.path())
            .install(
                &mut crate::plugin::PluginHost::new(
                    &mut protocols,
                    &mut commands,
                    &mut tui,
                    environment,
                )
                .with_credentials(manager),
            )
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
            accepts_images: false,
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
            accepts_images: false,
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
        let requests = backend.requests.lock().await;
        assert_eq!(requests[0].system, compaction::SUMMARY_SYSTEM_PROMPT);
        assert!(!requests[0].system.contains("frozen system"));
        assert!(!requests[0].tools);

        let _ = tokio::fs::remove_dir_all(output_directory).await;
    }

    #[tokio::test]
    async fn manual_compaction_rejects_context_at_or_below_twenty_percent() {
        let workspace = tempfile::tempdir().unwrap();
        let session_id = format!("test{}", uuid::Uuid::now_v7().simple());
        let session = crate::session::Session::open_at(
            workspace.path().join("sessions.db"),
            Some(&session_id),
            workspace.path(),
            "fake",
            "fake-model",
            SessionContext {
                system_prompt: "system".to_string(),
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
        let backend = Arc::new(FakeBackend::default());
        let runtime = AgentRuntime::new(
            Some(backend.clone()),
            Arc::new(ProtocolRegistry::new(output, TaskManager::new())),
            session.clone(),
            "system".to_string(),
            ModelLimits {
                context_window: 128_000,
                ..ModelLimits::default()
            },
        );

        let error = runtime.compact().await.unwrap_err();

        assert_eq!(
            error.to_string(),
            "manual compaction requires context usage above 20%"
        );
        assert!(backend.requests.lock().await.is_empty());
        assert!(
            !session
                .snapshot()
                .await
                .iter()
                .any(|event| matches!(event.kind, EventKind::Compaction { .. }))
        );

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
            accepts_images: false,
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
        assert_eq!(requests[0].system, compaction::SUMMARY_SYSTEM_PROMPT);
        assert!(!requests[0].tools);
        assert_eq!(requests[1].system, "frozen system");
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

    #[tokio::test]
    async fn automatic_compaction_retries_a_transient_summary_failure() {
        let workspace = tempfile::tempdir().unwrap();
        let backend = Arc::new(ScriptedBackend {
            responses: Mutex::new(VecDeque::from([
                scripted_failure(
                    ModelFailureKind::Server,
                    Some(Duration::ZERO),
                    "service unavailable",
                ),
                Ok(ModelResponse {
                    content: vec![AssistantContent::text("summary")],
                    usage: None,
                }),
                Ok(ModelResponse {
                    content: vec![AssistantContent::text("answer")],
                    usage: None,
                }),
            ])),
            requests: Mutex::new(Vec::new()),
        });
        let (runtime, session, output_directory) = test_runtime(
            workspace.path(),
            backend.clone(),
            ModelLimits {
                context_window: 64,
                ..ModelLimits::default()
            },
        )
        .await;
        session
            .append_batch(vec![
                EventKind::User {
                    text: "old task".into(),
                },
                EventKind::ModelMessage {
                    message: Message::user("old task"),
                },
                EventKind::AssistantText {
                    text: "old answer".into(),
                },
                EventKind::ModelMessage {
                    message: Message::assistant("old answer"),
                },
            ])
            .await
            .unwrap();

        runtime.run_turn("current task".into()).await.unwrap();

        let requests = backend.requests.lock().await;
        assert_eq!(requests.len(), 3);
        assert!(!requests[0].tools);
        assert!(!requests[1].tools);
        assert!(requests[2].tools);
        drop(requests);
        assert!(session.snapshot().await.iter().any(|event| matches!(
            &event.kind,
            EventKind::ModelRetry { attempt: 1, max_retries: 5, reason, .. }
                if reason.contains("server error")
        )));
        let _ = tokio::fs::remove_dir_all(output_directory).await;
    }

    #[tokio::test]
    async fn automatic_compaction_retries_a_blank_summary() {
        let workspace = tempfile::tempdir().unwrap();
        let backend = Arc::new(ScriptedBackend {
            responses: Mutex::new(VecDeque::from([
                Ok(ModelResponse {
                    content: vec![AssistantContent::text("  \n")],
                    usage: None,
                }),
                Ok(ModelResponse {
                    content: vec![AssistantContent::text("summary")],
                    usage: None,
                }),
                Ok(ModelResponse {
                    content: vec![AssistantContent::text("answer")],
                    usage: None,
                }),
            ])),
            requests: Mutex::new(Vec::new()),
        });
        let (runtime, session, output_directory) = test_runtime(
            workspace.path(),
            backend.clone(),
            ModelLimits {
                context_window: 64,
                ..ModelLimits::default()
            },
        )
        .await;
        session
            .append_batch(vec![
                EventKind::User {
                    text: "old task".into(),
                },
                EventKind::ModelMessage {
                    message: Message::user("old task"),
                },
                EventKind::AssistantText {
                    text: "old answer".into(),
                },
                EventKind::ModelMessage {
                    message: Message::assistant("old answer"),
                },
            ])
            .await
            .unwrap();

        runtime.run_turn("current task".into()).await.unwrap();

        assert_eq!(backend.requests.lock().await.len(), 3);
        assert!(session.snapshot().await.iter().any(|event| matches!(
            event.kind,
            EventKind::ModelRetry {
                attempt: 1,
                max_retries: 4,
                ..
            }
        )));
        let _ = tokio::fs::remove_dir_all(output_directory).await;
    }

    #[test]
    fn context_overflow_classifier_excludes_rate_limits() {
        assert!(is_context_overflow(&anyhow!(
            "Your input exceeds the context window of this model"
        )));
        assert!(is_context_overflow(&anyhow!(
            "prompt has 200,000 tokens, but the configured context size is 128,000 tokens"
        )));
        assert!(!is_context_overflow(&anyhow!(
            "Throttling error: too many tokens; rate limit exceeded"
        )));
    }

    #[test]
    fn retry_budgets_are_distinct_and_permanent_errors_are_not_retried() {
        for (kind, expected) in [
            (ModelFailureKind::RateLimit, 6),
            (ModelFailureKind::Network, 5),
            (ModelFailureKind::Server, 5),
            (ModelFailureKind::Timeout, 4),
            (ModelFailureKind::Conflict, 4),
            (ModelFailureKind::EmptyResponse, 4),
        ] {
            assert_eq!(model_retry_policy(kind).unwrap().max_retries, expected);
        }
        for kind in [
            ModelFailureKind::ContextOverflow,
            ModelFailureKind::Authentication,
            ModelFailureKind::Quota,
            ModelFailureKind::Client,
            ModelFailureKind::Other,
        ] {
            assert!(model_retry_policy(kind).is_none());
        }
    }

    #[test]
    fn retry_after_is_honored_with_a_sixty_second_cap() {
        let policy = model_retry_policy(ModelFailureKind::RateLimit).unwrap();
        let short = ModelFailure::for_test(
            ModelFailureKind::RateLimit,
            Some(Duration::from_secs(12)),
            "rate limited",
        );
        let long = ModelFailure::for_test(
            ModelFailureKind::RateLimit,
            Some(Duration::from_secs(120)),
            "rate limited",
        );
        assert_eq!(
            model_retry_delay(&short, policy, 1),
            Duration::from_secs(12)
        );
        assert_eq!(model_retry_delay(&long, policy, 1), MAX_RETRY_AFTER);
    }

    #[tokio::test]
    async fn network_failure_can_use_its_full_retry_budget_and_then_succeed() {
        let workspace = tempfile::tempdir().unwrap();
        let mut responses = VecDeque::new();
        for _ in 0..5 {
            responses.push_back(scripted_failure(
                ModelFailureKind::Network,
                Some(Duration::ZERO),
                "connection reset",
            ));
        }
        responses.push_back(Ok(ModelResponse {
            content: vec![AssistantContent::text("recovered")],
            usage: None,
        }));
        let backend = Arc::new(ScriptedBackend {
            responses: Mutex::new(responses),
            requests: Mutex::new(Vec::new()),
        });
        let (runtime, session, output_directory) =
            test_runtime(workspace.path(), backend.clone(), ModelLimits::default()).await;

        runtime.run_turn("retry this".into()).await.unwrap();

        assert_eq!(backend.requests.lock().await.len(), 6);
        let retries = session
            .snapshot()
            .await
            .into_iter()
            .filter_map(|event| match event.kind {
                EventKind::ModelRetry {
                    attempt,
                    max_retries,
                    ..
                } => Some((attempt, max_retries)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(retries, [(1, 5), (2, 5), (3, 5), (4, 5), (5, 5)]);
        let _ = tokio::fs::remove_dir_all(output_directory).await;
    }

    #[tokio::test]
    async fn timeout_failure_stops_after_its_smaller_retry_budget() {
        let workspace = tempfile::tempdir().unwrap();
        let responses = (0..5)
            .map(|_| {
                scripted_failure(
                    ModelFailureKind::Timeout,
                    Some(Duration::ZERO),
                    "request timed out",
                )
            })
            .collect();
        let backend = Arc::new(ScriptedBackend {
            responses: Mutex::new(responses),
            requests: Mutex::new(Vec::new()),
        });
        let (runtime, session, output_directory) =
            test_runtime(workspace.path(), backend.clone(), ModelLimits::default()).await;

        let error = runtime.run_turn("retry this".into()).await.unwrap_err();

        assert!(error.to_string().contains("request timed out"));
        assert_eq!(backend.requests.lock().await.len(), 5);
        assert_eq!(
            session
                .snapshot()
                .await
                .iter()
                .filter(|event| matches!(event.kind, EventKind::ModelRetry { .. }))
                .count(),
            4
        );
        let _ = tokio::fs::remove_dir_all(output_directory).await;
    }

    #[tokio::test]
    async fn changing_failure_kind_uses_each_types_own_budget() {
        let workspace = tempfile::tempdir().unwrap();
        let mut responses = VecDeque::new();
        for _ in 0..5 {
            responses.push_back(scripted_failure(
                ModelFailureKind::Network,
                Some(Duration::ZERO),
                "connection reset",
            ));
        }
        responses.push_back(scripted_failure(
            ModelFailureKind::RateLimit,
            Some(Duration::ZERO),
            "rate limited",
        ));
        responses.push_back(Ok(ModelResponse {
            content: vec![AssistantContent::text("recovered")],
            usage: None,
        }));
        let backend = Arc::new(ScriptedBackend {
            responses: Mutex::new(responses),
            requests: Mutex::new(Vec::new()),
        });
        let (runtime, session, output_directory) =
            test_runtime(workspace.path(), backend.clone(), ModelLimits::default()).await;

        runtime.run_turn("retry this".into()).await.unwrap();

        assert_eq!(backend.requests.lock().await.len(), 7);
        let retries = session
            .snapshot()
            .await
            .into_iter()
            .filter_map(|event| match event.kind {
                EventKind::ModelRetry {
                    attempt,
                    max_retries,
                    ..
                } => Some((attempt, max_retries)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(retries, [(1, 5), (2, 5), (3, 5), (4, 5), (5, 5), (1, 6)]);
        let _ = tokio::fs::remove_dir_all(output_directory).await;
    }

    #[tokio::test]
    async fn authentication_failure_is_not_retried() {
        let workspace = tempfile::tempdir().unwrap();
        let backend = Arc::new(ScriptedBackend {
            responses: Mutex::new(VecDeque::from([scripted_failure(
                ModelFailureKind::Authentication,
                Some(Duration::ZERO),
                "invalid credential",
            )])),
            requests: Mutex::new(Vec::new()),
        });
        let (runtime, session, output_directory) =
            test_runtime(workspace.path(), backend.clone(), ModelLimits::default()).await;

        runtime.run_turn("do not retry".into()).await.unwrap_err();

        assert_eq!(backend.requests.lock().await.len(), 1);
        assert!(
            !session
                .snapshot()
                .await
                .iter()
                .any(|event| matches!(event.kind, EventKind::ModelRetry { .. }))
        );
        let _ = tokio::fs::remove_dir_all(output_directory).await;
    }

    #[tokio::test]
    async fn user_interrupt_cancels_retry_backoff() {
        let workspace = tempfile::tempdir().unwrap();
        let backend = Arc::new(ScriptedBackend {
            responses: Mutex::new(VecDeque::from([scripted_failure(
                ModelFailureKind::RateLimit,
                Some(Duration::from_secs(60)),
                "rate limited",
            )])),
            requests: Mutex::new(Vec::new()),
        });
        let (runtime, session, output_directory) =
            test_runtime(workspace.path(), backend, ModelLimits::default()).await;
        runtime.start_turn("wait then retry".into()).await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if session
                    .snapshot()
                    .await
                    .iter()
                    .any(|event| matches!(event.kind, EventKind::ModelRetry { .. }))
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("retry boundary should be recorded before backoff");

        assert!(runtime.interrupt_turn().await);
        tokio::time::timeout(Duration::from_secs(1), async {
            while runtime.turn_running().await {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("user interruption should cancel retry backoff promptly");

        assert!(session.snapshot().await.iter().any(|event| matches!(
            &event.kind,
            EventKind::Error { text } if text == TURN_INTERRUPTED_BY_USER
        )));
        runtime.shutdown().await;
        let _ = tokio::fs::remove_dir_all(output_directory).await;
    }

    #[tokio::test]
    async fn first_context_overflow_compacts_and_retries_once() {
        let workspace = tempfile::tempdir().unwrap();
        let backend = Arc::new(ScriptedBackend {
            responses: Mutex::new(VecDeque::from([
                Err(anyhow!(
                    "Your input exceeds the context window of this model"
                )),
                Ok(ModelResponse {
                    content: vec![AssistantContent::text("summary of old work")],
                    usage: None,
                }),
                Ok(ModelResponse {
                    content: vec![AssistantContent::text("answer after retry")],
                    usage: None,
                }),
            ])),
            requests: Mutex::new(Vec::new()),
        });
        let (runtime, session, output_directory) = test_runtime(
            workspace.path(),
            backend.clone(),
            ModelLimits {
                context_window: 128_000,
                ..ModelLimits::default()
            },
        )
        .await;
        session
            .append_batch(vec![
                EventKind::User {
                    text: "old task".into(),
                },
                EventKind::ModelMessage {
                    message: Message::user("old task"),
                },
                EventKind::AssistantText {
                    text: "old answer".into(),
                },
                EventKind::ModelMessage {
                    message: Message::assistant("old answer"),
                },
            ])
            .await
            .unwrap();

        runtime.run_turn("current task".into()).await.unwrap();

        let requests = backend.requests.lock().await;
        assert_eq!(requests.len(), 3);
        assert!(requests[0].tools);
        assert_eq!(requests[1].system, compaction::SUMMARY_SYSTEM_PROMPT);
        assert!(requests[2].tools);
        drop(requests);
        assert_eq!(
            session
                .snapshot()
                .await
                .iter()
                .filter(|event| matches!(event.kind, EventKind::Compaction { .. }))
                .count(),
            1
        );
        assert!(matches!(
            session.snapshot().await.last().map(|event| &event.kind),
            Some(EventKind::TurnFinished)
        ));
        let _ = tokio::fs::remove_dir_all(output_directory).await;
    }

    #[tokio::test]
    async fn second_context_overflow_settles_without_an_infinite_retry() {
        let workspace = tempfile::tempdir().unwrap();
        let backend = Arc::new(ScriptedBackend {
            responses: Mutex::new(VecDeque::from([
                Err(anyhow!("prompt is too long")),
                Ok(ModelResponse {
                    content: vec![AssistantContent::text("summary")],
                    usage: None,
                }),
                Err(anyhow!("prompt is too long")),
            ])),
            requests: Mutex::new(Vec::new()),
        });
        let (runtime, session, output_directory) = test_runtime(
            workspace.path(),
            backend.clone(),
            ModelLimits {
                context_window: 128_000,
                ..ModelLimits::default()
            },
        )
        .await;
        session
            .append_batch(vec![
                EventKind::User {
                    text: "old task".into(),
                },
                EventKind::ModelMessage {
                    message: Message::user("old task"),
                },
                EventKind::AssistantText {
                    text: "old answer".into(),
                },
                EventKind::ModelMessage {
                    message: Message::assistant("old answer"),
                },
            ])
            .await
            .unwrap();

        let error = runtime.run_turn("current task".into()).await.unwrap_err();

        assert!(format!("{error:#}").contains("prompt is too long"));
        assert_eq!(backend.requests.lock().await.len(), 3);
        let events = session.snapshot().await;
        assert!(matches!(
            events.last().map(|event| &event.kind),
            Some(EventKind::TurnFinished)
        ));
        assert!(matches!(
            events.get(events.len() - 2).map(|event| &event.kind),
            Some(EventKind::Error { text }) if text.contains("prompt is too long")
        ));
        let _ = tokio::fs::remove_dir_all(output_directory).await;
    }

    #[tokio::test]
    async fn user_interrupt_cancels_and_durably_settles_a_running_turn() {
        let workspace = tempfile::tempdir().unwrap();
        let backend = Arc::new(BlockingBackend {
            started: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        });
        let (runtime, session, output_directory) =
            test_runtime(workspace.path(), backend.clone(), ModelLimits::default()).await;
        runtime.start_turn("long request".into()).await.unwrap();
        backend.started.notified().await;

        assert!(runtime.interrupt_turn().await);
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while runtime.turn_running().await {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("user interruption should cancel the model request promptly");
        assert!(!runtime.interrupt_turn().await);

        let events = session.snapshot().await;
        assert!(matches!(
            events.last().map(|event| &event.kind),
            Some(EventKind::TurnFinished)
        ));
        assert!(events.iter().any(|event| matches!(
            &event.kind,
            EventKind::Error { text } if text == TURN_INTERRUPTED_BY_USER
        )));
        runtime.shutdown().await;
        let _ = tokio::fs::remove_dir_all(output_directory).await;
    }

    #[tokio::test]
    async fn shutdown_cancels_and_durably_settles_a_running_turn() {
        let workspace = tempfile::tempdir().unwrap();
        let backend = Arc::new(BlockingBackend {
            started: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        });
        let (runtime, session, output_directory) =
            test_runtime(workspace.path(), backend.clone(), ModelLimits::default()).await;
        runtime.start_turn("long request".into()).await.unwrap();
        backend.started.notified().await;

        tokio::time::timeout(std::time::Duration::from_secs(1), runtime.shutdown())
            .await
            .expect("shutdown should cancel the model request promptly");

        let events = session.snapshot().await;
        assert!(matches!(
            events.last().map(|event| &event.kind),
            Some(EventKind::TurnFinished)
        ));
        assert!(events.iter().any(|event| matches!(
            &event.kind,
            EventKind::Error { text } if text == TURN_INTERRUPTED_BY_SHUTDOWN
        )));
        let _ = tokio::fs::remove_dir_all(output_directory).await;
    }

    #[tokio::test]
    async fn detached_turn_survives_its_conversation_surface_switching_away() {
        let workspace = tempfile::tempdir().unwrap();
        let backend = Arc::new(BlockingBackend {
            started: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        });
        let (runtime, session, output_directory) =
            test_runtime(workspace.path(), backend.clone(), ModelLimits::default()).await;
        runtime
            .start_turn("background request".into())
            .await
            .unwrap();
        backend.started.notified().await;
        drop(runtime);
        backend.release.notify_one();

        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if matches!(
                    session.snapshot().await.last().map(|event| &event.kind),
                    Some(EventKind::TurnFinished)
                ) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the detached turn should finish after the old surface is gone");
        assert!(session.snapshot().await.iter().any(|event| matches!(
            &event.kind,
            EventKind::AssistantText { text } if text == "released"
        )));
        let _ = tokio::fs::remove_dir_all(output_directory).await;
    }
}
