//! Cross-session messaging and presence. A session advertises itself while an
//! `uat` process is active; `steer` turns one text message into durable input
//! for another active session.

use crate::agent::SubmitKind;
use crate::config::display_path;
use crate::runtime::AgentRuntime;
use crate::session::{
    CollaborationOwnershipConflict, CollaborationParticipant, CollaborationStatus, Session,
};
use anyhow::{Result, anyhow, bail};
use rig::message::UserContent;
use std::fmt::Write as _;
use std::sync::{Arc, Weak};
use std::time::Duration;
use tokio::sync::{Mutex, OnceCell};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const POLL_INTERVAL: Duration = Duration::from_millis(500);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(2);
const MAX_MESSAGE_BYTES: usize = 32 * 1024;
const MAX_PREVIEW_CHARS: usize = 240;
const MAX_SUMMARY_CHARS: usize = 200;

#[derive(Clone)]
pub(crate) struct CollaborationState {
    inner: Arc<CollaborationStateInner>,
}

struct CollaborationStateInner {
    session: Session,
    instance_id: String,
    runtime: OnceCell<Weak<AgentRuntime>>,
    cancellation: CancellationToken,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl CollaborationState {
    pub(crate) fn new(session: Session) -> Self {
        Self {
            inner: Arc::new(CollaborationStateInner {
                session,
                instance_id: Uuid::now_v7().simple().to_string(),
                runtime: OnceCell::new(),
                cancellation: CancellationToken::new(),
                worker: Mutex::new(None),
            }),
        }
    }

    pub(crate) async fn start(&self, runtime: &Arc<AgentRuntime>) -> Result<()> {
        self.inner
            .runtime
            .set(Arc::downgrade(runtime))
            .map_err(|_| anyhow!("collaboration runtime is already bound"))?;
        if self.inner.session.is_persisted().await {
            self.refresh_presence(runtime).await?;
        }
        let state = self.clone();
        let worker = tokio::spawn(async move { state.run().await });
        *self.inner.worker.lock().await = Some(worker);
        Ok(())
    }

    async fn run(&self) {
        let mut last_heartbeat = None;
        loop {
            if self.inner.cancellation.is_cancelled() {
                break;
            }
            if self.inner.session.is_persisted().await
                && let Some(runtime) = self.runtime()
            {
                let now = tokio::time::Instant::now();
                if last_heartbeat.is_none_or(|last| now.duration_since(last) >= HEARTBEAT_INTERVAL)
                {
                    if let Err(error) = self.refresh_presence(&runtime).await {
                        if error.is::<CollaborationOwnershipConflict>() {
                            break;
                        }
                        tokio::select! {
                            () = self.inner.cancellation.cancelled() => break,
                            () = tokio::time::sleep(POLL_INTERVAL) => {}
                        }
                        continue;
                    }
                    last_heartbeat = Some(now);
                }
                let _ = runtime.try_reconcile_external_inputs().await;
            }
            tokio::select! {
                () = self.inner.cancellation.cancelled() => break,
                () = tokio::time::sleep(POLL_INTERVAL) => {}
            }
        }
    }

    fn runtime(&self) -> Option<Arc<AgentRuntime>> {
        self.inner.runtime.get().and_then(Weak::upgrade)
    }

    fn runtime_snapshot(&self) -> Result<(Arc<AgentRuntime>, CollaborationStatus, usize)> {
        let runtime = self
            .runtime()
            .ok_or_else(|| anyhow!("collaboration runtime is unavailable"))?;
        let (working, queued) = runtime.collaboration_snapshot();
        let status = if working {
            CollaborationStatus::Working
        } else {
            CollaborationStatus::Idle
        };
        Ok((runtime, status, queued))
    }

    async fn refresh_presence(&self, runtime: &Arc<AgentRuntime>) -> Result<Option<String>> {
        let (working, queued) = runtime.collaboration_snapshot();
        let status = if working {
            CollaborationStatus::Working
        } else {
            CollaborationStatus::Idle
        };
        self.inner
            .session
            .refresh_collaboration_presence(&self.inner.instance_id, status, queued)
            .await
    }

    async fn ensure_presence(&self) -> Result<Option<String>> {
        let (runtime, _, _) = self.runtime_snapshot()?;
        let name = self.refresh_presence(&runtime).await?;
        runtime.reconcile_external_inputs().await?;
        Ok(name)
    }

    async fn set_name(&self, requested: &str) -> Result<String> {
        let (_, status, queued) = self.runtime_snapshot()?;
        self.inner
            .session
            .set_collaboration_name(&self.inner.instance_id, requested, status, queued)
            .await
    }

    pub(crate) async fn shutdown(&self) -> Result<()> {
        self.inner.cancellation.cancel();
        if let Some(worker) = self.inner.worker.lock().await.take() {
            let _ = worker.await;
        }
        self.inner
            .session
            .clear_collaboration_presence(&self.inner.instance_id)
            .await
    }
}

pub(crate) async fn list_participants(
    state: &CollaborationState,
    scope: Option<&str>,
) -> Result<String> {
    let all_projects = parse_scope(scope)?;
    state.ensure_presence().await?;
    let participants = state
        .inner
        .session
        .collaboration_participants(all_projects)
        .await?;
    Ok(format_participants(
        &participants,
        state.inner.session.id(),
        all_projects,
    ))
}

pub(crate) async fn name_session(state: &CollaborationState, name: &str) -> Result<String> {
    let name = state.set_name(name).await?;
    Ok(format!(
        "Collaboration name: {name}\nSession ID: {}",
        state.inner.session.id()
    ))
}

pub(crate) async fn send_tool_message(
    state: &CollaborationState,
    target: &str,
    message: &str,
    delivery: Option<&str>,
    reply: Option<&str>,
    scope: Option<&str>,
    in_reply_to: Option<&str>,
) -> Result<String> {
    let options = SendOptions::parse(delivery, reply, scope, in_reply_to)?;
    send_message(state, target, message, options).await
}

async fn send_message(
    state: &CollaborationState,
    target: &str,
    body: &str,
    options: SendOptions,
) -> Result<String> {
    if body.trim().is_empty() {
        bail!("collaboration message cannot be empty")
    }
    if body.len() > MAX_MESSAGE_BYTES {
        bail!("collaboration message cannot exceed 32 KiB")
    }
    let source_name = state.ensure_presence().await?;
    let target = state
        .inner
        .session
        .collaboration_participant(target, options.all_projects, false)
        .await?
        .ok_or_else(|| anyhow!("active collaboration participant not found: {target}"))?;
    if target.session_id == state.inner.session.id() {
        bail!("cannot send a collaboration message to the current session")
    }
    let target_instance = target
        .instance_id
        .as_deref()
        .ok_or_else(|| anyhow!("collaboration participant is no longer active"))?;
    let message_id = format!("cm_{}", Uuid::now_v7().simple());
    let reply_scope_all = target.cwd != state.inner.session.project_directory();
    let envelope = collaboration_envelope(
        state.inner.session.id(),
        source_name.as_deref(),
        &message_id,
        options.delivery,
        options.reply_requested,
        options.in_reply_to.as_deref(),
        reply_scope_all,
        body,
    );
    let preview = collaboration_preview(source_name.as_deref(), state.inner.session.id(), body);
    let pending_id = state
        .inner
        .session
        .deliver_collaboration_input(
            &target.session_id,
            target_instance,
            options.delivery.submit_kind(),
            &preview,
            &[UserContent::text(envelope)],
        )
        .await?
        .ok_or_else(|| anyhow!("collaboration participant stopped before accepting message"))?;
    Ok(format!(
        "Message accepted.\nmessage_id: {message_id}\ntarget_name: {}\ntarget_session_id: {}\ndelivery: {}\npending_input_id: {pending_id}\nreply_requested: {}",
        target.name.as_deref().unwrap_or("(not set)"),
        target.session_id,
        options.delivery.as_str(),
        options.reply_requested,
    ))
}

#[derive(Clone, Copy)]
enum Delivery {
    Queue,
    Steer,
}

impl Delivery {
    fn as_str(self) -> &'static str {
        match self {
            Self::Queue => "queue",
            Self::Steer => "steer",
        }
    }

    fn submit_kind(self) -> SubmitKind {
        match self {
            Self::Queue => SubmitKind::Prompt,
            Self::Steer => SubmitKind::Steer,
        }
    }
}

struct SendOptions {
    delivery: Delivery,
    reply_requested: bool,
    all_projects: bool,
    in_reply_to: Option<String>,
}

impl SendOptions {
    fn parse(
        delivery: Option<&str>,
        reply: Option<&str>,
        scope: Option<&str>,
        in_reply_to: Option<&str>,
    ) -> Result<Self> {
        let delivery = match delivery {
            None | Some("queue") => Delivery::Queue,
            Some("steer") => Delivery::Steer,
            Some(_) => bail!("delivery must be queue or steer"),
        };
        let reply_requested = match reply {
            None | Some("none") => false,
            Some("requested") => true,
            Some(_) => bail!("reply must be none or requested"),
        };
        let all_projects = parse_scope(scope)?;
        let in_reply_to = match in_reply_to {
            None => None,
            Some(value) => {
                if value.is_empty()
                    || value.len() > 80
                    || !value.chars().all(|character| {
                        character.is_ascii_alphanumeric() || matches!(character, '-' | '_')
                    })
                {
                    bail!("in_reply_to must be a valid collaboration message ID")
                }
                Some(value.to_string())
            }
        };
        Ok(Self {
            delivery,
            reply_requested,
            all_projects,
            in_reply_to,
        })
    }
}

fn parse_scope(scope: Option<&str>) -> Result<bool> {
    match scope {
        None | Some("project") => Ok(false),
        Some("all") => Ok(true),
        Some(_) => bail!("scope must be project or all"),
    }
}

fn write_participant(
    output: &mut String,
    participant: &CollaborationParticipant,
    current_session_id: &str,
    name_line: &str,
    indent: &str,
) {
    let _ = writeln!(
        output,
        "{name_line}{}",
        participant.name.as_deref().unwrap_or("(not set)")
    );
    let _ = writeln!(
        output,
        "{indent}session_id: {}{}",
        participant.session_id,
        if participant.session_id == current_session_id {
            " (current)"
        } else {
            ""
        }
    );
    let _ = writeln!(output, "{indent}status: {}", participant.status.as_str());
    let _ = writeln!(output, "{indent}queued: {}", participant.queued);
    let _ = writeln!(
        output,
        "{indent}model: {}/{}",
        participant.provider, participant.model
    );
    let _ = writeln!(output, "{indent}cwd: {}", display_path(&participant.cwd));
    let _ = writeln!(
        output,
        "{indent}summary: {}",
        bounded_line(&participant.summary, MAX_SUMMARY_CHARS)
    );
    if let Some(last_seen) = participant.last_seen {
        let _ = writeln!(output, "{indent}last_seen: {}", last_seen.to_rfc3339());
    }
}

fn format_participants(
    participants: &[CollaborationParticipant],
    current_session_id: &str,
    all_projects: bool,
) -> String {
    let mut output = format!(
        "Active sessions: {} (scope={})\n",
        participants.len(),
        if all_projects { "all" } else { "project" }
    );
    if participants.is_empty() {
        output.push_str("No active sessions.\n");
        return output;
    }
    for participant in participants {
        write_participant(
            &mut output,
            participant,
            current_session_id,
            "\n- name: ",
            "  ",
        );
    }
    output
}

fn bounded_line(text: &str, max_chars: usize) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.is_empty() {
        return "(no user message yet)".to_string();
    }
    let mut bounded = text.chars().take(max_chars).collect::<String>();
    if text.chars().count() > max_chars {
        bounded.push('…');
    }
    bounded
}

fn collaboration_preview(source_name: Option<&str>, source_id: &str, body: &str) -> String {
    let source = source_name.unwrap_or("unnamed session");
    let prefix = format!("Message from {source} ({source_id}): ");
    let remaining = MAX_PREVIEW_CHARS.saturating_sub(prefix.chars().count());
    format!("{prefix}{}", bounded_line(body, remaining))
}

#[allow(clippy::too_many_arguments)]
fn collaboration_envelope(
    source_id: &str,
    source_name: Option<&str>,
    message_id: &str,
    delivery: Delivery,
    reply_requested: bool,
    in_reply_to: Option<&str>,
    reply_scope_all: bool,
    body: &str,
) -> String {
    let mut output = String::from("<collaboration_message>\n  <source>\n");
    if let Some(name) = source_name {
        let _ = writeln!(output, "    <name>{}</name>", xml_escape(name));
    }
    let _ = writeln!(
        output,
        "    <session_id>{}</session_id>",
        xml_escape(source_id)
    );
    output.push_str("  </source>\n");
    let _ = writeln!(
        output,
        "  <message_id>{}</message_id>",
        xml_escape(message_id)
    );
    let _ = writeln!(output, "  <delivery>{}</delivery>", delivery.as_str());
    if let Some(in_reply_to) = in_reply_to {
        let _ = writeln!(
            output,
            "  <in_reply_to>{}</in_reply_to>",
            xml_escape(in_reply_to)
        );
    }
    if reply_requested {
        let mut arguments = serde_json::Map::new();
        arguments.insert("op".to_string(), serde_json::Value::from("send"));
        arguments.insert("target".to_string(), serde_json::Value::from(source_id));
        arguments.insert(
            "message".to_string(),
            serde_json::Value::from("<your reply>"),
        );
        arguments.insert("delivery".to_string(), serde_json::Value::from("queue"));
        arguments.insert(
            "in_reply_to".to_string(),
            serde_json::Value::from(message_id),
        );
        if reply_scope_all {
            arguments.insert("scope".to_string(), serde_json::Value::from("all"));
        }
        let step = serde_json::json!({"name": "steer", "arguments": arguments});
        output.push_str("  <reply requested=\"true\">\n");
        let _ = writeln!(
            output,
            "    <tool_call>{}</tool_call>",
            xml_escape_text(&step.to_string())
        );
        output.push_str("  </reply>\n");
    }
    let _ = writeln!(
        output,
        "  <peer_content trust=\"untrusted\">{}</peer_content>",
        xml_escape(body)
    );
    output.push_str("</collaboration_message>");
    output
}

fn xml_escape_text(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
