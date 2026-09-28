//! moshi-hook daemon reporting.
//!
//! [Moshi](https://getmoshi.app/) is a mobile terminal app whose `moshi-hook`
//! daemon collects coding-agent lifecycle events over a local Unix socket
//! carrying newline-delimited JSON. When a daemon socket resolves, the
//! reporter forwards the visible session's lifecycle so Moshi can keep its
//! inbox row and push notifications current. The row title follows the
//! interface-generated session title once one exists — the daemon drops
//! category-less frames, so the refresh rides a published session_started
//! frame carrying a borrowed chat.message event name. A settled turn
//! reports Task Done and quotes the final reply, a failed turn the error.
//! Mid-turn
//! progress and tool frames are projected but dropped before sending:
//! moshi-hook never publishes category-less frames and suppresses tool
//! progress, so they would be pure socket noise (the send gate carries a
//! TODO). Session updates carry the remaining context percentage. Session
//! observation, the
//! moshi-hook envelope, and the socket transport stay separate: a daemon
//! quirk belongs in the envelope builder, not in the turn watcher. Reporting
//! is on by default and disabled with `URI_AGENT_MOSHI=0`. Only Unix
//! processes resolve a socket — WSL qualifies as Linux, while native Windows
//! never probes or reports. Session switches
//! move reporting to the newly visible session and rebind its terminal pane;
//! shutdown closes the reported row. Without a listening daemon the reporter
//! stays completely inactive, and reporting failures never affect the
//! conversation.

use crate::agent::AgentSpec;
use crate::runtime::AgentRuntime;
use crate::session::{EventKind, Session, SessionUpdate};
use chrono::{DateTime, SecondsFormat, Utc};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, broadcast, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Wire identity on the Moshi event API. That API rejects `uri-agent` and
/// only accepts a fixed agent list. Pi is the short allowlisted identity:
/// the daemon handles it as a first-class source family with session-title
/// tracking, and the two-letter name keeps the Live Activity headline
/// short. `codex` is not used, because the daemon drops a `Stop` event
/// from that source.
const SOURCE: &str = "pi";
/// Tool and streaming activity fire many times per turn; each channel
/// throttles to one frame per window, keeping the inbox row fresh without
/// flooding the daemon, which applies its own five-second push throttle.
const PROGRESS_THROTTLE: Duration = Duration::from_secs(5);
/// One socket exchange must never stall the conversation.
const SEND_TIMEOUT: Duration = Duration::from_secs(2);
/// The daemon answers fire-and-forget frames with an `ack`; drain it so the
/// daemon never writes to a closed peer, but do not wait long for it.
const ACK_TIMEOUT: Duration = Duration::from_millis(300);
/// A failing socket is retried after this pause instead of on every event.
const FAILURE_BACKOFF: Duration = Duration::from_secs(30);
/// Moshi renders at most 80 characters of an event title and 200 of its body;
/// stay inside both bounds so nothing is cut off on the server.
const TITLE_MAX: usize = 80;
/// Moshi's notification body bound. Prompts, rolling output tails, and the
/// final reply excerpt go here.
const MESSAGE_MAX: usize = 200;
/// Lifecycle names from the documented moshi-hook envelope. They are not a
/// source identity: `Stop` is only special-cased for `source: "codex"`.
const EVENT_SESSION_START: &str = "SessionStart";
const EVENT_USER_PROMPT: &str = "UserPromptSubmit";
const EVENT_STOP: &str = "Stop";
const EVENT_SESSION_END: &str = "SessionEnd";
const EVENT_PRE_TOOL: &str = "PreToolUse";
const EVENT_POST_TOOL: &str = "PostToolUse";
/// Borrowed message-event name for the published title refresh. A
/// non-empty eventName keeps the title URI Agent sends, and the daemon
/// publishes it under the session_started category without treating the
/// frame as a fresh session start.
const EVENT_CHAT_MESSAGE: &str = "chat.message";

/// The multiplexer pane the process runs in, when it advertises one. Moshi
/// uses the pane to attribute the session to the right terminal.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
enum Endpoint {
    #[default]
    None,
    Tmux {
        pane: String,
        /// Session and window names resolve lazily from tmux itself; the
        /// socket path comes from `TMUX`.
        session: Option<String>,
        window: Option<String>,
        socket: Option<String>,
    },
    Zellij {
        session: Option<String>,
        pane: Option<String>,
    },
    Herdr {
        pane: String,
        session: Option<String>,
        workspace_id: Option<String>,
        tab_id: Option<String>,
    },
}

impl Endpoint {
    fn from_lookup(lookup: &impl Fn(&str) -> Option<String>) -> Self {
        fn present(value: Option<String>) -> Option<String> {
            value.filter(|value| !value.is_empty())
        }
        if lookup("HERDR_ENV").as_deref() == Some("1")
            && let Some(pane) = present(lookup("HERDR_PANE_ID"))
        {
            return Self::Herdr {
                pane,
                session: present(lookup("HERDR_SESSION")),
                workspace_id: present(lookup("HERDR_WORKSPACE_ID")),
                tab_id: present(lookup("HERDR_TAB_ID")),
            };
        }
        if present(lookup("ZELLIJ")).is_some() {
            return Self::Zellij {
                session: present(lookup("ZELLIJ_SESSION_NAME")),
                pane: present(lookup("ZELLIJ_PANE_ID")),
            };
        }
        if let Some(pane) = present(lookup("TMUX_PANE")) {
            let socket = lookup("TMUX")
                .and_then(|value| value.split(',').next().map(str::to_string))
                .filter(|value| !value.is_empty());
            return Self::Tmux {
                pane,
                session: None,
                window: None,
                socket,
            };
        }
        Self::None
    }

    fn has_pane(&self) -> bool {
        !matches!(self, Self::None)
    }

    fn fields(&self) -> TerminalFields {
        match self {
            Self::None => TerminalFields::default(),
            Self::Tmux {
                pane,
                session,
                window,
                socket,
            } => TerminalFields {
                kind: Some("tmux"),
                tmux_pane: Some(pane.clone()),
                tmux_session: session.clone(),
                tmux_window: window.clone(),
                tmux_socket: socket.clone(),
                ..TerminalFields::default()
            },
            Self::Zellij { session, pane } => TerminalFields {
                kind: Some("zellij"),
                zellij_session: session.clone(),
                zellij_pane: pane.clone(),
                ..TerminalFields::default()
            },
            Self::Herdr {
                pane,
                session,
                workspace_id,
                tab_id,
            } => TerminalFields {
                kind: Some("herdr"),
                herdr_pane: Some(pane.clone()),
                herdr_session: session.clone(),
                herdr_workspace_id: workspace_id.clone(),
                herdr_tab_id: tab_id.clone(),
                ..TerminalFields::default()
            },
        }
    }
}

/// Terminal identity copied onto every frame. Empty when the process is not
/// inside a multiplexer pane.
#[derive(Clone, Debug, Default)]
struct TerminalFields {
    kind: Option<&'static str>,
    tmux_pane: Option<String>,
    tmux_session: Option<String>,
    tmux_window: Option<String>,
    tmux_socket: Option<String>,
    zellij_session: Option<String>,
    zellij_pane: Option<String>,
    herdr_pane: Option<String>,
    herdr_session: Option<String>,
    herdr_workspace_id: Option<String>,
    herdr_tab_id: Option<String>,
}

/// Static per-session fields every frame carries.
#[derive(Clone, Debug, PartialEq, Eq)]
struct FrameContext {
    session_id: String,
    cwd: String,
    project: String,
    model: String,
}

impl FrameContext {
    fn new(session_id: &str, spec: &AgentSpec) -> Self {
        let cwd = spec.working_directory.to_string_lossy().into_owned();
        let project = spec
            .working_directory
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| cwd.clone());
        Self {
            session_id: session_id.to_string(),
            cwd,
            project,
            model: spec.model.clone(),
        }
    }
}

/// What the watcher asks the envelope builder to send. This is still URI
/// Agent's view of a lifecycle fact; field names live only in `frame_for`.
#[derive(Clone, Debug, PartialEq, Eq)]
enum MoshiAction {
    /// Attach the terminal pane to the session; never published as a push.
    Bind,
    /// Category-less attach. The daemon treats it as state, not an inbox push.
    State {
        title: Option<String>,
    },
    /// The interface generated a session title after the row opened. A
    /// category-less refresh would be dropped by the daemon, so the new
    /// title rides a published session_started frame instead.
    TitleUpdated {
        title: String,
        /// Keeps the published body non-empty; falls back to the title.
        prompt: Option<String>,
    },
    SessionStarted {
        prompt: Option<String>,
    },
    /// A later prompt. Refreshes the open row without announcing a new session.
    PromptSubmitted {
        prompt: String,
    },
    /// Live turn status: the subtitle is the status label and the body the
    /// rolling output tail. Category-less, so the daemon never pushes it.
    Progress {
        status: Status,
        detail: Option<String>,
    },
    TaskComplete {
        failed: bool,
        snippet: Option<String>,
        prompt: Option<String>,
        error: Option<String>,
    },
    ToolRunning {
        tool: String,
        /// The call's arguments, when non-empty, so the body can show what
        /// is running.
        detail: Option<String>,
    },
    ToolFinished {
        tool: String,
    },
    SessionClosed,
}

/// Facts observed from the session, before any moshi-hook category is chosen.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Notice {
    Prompt,
    /// Streaming output or a retry moved the live status.
    Progress(Status),
    Finished,
    ToolStarted(String, Option<String>),
    ToolFinished(String),
}

/// What the turn is doing right now, mirroring the interface's activity
/// line. The watcher tracks only the kind; the labels ride the envelope.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Status {
    Reasoning,
    Writing,
    Retrying {
        attempt: usize,
        max_retries: usize,
        reason: String,
    },
}

impl Status {
    fn label(&self) -> String {
        match self {
            Self::Reasoning => "Reasoning".to_string(),
            Self::Writing => "Writing".to_string(),
            Self::Retrying {
                attempt,
                max_retries,
                ..
            } => format!("Retrying {attempt}/{max_retries}"),
        }
    }
}

/// Per-turn reporting state. The prompt opens and refreshes the row; while
/// the turn runs the body follows the rolling activity tail; a settled turn
/// quotes the final reply block and an error becomes the body, so the phone
/// shows how the turn ended.
#[derive(Default)]
struct TurnState {
    prompt: Option<String>,
    /// Rolling tail of the streaming text or reasoning, capped at the body
    /// bound. A tool call clears it, so the call's detail owns the body
    /// until output resumes.
    activity: String,
    /// Opening of the latest completed assistant text block, at the body
    /// bound. A settled turn quotes it as the final reply.
    snippet: Option<String>,
    error: Option<String>,
    failed: bool,
}

/// Streaming deltas arrive as fragments, so the transient/persisted split
/// drives the preview: a delta extends the rolling tail, while a persisted
/// block restarts it.
fn observe(kind: &EventKind, persisted: bool, turn: &mut TurnState) -> Option<Notice> {
    match kind {
        EventKind::User { text } => {
            let prompt = truncate(text, MESSAGE_MAX);
            if prompt.is_empty() {
                return None;
            }
            turn.prompt = Some(prompt);
            turn.activity.clear();
            turn.snippet = None;
            turn.error = None;
            turn.failed = false;
            Some(Notice::Prompt)
        }
        EventKind::AssistantText { text } => {
            if persisted {
                turn.activity = tail(text, MESSAGE_MAX);
                let excerpt = truncate(text, MESSAGE_MAX);
                if !excerpt.is_empty() {
                    turn.snippet = Some(excerpt);
                }
            } else {
                push_tail(&mut turn.activity, text, MESSAGE_MAX);
            }
            Some(Notice::Progress(Status::Writing))
        }
        EventKind::AssistantReasoning { text } => {
            if persisted {
                turn.activity = tail(text, MESSAGE_MAX);
            } else {
                push_tail(&mut turn.activity, text, MESSAGE_MAX);
            }
            Some(Notice::Progress(Status::Reasoning))
        }
        EventKind::ModelRetry {
            attempt,
            max_retries,
            reason,
            ..
        } => Some(Notice::Progress(Status::Retrying {
            attempt: *attempt,
            max_retries: *max_retries,
            reason: truncate(reason, MESSAGE_MAX),
        })),
        EventKind::Error { text } => {
            turn.failed = true;
            let error = truncate(text, MESSAGE_MAX);
            if !error.is_empty() {
                turn.error = Some(error);
            }
            None
        }
        EventKind::TurnFinished => Some(Notice::Finished),
        EventKind::ToolCall {
            name, arguments, ..
        } => {
            turn.activity.clear();
            // An argument-less call leaves the body to the previous frame.
            let detail = match arguments {
                serde_json::Value::Null => None,
                serde_json::Value::Object(map) if map.is_empty() => None,
                other => {
                    let detail = truncate(&other.to_string(), MESSAGE_MAX);
                    (!detail.is_empty()).then_some(detail)
                }
            };
            Some(Notice::ToolStarted(name.clone(), detail))
        }
        EventKind::ToolResult { name, .. } => Some(Notice::ToolFinished(name.clone())),
        _ => None,
    }
}

/// Turn a session notice into zero or more reports. The first real activity
/// announces the session. Later prompts refresh the open row; they do not
/// announce again.
fn project(notice: Notice, turn: &TurnState, announced: &mut bool) -> Vec<MoshiAction> {
    let mut actions = Vec::new();
    let first = !*announced
        && matches!(
            notice,
            Notice::Prompt
                | Notice::Progress(_)
                | Notice::Finished
                | Notice::ToolStarted(..)
                | Notice::ToolFinished(_)
        );
    if first {
        *announced = true;
        actions.push(MoshiAction::SessionStarted {
            prompt: turn.prompt.clone(),
        });
    }
    match notice {
        Notice::Prompt => {
            if !first && let Some(prompt) = turn.prompt.clone() {
                actions.push(MoshiAction::PromptSubmitted { prompt });
            }
        }
        Notice::Progress(status) => {
            let detail = match &status {
                Status::Retrying { reason, .. } => (!reason.is_empty()).then(|| reason.clone()),
                _ => (!turn.activity.is_empty()).then(|| turn.activity.clone()),
            };
            actions.push(MoshiAction::Progress { status, detail });
        }
        Notice::Finished => actions.push(MoshiAction::TaskComplete {
            failed: turn.failed,
            snippet: turn.snippet.clone(),
            prompt: turn.prompt.clone(),
            error: turn.error.clone(),
        }),
        Notice::ToolStarted(tool, detail) => {
            actions.push(MoshiAction::ToolRunning { tool, detail })
        }
        Notice::ToolFinished(tool) => actions.push(MoshiAction::ToolFinished { tool }),
    }
    actions
}

/// One newline-delimited JSON frame on the daemon socket. Optional fields
/// follow the moshi-hook envelope; unknown fields are ignored by the daemon.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct MoshiFrame {
    #[serde(rename = "type")]
    kind: &'static str,
    source: &'static str,
    #[serde(rename = "sessionId")]
    session_id: String,
    #[serde(rename = "requestedAt")]
    requested_at: String,
    #[serde(rename = "eventName", skip_serializing_if = "Option::is_none")]
    event_name: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    category: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    subtitle: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
    #[serde(rename = "toolName", skip_serializing_if = "Option::is_none")]
    tool_name: Option<String>,
    #[serde(rename = "modelName", skip_serializing_if = "Option::is_none")]
    model_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cwd: Option<String>,
    #[serde(rename = "projectName", skip_serializing_if = "Option::is_none")]
    project_name: Option<String>,
    #[serde(rename = "agentPid", skip_serializing_if = "Option::is_none")]
    agent_pid: Option<u32>,
    #[serde(rename = "contextRemaining", skip_serializing_if = "Option::is_none")]
    context_remaining: Option<u32>,
    #[serde(rename = "terminalKind", skip_serializing_if = "Option::is_none")]
    terminal_kind: Option<&'static str>,
    #[serde(rename = "tmuxPane", skip_serializing_if = "Option::is_none")]
    tmux_pane: Option<String>,
    #[serde(rename = "tmuxSession", skip_serializing_if = "Option::is_none")]
    tmux_session: Option<String>,
    #[serde(rename = "tmuxWindow", skip_serializing_if = "Option::is_none")]
    tmux_window: Option<String>,
    #[serde(rename = "tmuxSocket", skip_serializing_if = "Option::is_none")]
    tmux_socket: Option<String>,
    #[serde(rename = "zellijSession", skip_serializing_if = "Option::is_none")]
    zellij_session: Option<String>,
    #[serde(rename = "zellijPane", skip_serializing_if = "Option::is_none")]
    zellij_pane: Option<String>,
    #[serde(rename = "herdrPane", skip_serializing_if = "Option::is_none")]
    herdr_pane: Option<String>,
    #[serde(rename = "herdrSession", skip_serializing_if = "Option::is_none")]
    herdr_session: Option<String>,
    #[serde(rename = "herdrWorkspaceId", skip_serializing_if = "Option::is_none")]
    herdr_workspace_id: Option<String>,
    #[serde(rename = "herdrTabId", skip_serializing_if = "Option::is_none")]
    herdr_tab_id: Option<String>,
}

/// Wire fields that depend on the lifecycle fact. Terminal and session
/// fields are filled around this, so the adapter can change without the
/// watcher learning moshi-hook names. The generated session title owns the
/// headline once it exists; the subtitle carries the live status label.
struct WireParts {
    kind: &'static str,
    event_name: Option<&'static str>,
    category: Option<&'static str>,
    title: Option<String>,
    subtitle: Option<String>,
    message: Option<String>,
    tool_name: Option<String>,
    agent_pid: Option<u32>,
}

fn wire_parts(action: &MoshiAction, session_title: Option<&str>) -> WireParts {
    let session_title = session_title.filter(|title| !title.is_empty());
    match action {
        MoshiAction::Bind => WireParts {
            kind: "session.bind",
            event_name: None,
            category: None,
            title: None,
            subtitle: None,
            message: None,
            tool_name: None,
            agent_pid: Some(std::process::id()),
        },
        MoshiAction::State { title } => WireParts {
            kind: "session.update",
            event_name: Some(EVENT_SESSION_START),
            category: None,
            title: title.clone(),
            subtitle: None,
            message: None,
            tool_name: None,
            agent_pid: None,
        },
        MoshiAction::TitleUpdated { title, prompt } => WireParts {
            kind: "session.update",
            event_name: Some(EVENT_CHAT_MESSAGE),
            category: Some("session_started"),
            title: Some(title.clone()),
            // No subtitle: the refresh says nothing about the turn, so the
            // status from the previous frame stands.
            subtitle: None,
            // The frame is published, so the body stays non-empty: the
            // prompt, or the title itself before any prompt.
            message: Some(prompt.clone().unwrap_or_else(|| title.clone())),
            tool_name: None,
            agent_pid: None,
        },
        MoshiAction::SessionStarted { prompt } => WireParts {
            kind: "session.update",
            event_name: Some(EVENT_SESSION_START),
            category: Some("session_started"),
            title: Some(session_title.unwrap_or("Session started").to_string()),
            subtitle: Some("Thinking".to_string()),
            message: prompt.clone(),
            tool_name: None,
            agent_pid: None,
        },
        MoshiAction::PromptSubmitted { prompt } => WireParts {
            kind: "session.update",
            event_name: Some(EVENT_USER_PROMPT),
            category: Some("session_started"),
            title: Some(session_title.unwrap_or("Working").to_string()),
            subtitle: Some("Thinking".to_string()),
            message: Some(prompt.clone()),
            tool_name: None,
            agent_pid: None,
        },
        MoshiAction::Progress { status, detail } => WireParts {
            kind: "session.update",
            event_name: Some(EVENT_SESSION_START),
            category: None,
            title: None,
            subtitle: Some(status.label()),
            message: detail.clone(),
            tool_name: None,
            agent_pid: None,
        },
        MoshiAction::TaskComplete {
            failed,
            snippet,
            prompt,
            error,
        } => {
            let status = if *failed { "Turn failed" } else { "Task Done" };
            let fallback = if *failed {
                "Turn failed".to_string()
            } else {
                snippet
                    .as_deref()
                    .map(|snippet| truncate(snippet, TITLE_MAX))
                    .filter(|snippet| !snippet.is_empty())
                    .unwrap_or_else(|| "Turn complete".to_string())
            };
            let title = session_title.map(str::to_string).unwrap_or(fallback);
            // The subtitle is the bare status. The body leads with the same
            // status — a push may render only title and body — then quotes
            // the final reply, or the error when the turn failed; a missing
            // reply still leaves the prompt, in case a daemon replaces the
            // title.
            let body = if *failed {
                error
                    .clone()
                    .or_else(|| prompt.clone())
                    .or_else(|| snippet.clone())
            } else {
                snippet.clone().or_else(|| prompt.clone())
            };
            WireParts {
                kind: "session.update",
                event_name: Some(EVENT_STOP),
                category: Some("task_complete"),
                subtitle: (title != status).then(|| status.to_string()),
                title: Some(title),
                message: Some(match body {
                    Some(body) if !body.is_empty() => {
                        truncate(&format!("{status}\n{body}"), MESSAGE_MAX)
                    }
                    _ => status.to_string(),
                }),
                tool_name: None,
                agent_pid: None,
            }
        }
        // Tool frames carry no title: the daemon does not push tool progress,
        // and a title here would clobber the session title on the inbox row.
        // The status rides the subtitle instead.
        MoshiAction::ToolRunning { tool, detail } => WireParts {
            kind: "session.update",
            event_name: Some(EVENT_PRE_TOOL),
            category: Some("tool_running"),
            title: None,
            subtitle: Some(format!("Running {tool}")),
            message: detail.clone(),
            tool_name: Some(tool.clone()),
            agent_pid: None,
        },
        MoshiAction::ToolFinished { tool } => WireParts {
            kind: "session.update",
            event_name: Some(EVENT_POST_TOOL),
            category: Some("tool_finished"),
            title: None,
            subtitle: Some("Thinking".to_string()),
            // No message: the body keeps the finished call's detail.
            message: None,
            tool_name: Some(tool.clone()),
            agent_pid: None,
        },
        MoshiAction::SessionClosed => WireParts {
            kind: "session.closed",
            event_name: Some(EVENT_SESSION_END),
            // An empty category is a silent state carrier and is not published.
            category: Some("session_ended"),
            title: Some(session_title.unwrap_or("Session ended").to_string()),
            subtitle: None,
            message: None,
            tool_name: None,
            agent_pid: None,
        },
    }
}

/// Facts known only while the worker runs: the generated session title and
/// the remaining context percentage. One-shot frame builders leave them out.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct FrameFacts {
    session_title: Option<String>,
    context_remaining: Option<u32>,
}

fn frame_for(
    action: &MoshiAction,
    context: &FrameContext,
    endpoint: &Endpoint,
    facts: &FrameFacts,
    now: DateTime<Utc>,
) -> MoshiFrame {
    let parts = wire_parts(action, facts.session_title.as_deref());
    let terminal = endpoint.fields();
    MoshiFrame {
        kind: parts.kind,
        source: SOURCE,
        session_id: context.session_id.clone(),
        requested_at: now.to_rfc3339_opts(SecondsFormat::Secs, true),
        event_name: parts.event_name,
        category: parts.category,
        title: parts.title,
        subtitle: parts.subtitle,
        message: parts.message,
        tool_name: parts.tool_name,
        model_name: (!context.model.is_empty()).then(|| context.model.clone()),
        cwd: Some(context.cwd.clone()),
        project_name: Some(context.project.clone()),
        agent_pid: parts.agent_pid,
        // Context fill rides session updates only, mirroring moshi-hook's own
        // hooks; bind and close frames stay without it.
        context_remaining: facts
            .context_remaining
            .filter(|_| parts.kind == "session.update"),
        terminal_kind: terminal.kind,
        tmux_pane: terminal.tmux_pane,
        tmux_session: terminal.tmux_session,
        tmux_window: terminal.tmux_window,
        tmux_socket: terminal.tmux_socket,
        zellij_session: terminal.zellij_session,
        zellij_pane: terminal.zellij_pane,
        herdr_pane: terminal.herdr_pane,
        herdr_session: terminal.herdr_session,
        herdr_workspace_id: terminal.herdr_workspace_id,
        herdr_tab_id: terminal.herdr_tab_id,
    }
}

fn truncate(text: &str, max: usize) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= max {
        return trimmed.to_string();
    }
    trimmed.chars().take(max).collect()
}

/// The last `max` characters of `text`: a live body shows the fragment
/// being written, not the opening of a long stream.
fn tail(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.to_string();
    }
    text.chars().skip(count - max).collect()
}

/// Append a streaming fragment, keeping only the tail so the buffer stays
/// bounded no matter how long the turn runs.
fn push_tail(buffer: &mut String, text: &str, max: usize) {
    buffer.push_str(text);
    let count = buffer.chars().count();
    if count > max {
        *buffer = buffer.chars().skip(count - max).collect();
    }
}

/// Remaining context as a percentage, mirroring moshi-hook's own hooks: the
/// used share floors, a full window still reports 1, and unknown input
/// reports nothing.
fn remaining_percent(used: usize, window: usize) -> Option<u32> {
    if used == 0 || window == 0 {
        return None;
    }
    let used_pct = (used.saturating_mul(100) / window).min(100) as u32;
    Some((100 - used_pct).max(1))
}

/// The watched session's remaining context percentage, when both sides of
/// the ratio are known.
async fn context_remaining(runtime: &AgentRuntime) -> Option<u32> {
    remaining_percent(
        runtime.context_usage().tokens,
        runtime.context_window().await,
    )
}

/// Ask tmux for the session and window names of a pane, mirroring
/// moshi-hook's own hooks. A missing tmux binary or a stale pane resolves
/// to nothing and the frames simply omit the names.
#[cfg(unix)]
async fn tmux_names(pane: &str) -> Option<(String, String)> {
    let output = tokio::time::timeout(
        Duration::from_millis(500),
        tokio::process::Command::new("tmux")
            .args(["display-message", "-p", "-t", pane, "#S\t#I"])
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut parts = text.trim().split('\t');
    let session = parts.next()?.trim();
    if session.is_empty() {
        return None;
    }
    let window = parts.next().unwrap_or("").trim().to_string();
    Some((session.to_string(), window))
}

#[cfg(not(unix))]
async fn tmux_names(_pane: &str) -> Option<(String, String)> {
    None
}

/// Fill in the tmux session and window names once they resolve. Retried on
/// each attach until the pane answers; skipped while no daemon listens.
async fn resolve_tmux_names(inner: &MoshiInner) {
    if !inner.socket_path.exists() {
        return;
    }
    let pane = {
        let endpoint = inner.endpoint.read().expect("endpoint lock poisoned");
        match &*endpoint {
            Endpoint::Tmux {
                pane,
                session: None,
                ..
            } => pane.clone(),
            _ => return,
        }
    };
    let Some((session, window)) = tmux_names(&pane).await else {
        return;
    };
    let mut endpoint = inner.endpoint.write().expect("endpoint lock poisoned");
    if let Endpoint::Tmux {
        session: session_slot,
        window: window_slot,
        ..
    } = &mut *endpoint
    {
        *session_slot = Some(session);
        *window_slot = (!window.is_empty()).then_some(window);
    }
}

#[cfg(unix)]
fn socket_path(lookup: &impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    if let Some(path) = lookup("MOSHI_SOCKET_PATH").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(path));
    }
    default_socket_path(lookup)
}

/// moshi-hook keeps its socket inside the state directory on macOS.
#[cfg(all(unix, target_os = "macos"))]
fn default_socket_path(lookup: &impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    let state = lookup("MOSHI_STATE_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            lookup("HOME")
                .filter(|value| !value.is_empty())
                .map(|home| PathBuf::from(home).join("Library/Application Support/Moshi"))
        })?;
    Some(state.join("moshi-hook.sock"))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn default_socket_path(lookup: &impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    let runtime = lookup("XDG_RUNTIME_DIR").filter(|value| !value.is_empty())?;
    Some(PathBuf::from(runtime).join("moshi-hook.sock"))
}

/// moshi-hook's native Windows transport is a named pipe, which this reporter
/// does not implement; WSL sessions report through the Unix socket instead.
#[cfg(not(unix))]
fn socket_path(_lookup: &impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    None
}

#[cfg(unix)]
async fn send_line(path: &Path, line: &[u8]) -> std::io::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::UnixStream::connect(path).await?;
    stream.write_all(line).await?;
    // The line is delivered. Half-close so the daemon can finish its one
    // exchange, but a peer that already closed — or an ack that never arrives —
    // must not look like a failed send and swallow the next frames.
    let _ = stream.shutdown().await;
    let mut buffer = [0_u8; 256];
    let _ = tokio::time::timeout(ACK_TIMEOUT, stream.read(&mut buffer)).await;
    Ok(())
}

#[cfg(not(unix))]
async fn send_line(_path: &Path, _line: &[u8]) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "moshi-hook reporting requires a Unix socket",
    ))
}

/// The session currently shown in the terminal plus the frame fields captured
/// when reporting moved to it.
#[derive(Clone)]
struct WatchedSession {
    session: Session,
    runtime: Arc<AgentRuntime>,
    title: watch::Receiver<String>,
    context: FrameContext,
}

struct MoshiInner {
    socket_path: PathBuf,
    endpoint: RwLock<Endpoint>,
    target: watch::Sender<Option<WatchedSession>>,
    cancellation: CancellationToken,
    worker: Mutex<Option<JoinHandle<()>>>,
}

/// Reports the TUI's visible session lifecycle to a moshi-hook daemon.
///
/// One reporter serves the whole TUI process. Session switches swap the
/// watched session, and the reported session ID follows the visible
/// conversation. Reporting only writes newline-delimited JSON to the daemon
/// socket and drops failures, so a missing or busy daemon cannot affect the
/// conversation.
pub struct MoshiReporter {
    inner: Arc<MoshiInner>,
}

impl MoshiReporter {
    /// Create a reporter for the current process, or `None` when reporting is
    /// disabled with `URI_AGENT_MOSHI=0` or no daemon socket path resolves.
    pub fn from_env() -> Option<Self> {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Option<Self> {
        if lookup("URI_AGENT_MOSHI").as_deref() == Some("0") {
            return None;
        }
        let socket_path = socket_path(&lookup)?;
        Some(Self::from_parts(
            socket_path,
            Endpoint::from_lookup(&lookup),
        ))
    }

    fn from_parts(socket_path: PathBuf, endpoint: Endpoint) -> Self {
        Self {
            inner: Arc::new(MoshiInner {
                socket_path,
                endpoint: RwLock::new(endpoint),
                target: watch::channel(None).0,
                cancellation: CancellationToken::new(),
                worker: Mutex::new(None),
            }),
        }
    }

    /// Report the session now shown in the terminal. Repeated calls move
    /// reporting to the newly visible session and its terminal-title receiver
    /// without restarting the worker.
    pub async fn start(&self, runtime: Arc<AgentRuntime>, title: watch::Receiver<String>) {
        let session = runtime.session().clone();
        let spec = session.spec().await;
        let watched = WatchedSession {
            context: FrameContext::new(session.id(), &spec),
            session,
            runtime,
            title,
        };
        // `send_replace` stores the target even before the worker subscribes;
        // `send` would drop it when no receiver exists yet.
        let _ = self.inner.target.send_replace(Some(watched));
        let mut worker = self.inner.worker.lock().await;
        if worker.is_none() {
            let inner = self.inner.clone();
            *worker = Some(tokio::spawn(async move { run(inner).await }));
        }
    }

    /// Stop reporting and close the watched session's inbox row. The close
    /// frame is delivered by the worker before it exits, so awaiting it keeps
    /// the socket exchange ordered.
    pub async fn shutdown(&self) {
        self.inner.cancellation.cancel();
        if let Some(worker) = self.inner.worker.lock().await.take() {
            let _ = worker.await;
        }
    }
}

/// The worker's per-session observation state, swapped wholesale on attach.
#[derive(Default)]
struct WatchState {
    watched: Option<WatchedSession>,
    events: Option<broadcast::Receiver<SessionUpdate>>,
    titles: Option<watch::Receiver<String>>,
    session_title: Option<String>,
    turn: TurnState,
    announced: bool,
}

async fn run(inner: Arc<MoshiInner>) {
    let mut targets = inner.target.subscribe();
    let mut state = WatchState::default();
    let mut reported: Vec<FrameContext> = Vec::new();
    let mut last_tool_send: Option<Instant> = None;
    let mut last_progress_send: Option<Instant> = None;
    let mut backoff_until: Option<Instant> = None;

    let initial = targets.borrow_and_update().clone();
    attach(&inner, &mut state, initial, &mut backoff_until).await;
    loop {
        tokio::select! {
            () = inner.cancellation.cancelled() => break,
            changed = targets.changed() => {
                if changed.is_err() {
                    break;
                }
                let target = targets.borrow_and_update().clone();
                attach(&inner, &mut state, target, &mut backoff_until).await;
            }
            title = next_title(&mut state.titles) => {
                let Ok(title) = title else {
                    // The interface dropped the sender; stop watching titles
                    // until the next attach provides a receiver.
                    state.titles = None;
                    continue;
                };
                if title.is_empty() || state.session_title.as_deref() == Some(title.as_str()) {
                    continue;
                }
                state.session_title = Some(title.clone());
                let Some(current) = state.watched.as_ref() else {
                    continue;
                };
                // Before the row exists the first announce carries the
                // title; afterwards the refresh must be published — the
                // daemon drops category-less frames — so it rides a
                // session_started frame.
                if !state.announced {
                    continue;
                }
                let facts = FrameFacts {
                    session_title: Some(title.clone()),
                    context_remaining: context_remaining(&current.runtime).await,
                };
                let endpoint = inner.endpoint.read().expect("endpoint lock poisoned").clone();
                let frame = frame_for(
                    &MoshiAction::TitleUpdated {
                        title,
                        prompt: state.turn.prompt.clone(),
                    },
                    &current.context,
                    &endpoint,
                    &facts,
                    Utc::now(),
                );
                deliver(&inner, frame, &mut backoff_until).await;
            }
            update = next_update(&mut state.events) => {
                let (kind, persisted) = match update {
                    Ok(SessionUpdate::Persisted(event)) => (event.kind, true),
                    Ok(SessionUpdate::Transient(kind)) => (kind, false),
                    // Notifications are lossy: skipped events only delay the
                    // next inbox-row refresh.
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => {
                        state.events = None;
                        continue;
                    }
                };
                let Some(current) = state.watched.as_mut() else {
                    continue;
                };
                if let EventKind::AgentSpecUpdated { spec, .. } = &kind {
                    current.context = FrameContext::new(&current.context.session_id.clone(), spec);
                }
                let Some(notice) = observe(&kind, persisted, &mut state.turn) else {
                    continue;
                };
                let finished = matches!(notice, Notice::Finished);
                let actions = project(notice, &state.turn, &mut state.announced);
                if finished {
                    state.turn.snippet = None;
                    state.turn.error = None;
                    state.turn.failed = false;
                }
                for action in actions {
                    // TODO: moshi-hook never publishes category-less
                    // progress frames and explicitly suppresses tool
                    // progress ("daemon: suppressed tool progress event"),
                    // so sending these is pure socket noise. Drop them at
                    // the gate until the daemon grows a silent row-update
                    // channel; the projection above stays intact, so
                    // re-enabling means deleting this gate.
                    if matches!(
                        action,
                        MoshiAction::Progress { .. }
                            | MoshiAction::ToolRunning { .. }
                            | MoshiAction::ToolFinished { .. }
                    ) {
                        continue;
                    }
                    if matches!(action, MoshiAction::SessionStarted { .. }) {
                        remember_reported(&mut reported, &current.context);
                    }
                    // Tool and streaming frames throttle in separate windows,
                    // so a tool call right after a text burst still lands.
                    let window = match &action {
                        MoshiAction::ToolRunning { .. } | MoshiAction::ToolFinished { .. } => {
                            Some(&mut last_tool_send)
                        }
                        MoshiAction::Progress { .. } => Some(&mut last_progress_send),
                        _ => None,
                    };
                    if let Some(window) = window {
                        if window.is_some_and(|at| at.elapsed() < PROGRESS_THROTTLE) {
                            continue;
                        }
                        *window = Some(Instant::now());
                    }
                    let facts = FrameFacts {
                        session_title: state.session_title.clone(),
                        context_remaining: context_remaining(&current.runtime).await,
                    };
                    let endpoint =
                        inner.endpoint.read().expect("endpoint lock poisoned").clone();
                    let frame =
                        frame_for(&action, &current.context, &endpoint, &facts, Utc::now());
                    deliver(&inner, frame, &mut backoff_until).await;
                }
            }
        }
    }
    let visible_id = state
        .watched
        .as_ref()
        .map(|current| current.context.session_id.clone());
    drop(state.watched);
    // Close every announced row, not only the visible one. A session switch
    // leaves the previous conversation alive, so it is not closed until the
    // process exits. A backoff must not swallow these frames.
    for context in close_order(reported, visible_id.as_deref()) {
        let facts = FrameFacts {
            session_title: state
                .session_title
                .clone()
                .filter(|_| visible_id.as_deref() == Some(context.session_id.as_str())),
            context_remaining: None,
        };
        let endpoint = inner
            .endpoint
            .read()
            .expect("endpoint lock poisoned")
            .clone();
        let frame = frame_for(
            &MoshiAction::SessionClosed,
            &context,
            &endpoint,
            &facts,
            Utc::now(),
        );
        let mut no_backoff = None;
        deliver(&inner, frame, &mut no_backoff).await;
    }
}

/// Remember a session whose inbox row was opened. Repeating an id refreshes
/// the frame fields used when that row is closed.
fn remember_reported(reported: &mut Vec<FrameContext>, context: &FrameContext) {
    if let Some(existing) = reported
        .iter_mut()
        .find(|row| row.session_id == context.session_id)
    {
        *existing = context.clone();
        return;
    }
    reported.push(context.clone());
}

/// Announced rows to close, with the visible session last.
fn close_order(mut reported: Vec<FrameContext>, visible_id: Option<&str>) -> Vec<FrameContext> {
    if let Some(visible_id) = visible_id
        && let Some(index) = reported.iter().position(|row| row.session_id == visible_id)
    {
        let visible = reported.remove(index);
        reported.push(visible);
    }
    reported
}

/// Swap the watched session. Bind the pane, then send a category-less
/// update so the daemon can remember the terminal without pushing. The inbox
/// row opens on the first prompt, or on the first completion if that prompt
/// was already in the log.
async fn attach(
    inner: &MoshiInner,
    state: &mut WatchState,
    target: Option<WatchedSession>,
    backoff_until: &mut Option<Instant>,
) {
    resolve_tmux_names(inner).await;
    state.titles = target.as_ref().map(|watched| watched.title.clone());
    state.session_title = state.titles.as_mut().and_then(|receiver| {
        let title = receiver.borrow_and_update().clone();
        (!title.is_empty()).then_some(title)
    });
    state.events = target.as_ref().map(|watched| watched.session.subscribe());
    state.turn = TurnState::default();
    state.announced = false;
    state.watched = target;
    let Some(current) = state.watched.as_ref() else {
        return;
    };
    let endpoint = inner
        .endpoint
        .read()
        .expect("endpoint lock poisoned")
        .clone();
    if endpoint.has_pane() {
        let frame = frame_for(
            &MoshiAction::Bind,
            &current.context,
            &endpoint,
            &FrameFacts::default(),
            Utc::now(),
        );
        deliver(inner, frame, backoff_until).await;
    }
    let facts = FrameFacts {
        session_title: state.session_title.clone(),
        context_remaining: context_remaining(&current.runtime).await,
    };
    let frame = frame_for(
        &MoshiAction::State {
            title: state.session_title.clone(),
        },
        &current.context,
        &endpoint,
        &facts,
        Utc::now(),
    );
    deliver(inner, frame, backoff_until).await;
}

/// Receive the next session event, pending forever while no session is
/// watched so the select arm stays inert.
async fn next_update(
    events: &mut Option<broadcast::Receiver<SessionUpdate>>,
) -> Result<SessionUpdate, broadcast::error::RecvError> {
    match events.as_mut() {
        Some(receiver) => receiver.recv().await,
        None => std::future::pending().await,
    }
}

/// Receive the next generated session title, pending forever while no
/// session is watched so the select arm stays inert.
async fn next_title(
    titles: &mut Option<watch::Receiver<String>>,
) -> Result<String, watch::error::RecvError> {
    match titles.as_mut() {
        Some(receiver) => {
            receiver.changed().await?;
            Ok(receiver.borrow_and_update().clone())
        }
        None => std::future::pending().await,
    }
}

async fn deliver(inner: &MoshiInner, frame: MoshiFrame, backoff_until: &mut Option<Instant>) {
    if backoff_until.is_some_and(|until| Instant::now() < until) {
        return;
    }
    // A missing socket means no daemon is listening; the check keeps the
    // reporter inert while moshi-hook is absent and self-heals once it starts.
    if !inner.socket_path.exists() {
        return;
    }
    let mut line = match serde_json::to_vec(&frame) {
        Ok(line) => line,
        Err(_) => return,
    };
    line.push(b'\n');
    match tokio::time::timeout(SEND_TIMEOUT, send_line(&inner.socket_path, &line)).await {
        Ok(Ok(())) => *backoff_until = None,
        // Dropped on purpose: lifecycle frames are notifications, and the next
        // event rebuilds the inbox row.
        _ => *backoff_until = Some(Instant::now() + FAILURE_BACKOFF),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use crate::catalog::ModelLimits;
    use crate::catalog::ThinkingLevel;
    #[cfg(unix)]
    use crate::plugin::ModelToolRegistry;
    #[cfg(unix)]
    use crate::protocol::ProtocolRegistry;
    #[cfg(unix)]
    use crate::session::SessionContext;
    #[cfg(unix)]
    use crate::task::TaskManager;

    fn value_at(values: &[(&str, &str)], key: &str) -> Option<String> {
        values
            .iter()
            .find(|(name, _)| *name == key)
            .map(|(_, value)| (*value).to_string())
    }

    fn context_fixture() -> FrameContext {
        let spec = AgentSpec::root(
            "test-provider",
            "test-model",
            ThinkingLevel::Off,
            "/tmp/project",
        );
        FrameContext::new("sess-1", &spec)
    }

    #[test]
    fn reporter_is_on_by_default_and_disabled_explicitly() {
        // The explicit override wins over the platform default.
        #[cfg(unix)]
        {
            let overridden = [
                ("MOSHI_SOCKET_PATH", "/custom/moshi-hook.sock"),
                ("XDG_RUNTIME_DIR", "/run/user/1000"),
            ];
            let reporter = MoshiReporter::from_lookup(|key| value_at(&overridden, key)).unwrap();
            assert_eq!(
                reporter.inner.socket_path,
                PathBuf::from("/custom/moshi-hook.sock")
            );
        }

        // `URI_AGENT_MOSHI=0` disables reporting even with a socket path.
        let disabled = [
            ("URI_AGENT_MOSHI", "0"),
            ("MOSHI_SOCKET_PATH", "/tmp/moshi-hook.sock"),
        ];
        assert!(MoshiReporter::from_lookup(|key| value_at(&disabled, key)).is_none());

        // The platform default applies without the override.
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            let platform = [("XDG_RUNTIME_DIR", "/run/user/1000")];
            let reporter = MoshiReporter::from_lookup(|key| value_at(&platform, key)).unwrap();
            assert_eq!(
                reporter.inner.socket_path,
                PathBuf::from("/run/user/1000/moshi-hook.sock")
            );
            // Without a resolvable socket path the reporter still stays off.
            assert!(MoshiReporter::from_lookup(|_| None).is_none());
        }
        #[cfg(target_os = "macos")]
        {
            let platform = [("HOME", "/Users/test")];
            let reporter = MoshiReporter::from_lookup(|key| value_at(&platform, key)).unwrap();
            assert_eq!(
                reporter.inner.socket_path,
                PathBuf::from("/Users/test/Library/Application Support/Moshi/moshi-hook.sock")
            );
        }
        #[cfg(not(unix))]
        {
            // Native Windows is unsupported, so even the default stays off.
            assert!(MoshiReporter::from_lookup(|_| None).is_none());
        }
    }

    #[test]
    fn endpoint_detection_prefers_herdr_then_zellij_then_tmux() {
        assert_eq!(Endpoint::from_lookup(&|_| None), Endpoint::None);

        let tmux = [("TMUX_PANE", "%7")];
        assert_eq!(
            Endpoint::from_lookup(&|key| value_at(&tmux, key)),
            Endpoint::Tmux {
                pane: "%7".to_string(),
                session: None,
                window: None,
                socket: None,
            }
        );

        // The socket path is TMUX's first comma-separated field.
        let tmux_socket = [
            ("TMUX_PANE", "%7"),
            ("TMUX", "/tmp/tmux-1000/default,4035,0"),
        ];
        assert_eq!(
            Endpoint::from_lookup(&|key| value_at(&tmux_socket, key)),
            Endpoint::Tmux {
                pane: "%7".to_string(),
                session: None,
                window: None,
                socket: Some("/tmp/tmux-1000/default".to_string()),
            }
        );

        let zellij = [
            ("ZELLIJ", "0"),
            ("ZELLIJ_SESSION_NAME", "main"),
            ("ZELLIJ_PANE_ID", "terminal_1"),
            ("TMUX_PANE", "%7"),
        ];
        assert_eq!(
            Endpoint::from_lookup(&|key| value_at(&zellij, key)),
            Endpoint::Zellij {
                session: Some("main".to_string()),
                pane: Some("terminal_1".to_string())
            }
        );

        let herdr = [
            ("HERDR_ENV", "1"),
            ("HERDR_PANE_ID", "w1:p2"),
            ("HERDR_SESSION", "work"),
            ("HERDR_WORKSPACE_ID", "w1"),
            ("HERDR_TAB_ID", "w1:2"),
            ("TMUX_PANE", "%7"),
        ];
        assert_eq!(
            Endpoint::from_lookup(&|key| value_at(&herdr, key)),
            Endpoint::Herdr {
                pane: "w1:p2".to_string(),
                session: Some("work".to_string()),
                workspace_id: Some("w1".to_string()),
                tab_id: Some("w1:2".to_string()),
            }
        );

        // A Herdr gate without a pane falls through to the outer multiplexer.
        let incomplete = [("HERDR_ENV", "1"), ("TMUX_PANE", "%7")];
        assert_eq!(
            Endpoint::from_lookup(&|key| value_at(&incomplete, key)),
            Endpoint::Tmux {
                pane: "%7".to_string(),
                session: None,
                window: None,
                socket: None,
            }
        );
    }

    #[test]
    fn turn_events_map_to_inbox_categories() {
        let mut turn = TurnState::default();
        let mut announced = false;
        assert!(
            observe(
                &EventKind::User {
                    text: "hi".to_string()
                },
                true,
                &mut turn
            )
            .is_some()
        );
        assert_eq!(
            project(Notice::Prompt, &turn, &mut announced),
            vec![MoshiAction::SessionStarted {
                prompt: Some("hi".to_string())
            }]
        );
        // A later prompt refreshes the body but does not announce again.
        assert!(announced);
        observe(
            &EventKind::User {
                text: "again".to_string(),
            },
            true,
            &mut turn,
        );
        assert_eq!(
            project(Notice::Prompt, &turn, &mut announced),
            vec![MoshiAction::PromptSubmitted {
                prompt: "again".to_string()
            }]
        );
        // Streaming deltas move only the rolling tail; the completed block
        // owns the excerpt a settled turn quotes.
        assert_eq!(
            observe(
                &EventKind::AssistantText {
                    text: "working ".to_string()
                },
                false,
                &mut turn
            ),
            Some(Notice::Progress(Status::Writing))
        );
        assert_eq!(turn.activity, "working ");
        assert_eq!(turn.snippet, None);
        assert_eq!(
            observe(
                &EventKind::AssistantText {
                    text: "working on it".to_string()
                },
                true,
                &mut turn
            ),
            Some(Notice::Progress(Status::Writing))
        );
        assert_eq!(turn.snippet.as_deref(), Some("working on it"));
        assert_eq!(
            project(Notice::Progress(Status::Writing), &turn, &mut announced),
            vec![MoshiAction::Progress {
                status: Status::Writing,
                detail: Some("working on it".to_string())
            }]
        );
        // Reasoning reports its own status; a retry carries its numbers.
        assert_eq!(
            observe(
                &EventKind::AssistantReasoning {
                    text: "hmm".to_string()
                },
                false,
                &mut turn
            ),
            Some(Notice::Progress(Status::Reasoning))
        );
        assert_eq!(
            observe(
                &EventKind::ModelRetry {
                    attempt: 2,
                    max_retries: 5,
                    delay_ms: 1000,
                    reason: "rate limited".to_string()
                },
                false,
                &mut turn
            ),
            Some(Notice::Progress(Status::Retrying {
                attempt: 2,
                max_retries: 5,
                reason: "rate limited".to_string()
            }))
        );
        // A tool call clears the tail so its arguments own the body.
        assert_eq!(
            project(
                observe(
                    &EventKind::ToolCall {
                        call_id: "c1".to_string(),
                        name: "bash".to_string(),
                        arguments: serde_json::json!({ "command": "ls" })
                    },
                    true,
                    &mut turn
                )
                .unwrap(),
                &turn,
                &mut announced
            ),
            vec![MoshiAction::ToolRunning {
                tool: "bash".to_string(),
                detail: Some("{\"command\":\"ls\"}".to_string())
            }]
        );
        assert!(turn.activity.is_empty());
        // An argument-less call leaves the body to the previous frame.
        assert_eq!(
            observe(
                &EventKind::ToolCall {
                    call_id: "c2".to_string(),
                    name: "tasks".to_string(),
                    arguments: serde_json::json!({})
                },
                true,
                &mut turn
            ),
            Some(Notice::ToolStarted("tasks".to_string(), None))
        );
        assert_eq!(
            project(
                observe(
                    &EventKind::ToolResult {
                        call_id: "c1".to_string(),
                        name: "bash".to_string(),
                        output: "ok".to_string(),
                        failed: false,
                        protocol_help_required: false
                    },
                    true,
                    &mut turn
                )
                .unwrap(),
                &turn,
                &mut announced
            ),
            vec![MoshiAction::ToolFinished {
                tool: "bash".to_string()
            }]
        );
        assert_eq!(
            project(
                observe(&EventKind::TurnFinished, true, &mut turn).unwrap(),
                &turn,
                &mut announced
            ),
            vec![MoshiAction::TaskComplete {
                failed: false,
                snippet: Some("working on it".to_string()),
                prompt: Some("again".to_string()),
                error: None,
            }]
        );

        // The next turn starts clean, and an error flips its completion.
        // A completion with no prior announce still opens the inbox row.
        let mut announced = false;
        turn.snippet = None;
        turn.failed = false;
        assert_eq!(
            project(
                observe(&EventKind::TurnFinished, true, &mut turn).unwrap(),
                &turn,
                &mut announced
            ),
            vec![
                MoshiAction::SessionStarted {
                    prompt: Some("again".to_string())
                },
                MoshiAction::TaskComplete {
                    failed: false,
                    snippet: None,
                    prompt: Some("again".to_string()),
                    error: None,
                }
            ]
        );
        assert!(
            observe(
                &EventKind::Error {
                    text: "boom".to_string()
                },
                true,
                &mut turn
            )
            .is_none()
        );
        assert_eq!(
            project(
                observe(&EventKind::TurnFinished, true, &mut turn).unwrap(),
                &turn,
                &mut announced
            ),
            vec![MoshiAction::TaskComplete {
                failed: true,
                snippet: None,
                prompt: Some("again".to_string()),
                error: Some("boom".to_string()),
            }]
        );
    }

    #[test]
    fn frames_match_the_documented_envelope() {
        let context = context_fixture();
        let endpoint = Endpoint::Tmux {
            pane: "%7".to_string(),
            session: Some("main".to_string()),
            window: Some("3".to_string()),
            socket: Some("/tmp/tmux-1000/default".to_string()),
        };
        let now = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        let facts = FrameFacts::default();

        let started = serde_json::to_value(frame_for(
            &MoshiAction::SessionStarted {
                prompt: Some("hi".to_string()),
            },
            &context,
            &endpoint,
            &facts,
            now,
        ))
        .unwrap();
        assert_eq!(started["type"], "session.update");
        assert_eq!(started["source"], "pi");
        assert_eq!(started["sessionId"], "sess-1");
        assert_eq!(started["eventName"], "SessionStart");
        assert_eq!(started["category"], "session_started");
        assert_eq!(started["title"], "Session started");
        assert_eq!(started["message"], "hi");
        assert_eq!(started["cwd"], "/tmp/project");
        assert_eq!(started["projectName"], "project");
        assert_eq!(started["modelName"], "test-model");
        assert_eq!(started["terminalKind"], "tmux");
        assert_eq!(started["tmuxPane"], "%7");
        assert_eq!(started["tmuxSession"], "main");
        assert_eq!(started["tmuxWindow"], "3");
        assert_eq!(started["tmuxSocket"], "/tmp/tmux-1000/default");
        assert_eq!(started["subtitle"], "Thinking");
        assert!(started.get("contextRemaining").is_none());
        assert_eq!(started["requestedAt"], "2023-11-14T22:13:20Z");

        // Worker facts: the generated title owns the headline, and updates
        // carry the remaining context percentage.
        let facts = FrameFacts {
            session_title: Some("Fix parser recovery".to_string()),
            context_remaining: Some(87),
        };
        let completed = serde_json::to_value(frame_for(
            &MoshiAction::TaskComplete {
                failed: true,
                snippet: Some("partial answer".to_string()),
                prompt: Some("do the thing".to_string()),
                error: Some("boom".to_string()),
            },
            &context,
            &endpoint,
            &facts,
            now,
        ))
        .unwrap();
        assert_eq!(completed["eventName"], "Stop");
        assert_eq!(completed["category"], "task_complete");
        assert_eq!(completed["title"], "Fix parser recovery");
        assert_eq!(completed["subtitle"], "Turn failed");
        assert_eq!(completed["message"], "Turn failed\nboom");
        assert_eq!(completed["contextRemaining"], 87);

        // A settled turn reports the bare status and quotes the final reply,
        // not the prompt.
        let completed_success = serde_json::to_value(frame_for(
            &MoshiAction::TaskComplete {
                failed: false,
                snippet: Some("the fix is in".to_string()),
                prompt: Some("do the thing".to_string()),
                error: None,
            },
            &context,
            &endpoint,
            &facts,
            now,
        ))
        .unwrap();
        assert_eq!(completed_success["title"], "Fix parser recovery");
        assert_eq!(completed_success["subtitle"], "Task Done");
        assert_eq!(completed_success["message"], "Task Done\nthe fix is in");

        let completed_without_prompt = serde_json::to_value(frame_for(
            &MoshiAction::TaskComplete {
                failed: false,
                snippet: Some("all done".to_string()),
                prompt: None,
                error: None,
            },
            &context,
            &endpoint,
            &FrameFacts::default(),
            now,
        ))
        .unwrap();
        assert_eq!(completed_without_prompt["title"], "all done");
        assert_eq!(completed_without_prompt["message"], "Task Done\nall done");
        assert_eq!(completed_without_prompt["subtitle"], "Task Done");

        let refreshed = serde_json::to_value(frame_for(
            &MoshiAction::PromptSubmitted {
                prompt: "do the next thing".to_string(),
            },
            &context,
            &endpoint,
            &facts,
            now,
        ))
        .unwrap();
        assert_eq!(refreshed["eventName"], "UserPromptSubmit");
        assert_eq!(refreshed["category"], "session_started");
        assert_eq!(refreshed["title"], "Fix parser recovery");
        assert_eq!(refreshed["subtitle"], "Thinking");
        assert_eq!(refreshed["message"], "do the next thing");

        // Live progress is a silent state frame: the status rides the
        // subtitle and the rolling output tail the body.
        let progress = serde_json::to_value(frame_for(
            &MoshiAction::Progress {
                status: Status::Writing,
                detail: Some("streaming along".to_string()),
            },
            &context,
            &endpoint,
            &facts,
            now,
        ))
        .unwrap();
        assert_eq!(progress["type"], "session.update");
        assert!(progress.get("category").is_none());
        assert!(progress.get("title").is_none());
        assert_eq!(progress["subtitle"], "Writing");
        assert_eq!(progress["message"], "streaming along");
        assert_eq!(progress["contextRemaining"], 87);

        let retrying = serde_json::to_value(frame_for(
            &MoshiAction::Progress {
                status: Status::Retrying {
                    attempt: 2,
                    max_retries: 5,
                    reason: "rate limited".to_string(),
                },
                detail: Some("rate limited".to_string()),
            },
            &context,
            &endpoint,
            &FrameFacts::default(),
            now,
        ))
        .unwrap();
        assert_eq!(retrying["subtitle"], "Retrying 2/5");
        assert_eq!(retrying["message"], "rate limited");

        let tool = serde_json::to_value(frame_for(
            &MoshiAction::ToolRunning {
                tool: "bash".to_string(),
                detail: Some("{\"command\":\"cargo test\"}".to_string()),
            },
            &context,
            &endpoint,
            &FrameFacts::default(),
            now,
        ))
        .unwrap();
        assert_eq!(tool["eventName"], "PreToolUse");
        assert_eq!(tool["category"], "tool_running");
        // Tool frames carry no title, so they never clobber the row title;
        // the status rides the subtitle and the arguments the body.
        assert!(tool.get("title").is_none());
        assert_eq!(tool["subtitle"], "Running bash");
        assert_eq!(tool["message"], "{\"command\":\"cargo test\"}");
        assert_eq!(tool["toolName"], "bash");

        let tool_finished = serde_json::to_value(frame_for(
            &MoshiAction::ToolFinished {
                tool: "bash".to_string(),
            },
            &context,
            &endpoint,
            &FrameFacts::default(),
            now,
        ))
        .unwrap();
        // A finished call returns the subtitle to thinking and leaves the
        // body on the call's detail.
        assert_eq!(tool_finished["subtitle"], "Thinking");
        assert!(tool_finished.get("message").is_none());

        let closed = serde_json::to_value(frame_for(
            &MoshiAction::SessionClosed,
            &context,
            &Endpoint::None,
            &facts,
            now,
        ))
        .unwrap();
        assert_eq!(closed["type"], "session.closed");
        assert_eq!(closed["eventName"], "SessionEnd");
        assert_eq!(closed["category"], "session_ended");
        assert_eq!(closed["title"], "Fix parser recovery");
        // Bind and close frames stay without the context fill.
        assert!(closed.get("contextRemaining").is_none());
        assert!(closed.get("terminalKind").is_none());

        let closed_without_title = serde_json::to_value(frame_for(
            &MoshiAction::SessionClosed,
            &context,
            &Endpoint::None,
            &FrameFacts::default(),
            now,
        ))
        .unwrap();
        assert_eq!(closed_without_title["title"], "Session ended");

        let bind = serde_json::to_value(frame_for(
            &MoshiAction::Bind,
            &context,
            &endpoint,
            &FrameFacts::default(),
            now,
        ))
        .unwrap();
        assert_eq!(bind["type"], "session.bind");
        assert!(bind["agentPid"].is_u64());

        // A category-less state frame refreshes the row title silently.
        let state = serde_json::to_value(frame_for(
            &MoshiAction::State {
                title: Some("Fix parser recovery".to_string()),
            },
            &context,
            &endpoint,
            &facts,
            now,
        ))
        .unwrap();
        assert_eq!(state["type"], "session.update");
        assert!(state.get("category").is_none());
        assert_eq!(state["title"], "Fix parser recovery");
        assert_eq!(state["contextRemaining"], 87);

        // The generated title refreshes the row through a published frame:
        // a borrowed chat.message event name under the session_started
        // category, with the prompt keeping the body non-empty.
        let title_update = serde_json::to_value(frame_for(
            &MoshiAction::TitleUpdated {
                title: "Fix parser recovery".to_string(),
                prompt: Some("do the thing".to_string()),
            },
            &context,
            &endpoint,
            &facts,
            now,
        ))
        .unwrap();
        assert_eq!(title_update["type"], "session.update");
        assert_eq!(title_update["eventName"], "chat.message");
        assert_eq!(title_update["category"], "session_started");
        assert_eq!(title_update["title"], "Fix parser recovery");
        assert_eq!(title_update["message"], "do the thing");
        // No subtitle: the refresh says nothing about the turn.
        assert!(title_update.get("subtitle").is_none());
        assert_eq!(title_update["contextRemaining"], 87);

        // Before any prompt the title itself keeps the body non-empty.
        let title_without_prompt = serde_json::to_value(frame_for(
            &MoshiAction::TitleUpdated {
                title: "Fix parser recovery".to_string(),
                prompt: None,
            },
            &context,
            &endpoint,
            &facts,
            now,
        ))
        .unwrap();
        assert_eq!(title_without_prompt["message"], "Fix parser recovery");
    }

    #[test]
    fn remaining_context_mirrors_the_daemon_hooks() {
        assert_eq!(remaining_percent(0, 1000), None);
        assert_eq!(remaining_percent(100, 0), None);
        assert_eq!(remaining_percent(500, 1000), Some(50));
        assert_eq!(remaining_percent(1, 1000), Some(100));
        assert_eq!(remaining_percent(1000, 1000), Some(1));
        assert_eq!(remaining_percent(1500, 1000), Some(1));
    }

    #[test]
    fn truncate_respects_character_boundaries() {
        let long = "中".repeat(TITLE_MAX + 10);
        assert_eq!(truncate(&long, TITLE_MAX).chars().count(), TITLE_MAX);
        assert_eq!(truncate("  padded  ", TITLE_MAX), "padded");
    }

    #[test]
    fn tail_keeps_the_latest_characters() {
        assert_eq!(tail("short", 10), "short");
        assert_eq!(tail("abcdefgh", 3), "fgh");
        let mut buffer = String::from("ab");
        push_tail(&mut buffer, "cdef", 4);
        assert_eq!(buffer, "cdef");
        // Multi-byte characters stay whole.
        push_tail(&mut buffer, "中", 4);
        assert_eq!(buffer, "def中");
    }

    #[test]
    fn close_order_ends_the_visible_session_last() {
        let mut first = context_fixture();
        first.session_id = "first".to_string();
        let mut second = context_fixture();
        second.session_id = "second".to_string();
        let order = close_order(vec![first, second], Some("first"));
        assert_eq!(order[0].session_id, "second");
        assert_eq!(order[1].session_id, "first");
        assert!(close_order(Vec::new(), Some("missing")).is_empty());
    }

    #[cfg(unix)]
    async fn wait_for_frames(
        received: &Mutex<Vec<serde_json::Value>>,
        count: usize,
    ) -> Vec<serde_json::Value> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let frames = received.lock().await.clone();
            if frames.len() >= count {
                return frames;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {count} frames"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[cfg(unix)]
    async fn session_fixture(database: &Path, cwd: &Path, id: &str) -> Session {
        let session = Session::open_at(
            database.to_path_buf(),
            Some(id),
            cwd,
            "test-provider",
            "test-model",
            SessionContext {
                system_prompt: "system".to_string(),
                skills: Vec::new(),
            },
        )
        .await
        .unwrap();
        session.persist().await.unwrap();
        session
    }

    #[cfg(unix)]
    async fn runtime_fixture(database: &Path, cwd: &Path, id: &str) -> Arc<AgentRuntime> {
        let session = session_fixture(database, cwd, id).await;
        let output = Arc::new(
            crate::output::OutputStore::new(id, 32 * 1024)
                .await
                .unwrap(),
        );
        Arc::new(AgentRuntime::new(
            None,
            Arc::new(ProtocolRegistry::new(output, TaskManager::new())),
            Arc::new(ModelToolRegistry::new()),
            session,
            "system".to_string(),
            ModelLimits::default(),
        ))
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn reporter_streams_lifecycle_frames_to_the_daemon_socket() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let temp = tempfile::tempdir().unwrap();
        let socket = temp.path().join("moshi-hook.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let received = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
        let server_received = received.clone();
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let mut line = Vec::new();
                let mut byte = [0_u8; 1];
                loop {
                    match stream.read(&mut byte).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {
                            if byte[0] == b'\n' {
                                break;
                            }
                            line.push(byte[0]);
                        }
                    }
                }
                if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&line) {
                    server_received.lock().await.push(value);
                }
                let _ = stream.write_all(b"{\"type\":\"ack\"}\n").await;
            }
        });

        let cwd = temp.path().join("project");
        tokio::fs::create_dir_all(&cwd).await.unwrap();
        let runtime = runtime_fixture(&temp.path().join("sessions.db"), &cwd, "sess-1").await;
        let session = runtime.session().clone();

        let reporter = MoshiReporter::from_parts(
            socket.clone(),
            Endpoint::Tmux {
                pane: "%7".to_string(),
                session: None,
                window: None,
                socket: None,
            },
        );
        let (_title_sender, title_receiver) = watch::channel(String::new());
        reporter.start(runtime.clone(), title_receiver).await;

        // The attach frames prove the worker subscribed to the session, so
        // later appends cannot race the subscription. Attach itself is not a push.
        let frames = wait_for_frames(&received, 2).await;
        assert_eq!(frames[0]["type"], "session.bind");
        assert_eq!(frames[0]["tmuxPane"], "%7");
        assert_eq!(frames[1]["type"], "session.update");
        assert_eq!(frames[1]["eventName"], "SessionStart");
        assert!(frames[1].get("category").is_none());
        assert_eq!(frames[1]["sessionId"], "sess-1");
        assert_eq!(frames[1]["modelName"], "test-model");

        session
            .append(EventKind::User {
                text: "ship it".to_string(),
            })
            .await
            .unwrap();
        let frames = wait_for_frames(&received, 3).await;
        assert_eq!(frames[2]["category"], "session_started");
        assert_eq!(frames[2]["subtitle"], "Thinking");
        assert_eq!(frames[2]["message"], "ship it");

        session
            .append(EventKind::AssistantText {
                text: "all done".to_string(),
            })
            .await
            .unwrap();
        session
            .append(EventKind::ToolCall {
                call_id: "c1".to_string(),
                name: "bash".to_string(),
                arguments: serde_json::json!({}),
            })
            .await
            .unwrap();
        session
            .append(EventKind::ToolResult {
                call_id: "c1".to_string(),
                name: "bash".to_string(),
                output: "ok".to_string(),
                failed: false,
                protocol_help_required: false,
            })
            .await
            .unwrap();
        session.append(EventKind::TurnFinished).await.unwrap();

        // Progress and tool frames are projected but dropped at the send
        // gate — the daemon never publishes them — so the completion is
        // the frame right after the prompt announce.
        let frames = wait_for_frames(&received, 4).await;
        assert_eq!(frames[3]["category"], "task_complete");
        assert_eq!(frames[3]["eventName"], "Stop");
        assert_eq!(frames[3]["title"], "all done");
        // The completion reports the bare status and quotes the final
        // reply, not the prompt.
        assert_eq!(frames[3]["subtitle"], "Task Done");
        assert_eq!(frames[3]["message"], "Task Done\nall done");

        reporter.shutdown().await;
        let frames = wait_for_frames(&received, 5).await;
        assert_eq!(frames[4]["type"], "session.closed");
        assert_eq!(frames[4]["category"], "session_ended");
        assert_eq!(frames[4]["sessionId"], "sess-1");
        server.abort();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn generated_title_and_context_remaining_ride_later_frames() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let temp = tempfile::tempdir().unwrap();
        let socket = temp.path().join("moshi-hook.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let received = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
        let server_received = received.clone();
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let mut line = Vec::new();
                let mut byte = [0_u8; 1];
                loop {
                    match stream.read(&mut byte).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {
                            if byte[0] == b'\n' {
                                break;
                            }
                            line.push(byte[0]);
                        }
                    }
                }
                if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&line) {
                    server_received.lock().await.push(value);
                }
                let _ = stream.write_all(b"{\"type\":\"ack\"}\n").await;
            }
        });

        let cwd = temp.path().join("project");
        tokio::fs::create_dir_all(&cwd).await.unwrap();
        let runtime = runtime_fixture(&temp.path().join("sessions.db"), &cwd, "sess-1").await;
        runtime
            .set_backend(
                None,
                Some(ModelLimits {
                    context_window: 1_000,
                    ..ModelLimits::default()
                }),
            )
            .await;
        runtime.refresh_context_estimate().await;
        let expected_remaining = remaining_percent(runtime.context_usage().tokens, 1_000).unwrap();
        let session = runtime.session().clone();

        let reporter = MoshiReporter::from_parts(socket.clone(), Endpoint::None);
        let (title_sender, title_receiver) = watch::channel(String::new());
        reporter.start(runtime.clone(), title_receiver).await;

        // The attach state frame already carries the context fill.
        let frames = wait_for_frames(&received, 1).await;
        assert_eq!(frames[0]["type"], "session.update");
        assert!(frames[0].get("category").is_none());
        assert!(frames[0].get("title").is_none());
        assert_eq!(frames[0]["contextRemaining"], expected_remaining);

        session
            .append(EventKind::User {
                text: "ship it".to_string(),
            })
            .await
            .unwrap();
        let frames = wait_for_frames(&received, 2).await;
        assert_eq!(frames[1]["category"], "session_started");
        // No generated title yet: the fallback title announces the session.
        assert_eq!(frames[1]["title"], "Session started");
        assert_eq!(frames[1]["subtitle"], "Thinking");
        assert_eq!(frames[1]["contextRemaining"], expected_remaining);

        // The generated title arrives asynchronously and refreshes the row.
        // A category-less refresh would be dropped by the daemon, so the
        // title rides a published session_started frame instead, with the
        // prompt keeping the body non-empty.
        title_sender
            .send("Fix parser recovery".to_string())
            .unwrap();
        let frames = wait_for_frames(&received, 3).await;
        assert_eq!(frames[2]["type"], "session.update");
        assert_eq!(frames[2]["eventName"], "chat.message");
        assert_eq!(frames[2]["category"], "session_started");
        assert_eq!(frames[2]["title"], "Fix parser recovery");
        assert_eq!(frames[2]["message"], "ship it");
        assert_eq!(frames[2]["contextRemaining"], expected_remaining);

        session
            .append(EventKind::AssistantText {
                text: "all done".to_string(),
            })
            .await
            .unwrap();
        session.append(EventKind::TurnFinished).await.unwrap();
        let frames = wait_for_frames(&received, 4).await;
        // The streaming progress frame is dropped at the send gate, so the
        // completion is the next published frame.
        assert_eq!(frames[3]["category"], "task_complete");
        assert_eq!(frames[3]["title"], "Fix parser recovery");
        assert_eq!(frames[3]["contextRemaining"], expected_remaining);
        // The completion reports the bare status and quotes the final
        // reply, not the prompt.
        assert_eq!(frames[3]["subtitle"], "Task Done");
        assert_eq!(frames[3]["message"], "Task Done\nall done");

        reporter.shutdown().await;
        let frames = wait_for_frames(&received, 5).await;
        assert_eq!(frames[4]["type"], "session.closed");
        assert_eq!(frames[4]["title"], "Fix parser recovery");
        // Close frames stay without the context fill.
        assert!(frames[4].get("contextRemaining").is_none());
        server.abort();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn reporter_swaps_sessions_and_stays_inert_without_a_daemon() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().join("project");
        tokio::fs::create_dir_all(&cwd).await.unwrap();
        let database = temp.path().join("sessions.db");
        let first = runtime_fixture(&database, &cwd, "first-session").await;
        let second = runtime_fixture(&database, &cwd, "second-session").await;

        // A socket path with no listener keeps every delivery a silent no-op.
        let reporter = MoshiReporter::from_parts(temp.path().join("missing.sock"), Endpoint::None);
        let (_first_title, first_receiver) = watch::channel(String::new());
        let (_second_title, second_receiver) = watch::channel(String::new());
        reporter.start(first, first_receiver).await;
        reporter.start(second.clone(), second_receiver).await;
        assert!(reporter.inner.worker.lock().await.is_some());

        tokio::time::timeout(Duration::from_secs(2), reporter.shutdown())
            .await
            .expect("shutdown must not hang");
        assert!(reporter.inner.worker.lock().await.is_none());
        drop(second);
    }
}
