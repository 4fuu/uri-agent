use crate::agent::{
    AgentSpec, CapabilitySelection, CompactionCallback, SubmitKind, SystemPromptUpdate,
};
use crate::builtins::context::ContextState;
use crate::catalog::ModelLimits;
use crate::compaction;
use crate::model::{
    ModelBackend, ModelDelta, ModelFailure, ModelFailureKind, ModelRequest, ModelResponse,
    looks_like_cerebras_bodyless_overflow, looks_like_context_overflow, model_retry_delay,
    model_retry_policy, model_retry_reason,
};
use crate::plugin::ModelToolRegistry;
use crate::session::{ContextHint, EventKind, PendingInput, Session, SessionContext};
use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use rig::completion::{FinishReason, ToolDefinition, Usage};
use rig::message::{AssistantContent, Message, Text, ToolCall, ToolResultContent, UserContent};
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::RwLock as SyncRwLock;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::{Mutex, OnceCell, RwLock, broadcast, mpsc, watch};
use tokio::task::JoinHandle;

const TOOL_CALL_LOOP_THRESHOLD: usize = 5;
const TOOL_CALL_ARGUMENT_SUMMARY_CHARS: usize = 400;
const TOOL_CALL_RESULT_SUMMARY_CHARS: usize = 200;
const CONTEXT_REMINDER_RESERVE_MULTIPLIER: usize = 2;
const TURN_INTERRUPTED_BY_USER: &str = "turn interrupted by user";
const TURN_INTERRUPTED_BY_SHUTDOWN: &str = "turn interrupted by shutdown";

fn due_context_hint(
    context_tokens: usize,
    context_window: usize,
    settings: compaction::Settings,
    sent: u8,
) -> Option<ContextHint> {
    if context_tokens == 0 || context_window == 0 {
        return None;
    }
    let percent = context_tokens.saturating_mul(100) / context_window;
    let reminder_reserve = settings
        .reserve_for(context_window)
        .saturating_mul(CONTEXT_REMINDER_RESERVE_MULTIPLIER)
        .min(context_window);
    let final_due = context_tokens >= context_window.saturating_sub(reminder_reserve);
    [
        (ContextHint::Final, final_due),
        (ContextHint::Note50, percent >= 50),
        (ContextHint::Note30, percent >= 30),
        (ContextHint::Note15, percent >= 15),
    ]
    .into_iter()
    .find_map(|(hint, due)| (due && sent & hint.bit() == 0).then_some(hint))
}

fn context_hint_message(
    hint: ContextHint,
    window_id: u64,
    percent: usize,
    remaining: usize,
) -> String {
    match hint {
        ContextHint::Note15 => format!(
            "<context-note-hint>\n\
             Host reminder, not a user request. Window {window_id} is about {percent}% full ({remaining} tokens remaining).\n\
             Finish the work in hand. When it pauses, add a short `notes` entry if durable decisions, paths, verified results, or unfinished work are not recorded yet.\n\
             </context-note-hint>"
        ),
        ContextHint::Note30 => format!(
            "<context-note-hint>\n\
             Host reminder, not a user request. Window {window_id} is about {percent}% full ({remaining} tokens remaining).\n\
             Finish the work in hand, then update `notes` only if durable state changed since the last note.\n\
             </context-note-hint>"
        ),
        ContextHint::Note50 => format!(
            "<context-note-hint>\n\
             Host reminder, not a user request. Window {window_id} is about {percent}% full ({remaining} tokens remaining).\n\
             The host may summarize this conversation near the hard limit. Before that happens, make `notes` sufficient to resume: current goal, decisions, paths, verified results, and exact unfinished work.\n\
             </context-note-hint>"
        ),
        ContextHint::Final => format!(
            "<context-note-reminder>\n\
             Host reminder, not a user request. Window {window_id} has about {remaining} tokens remaining.\n\
             Review and update `notes` now. The host will compact the transcript near the hard limit; after compaction, notes are the durable record.\n\
             </context-note-reminder>"
        ),
    }
}

fn capability_names(selection: &CapabilitySelection) -> Option<&[String]> {
    match selection {
        CapabilitySelection::All => None,
        CapabilitySelection::Only(names) => Some(names),
    }
}

fn validate_prompt_update(prompt: &str) -> Result<()> {
    if prompt.trim().is_empty() {
        bail!("system prompt update cannot be empty");
    }
    Ok(())
}

#[derive(Default)]
struct ToolCallLoopGuard {
    last_signature: Option<String>,
    count: usize,
}

impl ToolCallLoopGuard {
    fn record_turn(&mut self, calls: &[ToolCall], results: &[String]) -> Option<String> {
        if calls.is_empty() {
            self.reset();
            return None;
        }

        let mut call_signatures = calls
            .iter()
            .map(|call| {
                serde_json::to_string(&Value::Array(vec![
                    Value::String(call.function.name.clone()),
                    canonical_tool_arguments(&call.function.arguments),
                ]))
            })
            .collect::<std::result::Result<Vec<_>, _>>()
            .ok()?;
        call_signatures.sort_unstable();
        let signature = serde_json::to_string(&call_signatures).ok()?;
        if self.last_signature.as_deref() == Some(&signature) {
            self.count = self.count.saturating_add(1);
        } else {
            self.last_signature = Some(signature);
            self.count = 1;
        }
        if self.count != TOOL_CALL_LOOP_THRESHOLD {
            return None;
        }

        let call = calls.first()?;
        let arguments = serde_json::to_string(&canonical_tool_arguments(&call.function.arguments))
            .unwrap_or_else(|_| call.function.arguments.to_string());
        let arguments = summarize_text(&arguments, TOOL_CALL_ARGUMENT_SUMMARY_CHARS);
        let result = summarize_text(
            results.first().map(String::as_str).unwrap_or_default(),
            TOOL_CALL_RESULT_SUMMARY_CHARS,
        );
        let result = if result.is_empty() {
            "(no text result)"
        } else {
            &result
        };
        Some(format!(
            "<system-interrupt reason=\"tool_call_loop_detected\">\n\
             You issued the same batch of {} tool call(s) {} consecutive times. A representative call was `{}` with arguments:\n\
             `{arguments}`\n\n\
             Representative result (truncated): `{result}`\n\n\
             NEVER repeat this tool-call batch again this turn. Use different arguments, choose another tool, or summarize findings and yield if complete.\n\
             </system-interrupt>",
            calls.len(),
            self.count,
            call.function.name
        ))
    }

    fn reset(&mut self) {
        self.last_signature = None;
        self.count = 0;
    }
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
    submission_id: u64,
    cancel: watch::Sender<Option<TurnCancellation>>,
    handle: JoinHandle<()>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PendingMessageKind {
    Queued,
    Steer,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingMessage {
    pub id: u64,
    pub text: String,
    pub kind: PendingMessageKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TurnOutcome {
    Completed,
    Cancelled,
    Failed(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TurnCompletion {
    pub submission_id: u64,
    pub outcome: TurnOutcome,
}

#[derive(Clone)]
struct PendingMessageEntry {
    persistent_id: Option<i64>,
    message: PendingMessage,
    content: Vec<UserContent>,
    visible: bool,
}

struct InputDelivery {
    pending_id: Option<i64>,
    take_initial_steer: bool,
    visible: bool,
}

#[derive(Default)]
struct PendingState {
    accepting: bool,
    messages: VecDeque<PendingMessageEntry>,
}

fn pending_entry(input: PendingInput) -> PendingMessageEntry {
    PendingMessageEntry {
        persistent_id: Some(input.id),
        message: PendingMessage {
            id: u64::try_from(input.id).unwrap_or_default(),
            text: input.text,
            kind: AgentRuntime::pending_message_kind(input.kind),
        },
        content: input.content,
        visible: input.visible,
    }
}

pub struct AgentRuntime {
    backend: RwLock<Option<Arc<dyn ModelBackend>>>,
    model_tools: Arc<ModelToolRegistry>,
    session: Session,
    system_prompt: OnceCell<String>,
    system_prompt_override: RwLock<Option<String>>,
    initializer: Option<Arc<dyn RuntimeInitializer>>,
    limits: RwLock<ModelLimits>,
    context_usage: SyncRwLock<compaction::ContextUsage>,
    context_state: ContextState,
    compaction_settings: RwLock<compaction::Settings>,
    compaction_callback: RwLock<Option<Arc<dyn CompactionCallback>>>,
    max_output_tokens: RwLock<Option<usize>>,
    turn: Mutex<()>,
    active_turn: Mutex<Option<ActiveTurn>>,
    shutting_down: AtomicBool,
    pending: Mutex<PendingState>,
    pending_updates: watch::Sender<Vec<PendingMessage>>,
    turn_completions: broadcast::Sender<TurnCompletion>,
    pending_restored: OnceCell<()>,
    external_pending_enabled: AtomicBool,
}

#[async_trait]
pub trait RuntimeInitializer: Send + Sync {
    async fn initialize(&self) -> Result<String>;

    async fn render_system_prompt(
        &self,
        _spec: &crate::agent::AgentSpec,
    ) -> Result<Option<String>> {
        Ok(None)
    }
}

impl AgentRuntime {
    fn new_with_context(
        backend: Option<Arc<dyn ModelBackend>>,
        model_tools: Arc<ModelToolRegistry>,
        session: Session,
        system_prompt: String,
        limits: ModelLimits,
        context_state: ContextState,
    ) -> Self {
        let (pending_updates, _) = watch::channel(Vec::new());
        let (turn_completions, _) = broadcast::channel(128);
        let system_prompt_cell = OnceCell::new();
        system_prompt_cell
            .set(system_prompt)
            .expect("a new runtime prompt cell is empty");
        Self {
            backend: RwLock::new(backend),
            model_tools,
            session,
            system_prompt: system_prompt_cell,
            system_prompt_override: RwLock::new(None),
            initializer: None,
            limits: RwLock::new(limits),
            context_usage: SyncRwLock::new(compaction::ContextUsage {
                tokens: 0,
                accuracy: compaction::ContextAccuracy::Estimated,
            }),
            context_state,
            compaction_settings: RwLock::new(compaction::Settings::default()),
            compaction_callback: RwLock::new(None),
            max_output_tokens: RwLock::new(None),
            turn: Mutex::new(()),
            active_turn: Mutex::new(None),
            shutting_down: AtomicBool::new(false),
            pending: Mutex::new(PendingState::default()),
            pending_updates,
            turn_completions,
            pending_restored: OnceCell::new(),
            external_pending_enabled: AtomicBool::new(false),
        }
    }

    pub(crate) fn new_deferred_with_context(
        backend: Option<Arc<dyn ModelBackend>>,
        model_tools: Arc<ModelToolRegistry>,
        session: Session,
        initializer: Arc<dyn RuntimeInitializer>,
        limits: ModelLimits,
        context_state: ContextState,
    ) -> Self {
        let mut runtime = Self::new_with_context(
            backend,
            model_tools,
            session,
            String::new(),
            limits,
            context_state,
        );
        runtime.system_prompt = OnceCell::new();
        runtime.initializer = Some(initializer);
        runtime
    }

    async fn system_prompt(&self) -> Result<String> {
        if let Some(prompt) = self.system_prompt_override.read().await.clone() {
            return Ok(prompt);
        }
        self.system_prompt
            .get_or_try_init(|| async {
                self.initializer
                    .as_ref()
                    .ok_or_else(|| anyhow!("runtime startup context is unavailable"))?
                    .initialize()
                    .await
            })
            .await
            .cloned()
    }

    pub async fn prepare_context(&self) -> Result<()> {
        self.system_prompt().await?;
        self.pending_restored
            .get_or_try_init(|| async {
                let restored = self.session.pending_inputs().await?;
                if restored.is_empty() {
                    return Ok::<_, anyhow::Error>(());
                }
                let mut pending = self.pending.lock().await;
                if pending.messages.is_empty() {
                    pending
                        .messages
                        .extend(restored.into_iter().map(pending_entry));
                    self.publish_pending(&pending);
                }
                Ok(())
            })
            .await?;
        Ok(())
    }

    async fn synchronize_pending_inputs(&self) -> Result<(usize, bool)> {
        if !self.session.is_persisted().await {
            return Ok((0, false));
        }
        let active = self.active_turn.lock().await;
        let active_id = active.as_ref().map(|turn| turn.submission_id);
        let mut pending = self.pending.lock().await;
        self.merge_durable_pending_inputs(active_id, &mut pending)
            .await
    }

    async fn merge_durable_pending_inputs(
        &self,
        active_id: Option<u64>,
        pending: &mut PendingState,
    ) -> Result<(usize, bool)> {
        if !self.session.is_persisted().await {
            return Ok((0, false));
        }
        // Freeze local producers before the database snapshot so anything
        // committed after this read has a larger durable ID than this dequeue.
        let stored = self.session.pending_inputs().await?;
        let known = pending
            .messages
            .iter()
            .filter_map(|entry| entry.persistent_id)
            .collect::<HashSet<_>>();
        let additions = stored
            .into_iter()
            .filter(|input| !known.contains(&input.id) && active_id != u64::try_from(input.id).ok())
            .map(pending_entry)
            .collect::<Vec<_>>();
        let added = additions.len();
        let previous_order = pending
            .messages
            .iter()
            .map(|entry| entry.persistent_id)
            .collect::<Vec<_>>();
        pending.messages.extend(additions);
        pending
            .messages
            .make_contiguous()
            .sort_by_key(|entry| entry.persistent_id.unwrap_or(i64::MAX));
        let changed = added > 0
            || !pending
                .messages
                .iter()
                .map(|entry| entry.persistent_id)
                .eq(previous_order);
        if changed {
            self.publish_pending(pending);
        }
        Ok((added, changed))
    }

    pub(crate) async fn reconcile_external_inputs(self: &Arc<Self>) -> Result<usize> {
        self.external_pending_enabled.store(true, Ordering::Release);
        self.prepare_context().await?;
        let (added, changed) = self.synchronize_pending_inputs().await?;
        if changed {
            self.start_accepted_prompt().await?;
        }
        Ok(added)
    }

    pub(crate) async fn try_reconcile_external_inputs(self: &Arc<Self>) -> Result<usize> {
        self.external_pending_enabled.store(true, Ordering::Release);
        let Ok(active) = self.active_turn.try_lock() else {
            return Ok(0);
        };
        self.prepare_context().await?;
        let active_id = active.as_ref().map(|turn| turn.submission_id);
        let mut pending = self.pending.lock().await;
        let (added, changed) = self
            .merge_durable_pending_inputs(active_id, &mut pending)
            .await?;
        drop(pending);
        drop(active);
        if changed {
            self.start_accepted_prompt().await?;
        }
        Ok(added)
    }

    pub fn session(&self) -> &Session {
        &self.session
    }

    /// Tokens the next model request would carry. Provider usage is the
    /// baseline when available; only messages after that response are estimated.
    pub fn estimated_context(&self) -> usize {
        self.context_usage().tokens
    }

    pub fn context_usage(&self) -> compaction::ContextUsage {
        *self
            .context_usage
            .read()
            .expect("context usage lock poisoned")
    }

    /// Context window of the active model. `set_backend` keeps it current
    /// across model switches.
    pub async fn context_window(&self) -> usize {
        self.limits.read().await.context_window
    }

    /// Read the model history once, measure the context the next request
    /// would carry, and refresh the shared usage meter. One history read
    /// keeps the returned history and usage consistent even when another
    /// task appends concurrently.
    async fn measured_history(
        &self,
        system: &str,
        tools: &[ToolDefinition],
    ) -> (Vec<Message>, compaction::ContextUsage) {
        let model = self.session.model_settings().await;
        let (history, usage) = self
            .session
            .with_model_history(&model.provider, &model.model, |history, latest, after| {
                let usage = compaction::context_usage(system, history, tools, latest, after);
                (history.to_vec(), usage)
            })
            .await;
        *self
            .context_usage
            .write()
            .expect("context usage lock poisoned") = usage;
        let context_window = self.limits.read().await.context_window.max(1);
        let base_context_tokens = compaction::estimate_request_tokens(system, &[], tools);
        self.context_state
            .update_meter(context_window, base_context_tokens, usage);
        (history, usage)
    }

    async fn build_model_request(&self) -> Result<ModelRequest> {
        let system = self.system_prompt().await?;
        let tools = self.model_tools.definitions();
        let (history, usage) = self.measured_history(&system, &tools).await;
        Ok(ModelRequest {
            system,
            history,
            tools,
            estimated_context: usage.tokens,
            max_output_tokens: *self.max_output_tokens.read().await,
        })
    }

    pub async fn refresh_context_estimate(&self) {
        let Ok(system) = self.system_prompt().await else {
            return;
        };
        let tools = self.model_tools.definitions();
        self.measured_history(&system, &tools).await;
    }

    pub async fn set_compaction_settings(&self, settings: compaction::Settings) {
        *self.compaction_settings.write().await = settings;
    }

    pub async fn set_compaction_callback(&self, callback: Option<Arc<dyn CompactionCallback>>) {
        *self.compaction_callback.write().await = callback;
    }

    pub async fn set_max_output_tokens(&self, max_output_tokens: Option<usize>) {
        *self.max_output_tokens.write().await = max_output_tokens;
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

    pub async fn turn_running(&self) -> bool {
        self.active_turn
            .lock()
            .await
            .as_ref()
            .is_some_and(|turn| !turn.handle.is_finished())
    }

    pub(crate) fn collaboration_snapshot(&self) -> (bool, usize) {
        let working = self.active_turn.try_lock().map_or(true, |active| {
            active
                .as_ref()
                .is_some_and(|turn| !turn.handle.is_finished())
        });
        let queued = self.pending_updates.borrow().len();
        (working, queued)
    }

    pub fn subscribe_pending_messages(&self) -> watch::Receiver<Vec<PendingMessage>> {
        self.pending_updates.subscribe()
    }

    pub fn subscribe_turn_completions(&self) -> broadcast::Receiver<TurnCompletion> {
        self.turn_completions.subscribe()
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

    /// Map a durable input kind onto its queued-message kind.
    fn pending_message_kind(kind: SubmitKind) -> PendingMessageKind {
        match kind {
            SubmitKind::Prompt => PendingMessageKind::Queued,
            SubmitKind::Steer => PendingMessageKind::Steer,
        }
    }

    /// Persist one accepted input durably and build its in-memory entry.
    async fn persist_pending_entry(
        &self,
        kind: SubmitKind,
        prompt: &str,
        content: Vec<UserContent>,
        visible: bool,
    ) -> Result<PendingMessageEntry> {
        let persistent_id = self
            .session
            .add_pending_input(kind, prompt, &content, visible)
            .await?;
        Ok(PendingMessageEntry {
            persistent_id: Some(persistent_id),
            message: PendingMessage {
                id: u64::try_from(persistent_id).unwrap_or_default(),
                text: prompt.to_string(),
                kind: Self::pending_message_kind(kind),
            },
            content,
            visible,
        })
    }

    /// Take the active-turn slot for a new turn. Returns `true` when a live
    /// turn still owns the slot (it is restored unchanged); otherwise waits
    /// for the finished turn's handle to settle and leaves the slot vacant.
    async fn take_finished_turn(
        active: &mut tokio::sync::MutexGuard<'_, Option<ActiveTurn>>,
    ) -> bool {
        if let Some(previous) = active.take() {
            if previous.handle.is_finished() {
                let _ = previous.handle.await;
            } else {
                **active = Some(previous);
                return true;
            }
        }
        false
    }

    async fn prepared_submission(&self, prompt: String) -> Result<(String, Vec<UserContent>)> {
        if self.shutting_down.load(Ordering::Acquire) {
            bail!("Agent runtime is shutting down")
        }
        let prompt = prompt.trim().to_string();
        if prompt.is_empty() {
            bail!("message is empty")
        }
        self.prepare_context().await?;
        let content = vec![UserContent::text(prompt.as_str())];
        Ok((prompt, content))
    }

    /// Durably accept plugin input. Prompt starts a run when idle; Steer waits
    /// for the next model boundary while active and becomes Prompt when idle.
    pub async fn submit(
        self: &Arc<Self>,
        prompt: String,
        kind: SubmitKind,
    ) -> Result<PendingMessage> {
        let (prompt, content) = self.prepared_submission(prompt).await?;
        let (message, kind) = {
            let mut pending = self.pending.lock().await;
            let kind = match kind {
                SubmitKind::Steer if !pending.accepting => SubmitKind::Prompt,
                kind => kind,
            };
            let entry = self
                .persist_pending_entry(kind, &prompt, content, true)
                .await?;
            let message = entry.message.clone();
            pending.messages.push_back(entry);
            self.publish_pending(&pending);
            (message, kind)
        };
        if kind == SubmitKind::Prompt {
            self.start_accepted_prompt().await?;
        }
        Ok(message)
    }

    /// Durably accept and immediately start one prompt only when no other
    /// native turn or queued input owns the session. Frontends with a
    /// request/response prompt contract use this to keep cancellation and
    /// completion tied to the accepted submission rather than an earlier turn.
    pub async fn submit_exclusive(self: &Arc<Self>, prompt: String) -> Result<PendingMessage> {
        let (prompt, content) = self.prepared_submission(prompt).await?;

        let mut active = self.active_turn.lock().await;
        if self.shutting_down.load(Ordering::Acquire) {
            bail!("Agent runtime is shutting down")
        }
        if Self::take_finished_turn(&mut active).await {
            bail!("the session is busy with another turn")
        }
        let input = {
            let mut pending = self.pending.lock().await;
            if pending.accepting || !pending.messages.is_empty() {
                bail!("the session has pending native input")
            }
            let input = self
                .persist_pending_entry(SubmitKind::Prompt, &prompt, content, true)
                .await?;
            pending.accepting = true;
            self.publish_pending(&pending);
            input
        };
        let message = input.message.clone();
        let (cancel, receiver) = watch::channel(None);
        let runtime = self.clone();
        let handle = tokio::spawn(async move {
            runtime
                .run_active_turn(input, true, true, false, None, receiver)
                .await;
        });
        *active = Some(ActiveTurn {
            submission_id: message.id,
            cancel,
            handle,
        });
        Ok(message)
    }

    /// Continue durable input after reopening an Agent. A lone Steer is
    /// promoted to Prompt because there is no active model boundary to steer.
    pub async fn resume_pending(self: &Arc<Self>) -> Result<bool> {
        self.prepare_context().await?;
        {
            let mut pending = self.pending.lock().await;
            if !pending
                .messages
                .iter()
                .any(|entry| entry.message.kind == PendingMessageKind::Queued)
                && let Some(index) = pending
                    .messages
                    .iter()
                    .position(|entry| entry.message.kind == PendingMessageKind::Steer)
            {
                if let Some(id) = pending.messages[index].persistent_id
                    && !self
                        .session
                        .update_pending_input_kind(id, SubmitKind::Prompt)
                        .await?
                {
                    bail!("cannot promote missing pending Steer input")
                }
                pending.messages[index].message.kind = PendingMessageKind::Queued;
                self.publish_pending(&pending);
            }
        }
        self.start_accepted_prompt().await
    }

    async fn start_accepted_prompt(self: &Arc<Self>) -> Result<bool> {
        let mut active = self.active_turn.lock().await;
        if self.shutting_down.load(Ordering::Acquire) {
            self.pending.lock().await.accepting = false;
            return Ok(false);
        }
        if let Some(previous) = active.take() {
            if !previous.handle.is_finished() && self.pending.lock().await.accepting {
                *active = Some(previous);
                return Ok(false);
            }
            let _ = previous.handle.await;
            if self.shutting_down.load(Ordering::Acquire) {
                self.pending.lock().await.accepting = false;
                return Ok(false);
            }
        }
        let mut pending = self.pending.lock().await;
        if let Err(error) = self.merge_durable_pending_inputs(None, &mut pending).await {
            pending.accepting = false;
            return Err(error);
        }
        let (index, input) = {
            let index = pending
                .messages
                .iter()
                .position(|entry| entry.message.kind == PendingMessageKind::Queued)
                .or_else(|| {
                    pending
                        .messages
                        .iter()
                        .position(|entry| entry.message.kind == PendingMessageKind::Steer)
                });
            let Some(index) = index else {
                pending.accepting = false;
                return Ok(false);
            };
            if pending.messages[index].message.kind == PendingMessageKind::Steer {
                if let Some(id) = pending.messages[index].persistent_id
                    && !self
                        .session
                        .update_pending_input_kind(id, SubmitKind::Prompt)
                        .await?
                {
                    bail!("cannot promote missing pending Steer input")
                }
                pending.messages[index].message.kind = PendingMessageKind::Queued;
            }
            let input = pending
                .messages
                .remove(index)
                .expect("the selected pending Prompt exists");
            pending.accepting = true;
            self.publish_pending(&pending);
            (index, input)
        };
        drop(pending);
        let (cancel, receiver) = watch::channel(None);
        let submission_id = input.message.id;
        let runtime = self.clone();
        let handle = tokio::spawn(async move {
            runtime
                .run_active_turn(input, true, true, true, Some(index), receiver)
                .await;
        });
        *active = Some(ActiveTurn {
            submission_id,
            cancel,
            handle,
        });
        Ok(true)
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

    async fn run_active_turn(
        self: Arc<Self>,
        mut input: PendingMessageEntry,
        mut content_prepared: bool,
        mut take_initial_steer: bool,
        retain_undelivered_input: bool,
        mut pending_index: Option<usize>,
        mut cancel: watch::Receiver<Option<TurnCancellation>>,
    ) {
        loop {
            let mut input_delivered = false;
            let result = self
                .run_turn_with_cancel(
                    input.message.text.clone(),
                    content_prepared.then_some(input.content.clone()),
                    InputDelivery {
                        pending_id: input.persistent_id,
                        take_initial_steer,
                        visible: input.visible,
                    },
                    &mut input_delivered,
                    &mut cancel,
                )
                .await;
            let submission_id = input.message.id;
            if let Err(error) = result {
                let mut cleanup_error = None;
                if content_prepared && !input_delivered && input.visible {
                    if retain_undelivered_input {
                        self.restore_pending_entry(pending_index.unwrap_or_default(), input)
                            .await;
                    } else if let Some(persistent_id) = input.persistent_id
                        && let Err(remove_error) =
                            self.session.remove_pending_input(persistent_id).await
                    {
                        self.restore_pending_entry(pending_index.unwrap_or_default(), input)
                            .await;
                        cleanup_error = Some(format!(
                            "cannot discard failed exclusive input: {remove_error:#}"
                        ));
                    }
                }
                self.stop_accepting_pending().await;
                let outcome = if let Some(cleanup_error) = cleanup_error {
                    TurnOutcome::Failed(format!("{error:#}; {cleanup_error}"))
                } else if matches!(*cancel.borrow(), Some(TurnCancellation::User)) {
                    TurnOutcome::Cancelled
                } else {
                    TurnOutcome::Failed(format!("{error:#}"))
                };
                let _ = self.turn_completions.send(TurnCompletion {
                    submission_id,
                    outcome,
                });
                return;
            }
            let _ = self.turn_completions.send(TurnCompletion {
                submission_id,
                outcome: TurnOutcome::Completed,
            });
            let next = match self.take_next_pending_or_stop().await {
                Ok(next) => next,
                Err(_) => {
                    self.stop_accepting_pending().await;
                    return;
                }
            };
            let Some((index, next)) = next else {
                return;
            };
            input = next;
            pending_index = Some(index);
            content_prepared = true;
            take_initial_steer = false;
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

    /// Request cancellation only when the named submission still owns the
    /// active turn. Frontends use this to avoid interrupting recovered or
    /// native work that happens to run in the same session.
    pub(crate) async fn interrupt_submission(&self, submission_id: u64) -> bool {
        let active = self.active_turn.lock().await;
        let Some(active) = active
            .as_ref()
            .filter(|active| active.submission_id == submission_id && !active.handle.is_finished())
        else {
            return false;
        };
        active.cancel.send(Some(TurnCancellation::User)).is_ok()
    }

    /// Cancel the active request/tool operation, then wait until the worker has
    /// durably recorded its interrupted terminal boundary.
    pub async fn shutdown(&self) {
        self.shutting_down.store(true, Ordering::Release);
        if let Some(active) = self.active_turn.lock().await.take() {
            if active.cancel.borrow().is_none() {
                let _ = active.cancel.send(Some(TurnCancellation::Shutdown));
            }
            let _ = active.handle.await;
        }
        self.pending.lock().await.accepting = false;
    }

    async fn run_turn_with_cancel(
        &self,
        prompt: String,
        prepared_content: Option<Vec<UserContent>>,
        delivery: InputDelivery,
        input_delivered: &mut bool,
        cancel: &mut watch::Receiver<Option<TurnCancellation>>,
    ) -> Result<()> {
        let _turn = self.turn.lock().await;
        let prompt = prompt.trim();
        if prompt.is_empty() {
            return Ok(());
        }
        // Failures before the user turn boundary is persisted must still
        // reach the transcript: frontends track turn state through session
        // events, and an undelivered input is restored to the pending queue
        // without emitting any other event they could observe.
        let started = async {
            self.prepare_context().await?;
            let backend = self
                .backend
                .read()
                .await
                .clone()
                .ok_or_else(|| anyhow!("no credential configured; press :login"))?;
            backend.prepare().await?;
            let content = match prepared_content {
                Some(content) => {
                    if content
                        .iter()
                        .any(|item| matches!(item, UserContent::Image(_)))
                        && !backend.accepts_image_input()
                    {
                        bail!("the active model does not accept image input");
                    }
                    content
                }
                None => vec![UserContent::text(prompt)],
            };
            self.compact_with(Some(backend.as_ref()), false, false, cancel)
                .await?;
            self.append_user_input(
                prompt.to_string(),
                content,
                false,
                delivery.visible,
                delivery.pending_id,
            )
            .await?;
            Ok::<Arc<dyn ModelBackend>, anyhow::Error>(backend)
        };
        let backend = match started.await {
            Ok(backend) => backend,
            Err(error) => {
                let text = format!("{error:#}");
                self.session
                    .append(EventKind::Error { text: text.clone() })
                    .await?;
                return Err(anyhow!(text));
            }
        };
        *input_delivered = true;

        let result = self
            .run_tool_loop(backend, delivery.take_initial_steer, cancel)
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

    async fn append_user_input(
        &self,
        text: String,
        content: Vec<UserContent>,
        finish_previous: bool,
        visible: bool,
        pending_id: Option<i64>,
    ) -> Result<()> {
        let mut events = Vec::with_capacity(3);
        if finish_previous {
            events.push(EventKind::TurnFinished);
        }
        if visible {
            events.push(EventKind::User { text });
        }
        events.push(EventKind::ModelMessage {
            message: Message::User { content },
        });
        if let Some(pending_id) = pending_id {
            self.session
                .append_batch_consuming_pending(pending_id, events)
                .await
                .context("cannot persist pending user turn boundary")?;
        } else {
            self.session
                .append_batch(events)
                .await
                .context("cannot persist user turn boundary")?;
        }
        Ok(())
    }

    async fn append_reconciled_steer(
        &self,
        take_steer: bool,
        finish_previous: bool,
    ) -> Result<bool> {
        let active = self.active_turn.lock().await;
        let active_id = active.as_ref().map(|turn| turn.submission_id);
        let mut pending = self.pending.lock().await;
        if self.external_pending_enabled.load(Ordering::Acquire) {
            self.merge_durable_pending_inputs(active_id, &mut pending)
                .await?;
        }
        if !take_steer {
            return Ok(false);
        }
        let index = pending
            .messages
            .iter()
            .position(|entry| entry.message.kind == PendingMessageKind::Steer);
        let Some(index) = index else {
            return Ok(false);
        };
        let entry = pending
            .messages
            .remove(index)
            .expect("the selected pending Steer exists");
        self.publish_pending(&pending);
        if let Err(error) = self
            .append_user_input(
                entry.message.text.clone(),
                entry.content.clone(),
                finish_previous,
                entry.visible,
                entry.persistent_id,
            )
            .await
        {
            pending.accepting = false;
            pending.messages.insert(index, entry);
            self.publish_pending(&pending);
            return Err(error);
        }
        Ok(true)
    }

    async fn take_next_pending_or_stop(&self) -> Result<Option<(usize, PendingMessageEntry)>> {
        let mut active = self.active_turn.lock().await;
        let active_id = active.as_ref().map(|turn| turn.submission_id);
        let mut pending = self.pending.lock().await;
        self.merge_durable_pending_inputs(active_id, &mut pending)
            .await?;
        let index = pending
            .messages
            .iter()
            .position(|entry| entry.message.kind == PendingMessageKind::Steer)
            .or_else(|| (!pending.messages.is_empty()).then_some(0));
        let Some(index) = index else {
            pending.accepting = false;
            return Ok(None);
        };
        let entry = pending
            .messages
            .remove(index)
            .expect("the selected pending input exists");
        if let Some(active) = active.as_mut() {
            active.submission_id = entry.message.id;
        }
        self.publish_pending(&pending);
        Ok(Some((index, entry)))
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

    async fn run_tool_loop(
        &self,
        backend: Arc<dyn ModelBackend>,
        take_initial_steer: bool,
        cancel: &mut watch::Receiver<Option<TurnCancellation>>,
    ) -> Result<()> {
        let mut overflow_retried = false;
        let mut has_model_response = false;
        let mut steer_ready = false;
        let mut loop_guard = ToolCallLoopGuard::default();
        let provider = self.session.model_settings().await.provider;
        if self
            .append_reconciled_steer(take_initial_steer, false)
            .await?
        {
            steer_ready = true;
        }
        loop {
            self.compact_with(Some(backend.as_ref()), false, false, cancel)
                .await?;
            if !steer_ready {
                self.append_reconciled_steer(true, has_model_response)
                    .await?;
            }
            let (response, force_post_compaction) = self
                .complete_with_recovery(&backend, &provider, &mut overflow_retried, cancel)
                .await?;
            steer_ready = false;
            let assistant_message = Message::Assistant {
                id: None,
                content: response.content.clone(),
            };
            let mut events = self.assistant_events(&response.content);
            if let Some(usage) = self
                .usage_event(response.usage, response.context_tokens, true)
                .await
            {
                events.insert(0, usage);
            }
            events.push(EventKind::ModelMessage {
                message: assistant_message,
            });
            self.session
                .append_batch(events)
                .await
                .context("cannot persist assistant turn boundary")?;
            self.refresh_context_estimate().await;
            has_model_response = true;

            let tool_calls = response
                .content
                .iter()
                .filter_map(|content| match content {
                    AssistantContent::ToolCall(call) => Some(call.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            let mut tool_results = Vec::with_capacity(tool_calls.len());
            let mut cancellation = None;
            for call in tool_calls.iter().cloned() {
                let (result, interrupted) =
                    self.execute_tool(call, backend.as_ref(), cancel).await?;
                cancellation = cancellation.or(interrupted);
                tool_results.push(result);
            }
            if let Some(cancellation) = cancellation.or(*cancel.borrow()) {
                bail!(cancellation.message())
            }
            if let Some(redirect) = loop_guard.record_turn(&tool_calls, &tool_results) {
                self.session
                    .append(EventKind::ModelMessage {
                        message: Message::user(redirect),
                    })
                    .await
                    .context("cannot persist tool-call loop redirect")?;
                self.refresh_context_estimate().await;
            }
            if self.append_reconciled_steer(true, true).await? {
                steer_ready = true;
                continue;
            }
            if tool_calls.is_empty() {
                let compaction = self
                    .compact_with(Some(backend.as_ref()), force_post_compaction, false, cancel)
                    .await;
                if let Err(error) = compaction {
                    self.session
                        .append(EventKind::Notice {
                            text: format!("Automatic context checkpoint failed: {error:#}"),
                        })
                        .await?;
                }
                return Ok(());
            }
        }
    }

    /// Complete one model request, recovering a length-truncated or
    /// overflowed response through one forced compaction and retrying
    /// transient failures. `overflow_retried` carries the one-shot overflow
    /// recovery across this turn's iterations. Returns the response and
    /// whether the following idle boundary must force a post-compaction.
    async fn complete_with_recovery(
        &self,
        backend: &Arc<dyn ModelBackend>,
        provider: &str,
        overflow_retried: &mut bool,
        cancel: &mut watch::Receiver<Option<TurnCancellation>>,
    ) -> Result<(ModelResponse, bool)> {
        let mut model_retries = HashMap::new();
        loop {
            match self.complete_once(backend.as_ref(), cancel).await {
                Ok(response) => {
                    let settings = *self.compaction_settings.read().await;
                    let context_window = self.limits.read().await.context_window.max(1);
                    if settings.enabled
                        && !*overflow_retried
                        && is_recoverable_length(
                            &response,
                            context_window,
                            backend.desired_max_output_tokens(),
                        )
                        && self
                            .compact_with(Some(backend.as_ref()), true, false, cancel)
                            .await?
                    {
                        *overflow_retried = true;
                        self.record_usage(response.usage, response.context_tokens, false)
                            .await?;
                        continue;
                    }
                    let force_post_compaction = settings.enabled
                        && is_successful_context_overflow(&response, context_window);
                    return Ok((response, force_post_compaction));
                }
                Err(error) if !*overflow_retried && is_context_overflow(&error, provider) => {
                    if !self.compaction_settings.read().await.enabled {
                        return Err(error);
                    }
                    *overflow_retried = true;
                    if !self
                        .compact_with(Some(backend.as_ref()), true, false, cancel)
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
        }
    }

    async fn maybe_append_context_reminder(
        &self,
        context_tokens: usize,
        context_window: usize,
        settings: compaction::Settings,
    ) -> Result<bool> {
        if context_tokens == 0 {
            return Ok(false);
        }
        let window_id = self.session.context_window_id().await;
        let sent = self.session.context_hints_sent(window_id).await;
        let Some(hint) = due_context_hint(context_tokens, context_window, settings, sent) else {
            return Ok(false);
        };
        let percent = context_tokens.saturating_mul(100) / context_window.max(1);
        let remaining = context_window.saturating_sub(context_tokens);
        self.session
            .append_batch(vec![
                EventKind::ContextReminder {
                    window_id,
                    hint: Some(hint),
                },
                EventKind::ModelMessage {
                    message: Message::user(context_hint_message(
                        hint, window_id, percent, remaining,
                    )),
                },
            ])
            .await
            .context("cannot persist context hint")?;
        self.refresh_context_estimate().await;
        Ok(true)
    }

    async fn compact_with(
        &self,
        backend: Option<&dyn ModelBackend>,
        force: bool,
        manual: bool,
        cancel: &mut watch::Receiver<Option<TurnCancellation>>,
    ) -> Result<bool> {
        let settings = *self.compaction_settings.read().await;
        if !force && !settings.enabled {
            return Ok(false);
        }
        self.refresh_context_estimate().await;
        let context_usage = self.context_usage();
        if !force && context_usage.accuracy == compaction::ContextAccuracy::Unknown {
            return Ok(false);
        }
        let history = self.session.model_history().await;
        let context_window = self.limits.read().await.context_window.max(1);
        if !force
            && !compaction::should_compact_usage(context_usage.tokens, context_window, settings)
        {
            self.maybe_append_context_reminder(context_usage.tokens, context_window, settings)
                .await?;
            return Ok(false);
        }
        let backend = backend.ok_or_else(|| anyhow!("no credential configured; press :login"))?;
        // Pi permits a single oversized latest turn to be split at a valid
        // message boundary. UAT additionally preserves tool-call/result
        // pairing when selecting that boundary.
        let preparation = compaction::prepare_with_settings(
            &self.system_prompt().await?,
            &history,
            context_window,
            force,
            settings,
        );
        let Some(mut preparation) = preparation else {
            return Ok(false);
        };
        preparation.tokens_before = self.context_usage().tokens;
        let previous_summary = self.session.latest_compaction_summary().await;
        let summary_output_tokens = settings.summary_output_tokens(context_window).max(1);
        let summary_system_tokens =
            compaction::estimate_tokens(compaction::SUMMARY_SYSTEM_PROMPT, &[]);
        let summary_history = compaction::summary_history(
            &preparation,
            previous_summary.as_deref(),
            context_window
                .saturating_sub(summary_output_tokens)
                .saturating_sub(summary_system_tokens),
        );
        let request = ModelRequest {
            system: compaction::SUMMARY_SYSTEM_PROMPT.to_string(),
            estimated_context: compaction::estimate_request_tokens(
                compaction::SUMMARY_SYSTEM_PROMPT,
                &summary_history,
                &[],
            ),
            history: summary_history,
            tools: Vec::new(),
            max_output_tokens: Some(summary_output_tokens),
        };
        let (response, summary) = self.request_summary(backend, request, cancel).await?;
        let updated = self.compaction_spec_update().await?;
        self.record_usage(response.usage, response.context_tokens, false)
            .await?;
        let replacement = compaction::replacement_history(&summary, &preparation.retained);
        self.session
            .append_compaction_with_spec(
                summary,
                preparation.tokens_before,
                replacement,
                manual,
                updated.clone(),
            )
            .await?;
        if let Some((spec, context)) = updated {
            self.model_tools.select(capability_names(&spec.tools))?;
            *self.system_prompt_override.write().await = Some(context.system_prompt);
        }
        self.refresh_context_estimate().await;
        Ok(true)
    }

    /// Run one checkpoint summary request, retrying transient model failures
    /// and empty summaries, and stopping for cancellation. Returns the
    /// response and its non-empty summary text.
    async fn request_summary(
        &self,
        backend: &dyn ModelBackend,
        request: ModelRequest,
        cancel: &mut watch::Receiver<Option<TurnCancellation>>,
    ) -> Result<(ModelResponse, String)> {
        let mut model_retries = HashMap::new();
        loop {
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
                        return Ok((response, summary));
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
        }
    }

    /// Ask the compaction callback for a spec patch and fold it into the
    /// spec and context the compaction event must persist. Returns `None`
    /// when no callback is installed or the callback reports no patch.
    async fn compaction_spec_update(&self) -> Result<Option<(AgentSpec, SessionContext)>> {
        let Some(callback) = self.compaction_callback.read().await.clone() else {
            return Ok(None);
        };
        let current = self.session.spec().await;
        let Some(patch) = callback.compacted().await? else {
            return Ok(None);
        };
        let prompt_update = patch.system_prompt.clone();
        if let Some(CapabilitySelection::Only(names)) = patch.tools.as_ref() {
            self.model_tools.validate_selection(names)?;
        }
        let mut next = current;
        if let Some(update) = patch.system_prompt {
            next.system_prompt = match update {
                SystemPromptUpdate::Append(fragment) => {
                    validate_prompt_update(&fragment)?;
                    match next.system_prompt {
                        crate::agent::SystemPromptSelection::Inherit => {
                            crate::agent::SystemPromptSelection::Append(fragment)
                        }
                        crate::agent::SystemPromptSelection::Append(existing) => {
                            crate::agent::SystemPromptSelection::Append(format!(
                                "{existing}\n\n{fragment}"
                            ))
                        }
                        crate::agent::SystemPromptSelection::Replace(existing) => {
                            crate::agent::SystemPromptSelection::Replace(format!(
                                "{existing}\n\n{fragment}"
                            ))
                        }
                    }
                }
                SystemPromptUpdate::Replace(prompt) => {
                    validate_prompt_update(&prompt)?;
                    crate::agent::SystemPromptSelection::Replace(prompt)
                }
            };
        }
        if let Some(tools) = patch.tools {
            next.tools = tools;
        }
        let next_prompt = if let Some(initializer) = &self.initializer
            && let Some(prompt) = initializer.render_system_prompt(&next).await?
        {
            prompt
        } else {
            let current_prompt = self.system_prompt().await?;
            match prompt_update {
                Some(SystemPromptUpdate::Append(fragment)) => {
                    format!("{current_prompt}\n\n{fragment}")
                }
                Some(SystemPromptUpdate::Replace(prompt)) => prompt,
                None => current_prompt,
            }
        };
        let mut context = self.session.context().await;
        context.system_prompt = next_prompt;
        Ok(Some((next, context)))
    }

    /// Persist one response's token usage, priced with the active model's
    /// catalog rates. A zero-valued report is the sentinel for missing
    /// metrics and carries no information worth an event.
    async fn record_usage(
        &self,
        usage: Option<Usage>,
        context_tokens: Option<usize>,
        context: bool,
    ) -> Result<()> {
        let Some(event) = self.usage_event(usage, context_tokens, context).await else {
            return Ok(());
        };
        self.session.append(event).await?;
        Ok(())
    }

    async fn usage_event(
        &self,
        usage: Option<Usage>,
        context_tokens: Option<usize>,
        context: bool,
    ) -> Option<EventKind> {
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
        let model = self.session.model_settings().await;
        Some(EventKind::Usage {
            input: usage.input_tokens,
            output: usage.output_tokens,
            reasoning: usage.reasoning_tokens,
            cache_read: usage.cached_input_tokens,
            cache_write: usage.cache_creation_input_tokens,
            cost,
            total: context_tokens
                .and_then(|tokens| u64::try_from(tokens).ok())
                .unwrap_or_default(),
            context,
            provider: model.provider,
            model: model.model,
        })
    }

    async fn complete_once(
        &self,
        backend: &dyn ModelBackend,
        cancel: &mut watch::Receiver<Option<TurnCancellation>>,
    ) -> Result<ModelResponse> {
        let request = self.build_model_request().await?;
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
            ModelDelta::ToolCall(text) => EventKind::AssistantToolCallDelta { text },
        });
    }

    async fn execute_tool(
        &self,
        call: ToolCall,
        _backend: &dyn ModelBackend,
        cancel: &mut watch::Receiver<Option<TurnCancellation>>,
    ) -> Result<(String, Option<TurnCancellation>)> {
        let name = call.function.name.clone();
        let call_id = call.id.to_string();
        let (result, cancellation) = if let Some(cancellation) = *cancel.borrow() {
            (Err(anyhow!(cancellation.message())), Some(cancellation))
        } else {
            let dispatch = self.model_tools.dispatch(&name, &call.function.arguments);
            tokio::pin!(dispatch);
            tokio::select! {
                result = &mut dispatch => (result, None),
                changed = cancel.changed() => {
                    if changed.is_ok()
                        && let Some(cancellation) = *cancel.borrow()
                    {
                        (Err(anyhow!(cancellation.message())), Some(cancellation))
                    } else {
                        (dispatch.await, None)
                    }
                }
            }
        };
        let (output, failed) = match result {
            Ok(output) => (output.into_text(), false),
            Err(error) => (truncate_tool_output(format!("Error: {error:#}")), true),
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
                    name,
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
        self.refresh_context_estimate().await;
        Ok((String::new(), cancellation))
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

fn canonical_json(value: &Value) -> Value {
    match value {
        Value::Object(fields) => {
            let mut keys = fields.keys().collect::<Vec<_>>();
            keys.sort_unstable();
            Value::Object(
                keys.into_iter()
                    .map(|key| (key.clone(), canonical_json(&fields[key])))
                    .collect(),
            )
        }
        Value::Array(values) => Value::Array(values.iter().map(canonical_json).collect()),
        _ => value.clone(),
    }
}

fn canonical_tool_arguments(value: &Value) -> Value {
    canonical_json(value)
}

fn summarize_text(text: &str, limit: usize) -> String {
    let summary = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if summary.chars().count() <= limit {
        return summary;
    }
    let mut summary = summary.chars().take(limit).collect::<String>();
    summary.push('…');
    summary
}

fn is_context_overflow(error: &anyhow::Error, provider: &str) -> bool {
    if let Some(failure) = error.downcast_ref::<ModelFailure>() {
        return failure.kind() == ModelFailureKind::ContextOverflow;
    }
    let message = format!("{error:#}");
    looks_like_context_overflow(&message)
        || looks_like_cerebras_bodyless_overflow(&message, provider)
}

fn reported_input_tokens(response: &ModelResponse) -> u64 {
    response.usage.as_ref().map_or(0, |usage| {
        usage
            .input_tokens
            .saturating_add(usage.cached_input_tokens)
            .saturating_add(usage.cache_creation_input_tokens)
    })
}

fn is_successful_context_overflow(response: &ModelResponse, context_window: usize) -> bool {
    matches!(response.finish_reason, None | Some(FinishReason::Stop))
        && reported_input_tokens(response) > context_window as u64
}

fn is_recoverable_length(
    response: &ModelResponse,
    context_window: usize,
    desired_max_output: usize,
) -> bool {
    if !matches!(response.finish_reason, Some(FinishReason::Length)) {
        return false;
    }
    let output = response
        .usage
        .as_ref()
        .map_or(0, |usage| usage.output_tokens as usize);
    (desired_max_output > 0 && output < desired_max_output)
        || (output == 0
            && reported_input_tokens(response) as usize >= context_window.saturating_mul(99) / 100)
}

fn truncate_tool_output(output: String) -> String {
    const LIMIT: usize = 64 * 1024;
    if output.len() <= LIMIT {
        return output;
    }
    let mut head = LIMIT * 3 / 4;
    while head > 0 && !output.is_char_boundary(head) {
        head -= 1;
    }
    let mut tail_start = output.len() - (LIMIT - head);
    while tail_start < output.len() && !output.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    format!(
        "{}\n…[tool output truncated]…\n{}",
        &output[..head],
        &output[tail_start..]
    )
}
