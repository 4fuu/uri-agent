//! moshi-hook daemon reporting.
//!
//! [Moshi](https://getmoshi.app/) is a mobile terminal app whose `moshi-hook`
//! daemon collects coding-agent lifecycle events over a local Unix socket
//! carrying newline-delimited JSON. When a daemon socket resolves, the
//! reporter forwards the visible session's
//! lifecycle — session start, turn completion, and throttled tool activity —
//! so Moshi can keep its inbox row and push notifications current. Reporting
//! is on by default and disabled with `URI_AGENT_MOSHI=0`. Only Unix
//! processes resolve a socket — WSL qualifies as Linux, while native Windows
//! never probes or reports. Session switches
//! move reporting to the newly visible session and rebind its terminal pane;
//! shutdown closes the reported row. Without a listening daemon the reporter
//! stays completely inactive, and reporting failures never affect the
//! conversation.

use crate::agent::AgentSpec;
use crate::session::{EventKind, Session, SessionUpdate};
use chrono::{DateTime, SecondsFormat, Utc};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, broadcast, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Agent identity Moshi shows on inbox rows and notifications.
const SOURCE: &str = "uri-agent";
/// Tool activity fires many times per turn; one frame per window keeps the
/// inbox row fresh without flooding the daemon, which applies its own
/// five-second push throttle as well.
const TOOL_THROTTLE: Duration = Duration::from_secs(5);
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

/// The multiplexer pane the process runs in, when it advertises one. Moshi
/// uses the pane to attribute the session to the right terminal.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
enum Endpoint {
    #[default]
    None,
    Tmux {
        pane: String,
    },
    Zellij {
        session: Option<String>,
        pane: Option<String>,
    },
    Herdr {
        pane: String,
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
            return Self::Herdr { pane };
        }
        if present(lookup("ZELLIJ")).is_some() {
            return Self::Zellij {
                session: present(lookup("ZELLIJ_SESSION_NAME")),
                pane: present(lookup("ZELLIJ_PANE_ID")),
            };
        }
        if let Some(pane) = present(lookup("TMUX_PANE")) {
            return Self::Tmux { pane };
        }
        Self::None
    }

    fn has_pane(&self) -> bool {
        !matches!(self, Self::None)
    }

    #[allow(clippy::type_complexity)]
    fn fields(
        &self,
    ) -> (
        Option<&'static str>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    ) {
        match self {
            Self::None => (None, None, None, None, None),
            Self::Tmux { pane } => (Some("tmux"), Some(pane.clone()), None, None, None),
            Self::Zellij { session, pane } => {
                (Some("zellij"), None, session.clone(), pane.clone(), None)
            }
            Self::Herdr { pane } => (Some("herdr"), None, None, None, Some(pane.clone())),
        }
    }
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

/// What one session event should tell Moshi, before serialization.
#[derive(Clone, Debug, PartialEq, Eq)]
enum MoshiAction {
    /// Attach the terminal pane to the session; never published as a push.
    Bind,
    SessionStarted,
    TaskComplete {
        failed: bool,
        snippet: Option<String>,
    },
    ToolRunning {
        tool: String,
    },
    ToolFinished {
        tool: String,
        failed: bool,
    },
    SessionClosed,
}

/// Per-turn reporting state: the latest assistant text becomes the completion
/// title, and any error flips the completion to failed.
#[derive(Default)]
struct TurnState {
    snippet: Option<String>,
    failed: bool,
}

fn map_event(kind: &EventKind, turn: &mut TurnState) -> Option<MoshiAction> {
    match kind {
        EventKind::AssistantText { text } => {
            turn.snippet = Some(truncate(text, TITLE_MAX));
            None
        }
        EventKind::Error { .. } => {
            turn.failed = true;
            None
        }
        EventKind::TurnFinished => Some(MoshiAction::TaskComplete {
            failed: std::mem::take(&mut turn.failed),
            snippet: turn.snippet.take(),
        }),
        EventKind::ToolCall { name, .. } => Some(MoshiAction::ToolRunning { tool: name.clone() }),
        EventKind::ToolResult { name, failed, .. } => Some(MoshiAction::ToolFinished {
            tool: name.clone(),
            failed: *failed,
        }),
        _ => None,
    }
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
    #[serde(skip_serializing_if = "Option::is_none")]
    category: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
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
    #[serde(rename = "terminalKind", skip_serializing_if = "Option::is_none")]
    terminal_kind: Option<&'static str>,
    #[serde(rename = "tmuxPane", skip_serializing_if = "Option::is_none")]
    tmux_pane: Option<String>,
    #[serde(rename = "zellijSession", skip_serializing_if = "Option::is_none")]
    zellij_session: Option<String>,
    #[serde(rename = "zellijPane", skip_serializing_if = "Option::is_none")]
    zellij_pane: Option<String>,
    #[serde(rename = "herdrPane", skip_serializing_if = "Option::is_none")]
    herdr_pane: Option<String>,
}

/// The action-specific fields of a frame: message type, inbox category,
/// title, body, tool name, and the binding process ID.
type FrameParts = (
    &'static str,
    Option<&'static str>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<u32>,
);

fn frame_for(
    action: &MoshiAction,
    context: &FrameContext,
    endpoint: &Endpoint,
    now: DateTime<Utc>,
) -> MoshiFrame {
    let (kind, category, title, message, tool_name, agent_pid): FrameParts = match action {
        MoshiAction::Bind => (
            "session.bind",
            None,
            None,
            None,
            None,
            Some(std::process::id()),
        ),
        MoshiAction::SessionStarted => (
            "session.update",
            Some("session_started"),
            Some("Session started".to_string()),
            None,
            None,
            None,
        ),
        MoshiAction::TaskComplete {
            failed: true,
            snippet,
        } => (
            "session.update",
            Some("task_complete"),
            Some("Turn failed".to_string()),
            snippet.clone(),
            None,
            None,
        ),
        MoshiAction::TaskComplete {
            failed: false,
            snippet,
        } => (
            "session.update",
            Some("task_complete"),
            Some(
                snippet
                    .clone()
                    .unwrap_or_else(|| "Turn complete".to_string()),
            ),
            None,
            None,
            None,
        ),
        MoshiAction::ToolRunning { tool } => (
            "session.update",
            Some("tool_running"),
            Some(format!("Running {tool}")),
            None,
            Some(tool.clone()),
            None,
        ),
        MoshiAction::ToolFinished { tool, failed } => (
            "session.update",
            Some("tool_finished"),
            Some(if *failed {
                format!("{tool} failed")
            } else {
                format!("{tool} finished")
            }),
            None,
            Some(tool.clone()),
            None,
        ),
        MoshiAction::SessionClosed => ("session.closed", None, None, None, None, None),
    };
    let (terminal_kind, tmux_pane, zellij_session, zellij_pane, herdr_pane) = endpoint.fields();
    MoshiFrame {
        kind,
        source: SOURCE,
        session_id: context.session_id.clone(),
        requested_at: now.to_rfc3339_opts(SecondsFormat::Secs, true),
        category,
        title,
        message,
        tool_name,
        model_name: (!context.model.is_empty()).then(|| context.model.clone()),
        cwd: Some(context.cwd.clone()),
        project_name: Some(context.project.clone()),
        agent_pid,
        terminal_kind,
        tmux_pane,
        zellij_session,
        zellij_pane,
        herdr_pane,
    }
}

fn truncate(text: &str, max: usize) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= max {
        return trimmed.to_string();
    }
    trimmed.chars().take(max).collect()
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
    stream.shutdown().await?;
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
    context: FrameContext,
}

struct MoshiInner {
    socket_path: PathBuf,
    endpoint: Endpoint,
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
                endpoint,
                target: watch::channel(None).0,
                cancellation: CancellationToken::new(),
                worker: Mutex::new(None),
            }),
        }
    }

    /// Report the session now shown in the terminal. Repeated calls move
    /// reporting to the newly visible session without restarting the worker.
    pub async fn start(&self, session: Session) {
        let spec = session.spec().await;
        let watched = WatchedSession {
            context: FrameContext::new(session.id(), &spec),
            session,
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

async fn run(inner: Arc<MoshiInner>) {
    let mut targets = inner.target.subscribe();
    let mut watched: Option<WatchedSession> = None;
    let mut events: Option<broadcast::Receiver<SessionUpdate>> = None;
    let mut turn = TurnState::default();
    let mut last_tool_send: Option<Instant> = None;
    let mut backoff_until: Option<Instant> = None;

    let initial = targets.borrow_and_update().clone();
    attach(
        &inner,
        &mut watched,
        &mut events,
        &mut turn,
        initial,
        &mut backoff_until,
    )
    .await;
    loop {
        tokio::select! {
            () = inner.cancellation.cancelled() => break,
            changed = targets.changed() => {
                if changed.is_err() {
                    break;
                }
                let target = targets.borrow_and_update().clone();
                attach(&inner, &mut watched, &mut events, &mut turn, target, &mut backoff_until)
                    .await;
            }
            update = next_update(&mut events) => {
                let kind = match update {
                    Ok(SessionUpdate::Persisted(event)) => event.kind,
                    Ok(SessionUpdate::Transient(kind)) => kind,
                    // Notifications are lossy: skipped events only delay the
                    // next inbox-row refresh.
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => {
                        events = None;
                        continue;
                    }
                };
                let Some(current) = watched.as_mut() else {
                    continue;
                };
                if let EventKind::AgentSpecUpdated { spec, .. } = &kind {
                    current.context = FrameContext::new(&current.context.session_id.clone(), spec);
                }
                let Some(action) = map_event(&kind, &mut turn) else {
                    continue;
                };
                let tool_event = matches!(
                    action,
                    MoshiAction::ToolRunning { .. } | MoshiAction::ToolFinished { .. }
                );
                if tool_event && last_tool_send.is_some_and(|at| at.elapsed() < TOOL_THROTTLE) {
                    continue;
                }
                if tool_event {
                    last_tool_send = Some(Instant::now());
                }
                let frame = frame_for(&action, &current.context, &inner.endpoint, Utc::now());
                deliver(&inner, frame, &mut backoff_until).await;
            }
        }
    }
    if let Some(current) = watched.take() {
        let frame = frame_for(
            &MoshiAction::SessionClosed,
            &current.context,
            &inner.endpoint,
            Utc::now(),
        );
        // A backoff from an earlier failure must not swallow the close frame.
        let mut no_backoff = None;
        deliver(&inner, frame, &mut no_backoff).await;
    }
}

/// Swap the watched session and announce it: bind the terminal pane first so
/// the daemon attributes it to this session, then open the inbox row.
async fn attach(
    inner: &MoshiInner,
    watched: &mut Option<WatchedSession>,
    events: &mut Option<broadcast::Receiver<SessionUpdate>>,
    turn: &mut TurnState,
    target: Option<WatchedSession>,
    backoff_until: &mut Option<Instant>,
) {
    *watched = target;
    *events = watched.as_ref().map(|watched| watched.session.subscribe());
    *turn = TurnState::default();
    let Some(current) = watched.as_ref() else {
        return;
    };
    if inner.endpoint.has_pane() {
        let frame = frame_for(
            &MoshiAction::Bind,
            &current.context,
            &inner.endpoint,
            Utc::now(),
        );
        deliver(inner, frame, backoff_until).await;
    }
    let frame = frame_for(
        &MoshiAction::SessionStarted,
        &current.context,
        &inner.endpoint,
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
    use crate::catalog::ThinkingLevel;
    #[cfg(unix)]
    use crate::session::SessionContext;

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
                pane: "%7".to_string()
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
            ("TMUX_PANE", "%7"),
        ];
        assert_eq!(
            Endpoint::from_lookup(&|key| value_at(&herdr, key)),
            Endpoint::Herdr {
                pane: "w1:p2".to_string()
            }
        );

        // A Herdr gate without a pane falls through to the outer multiplexer.
        let incomplete = [("HERDR_ENV", "1"), ("TMUX_PANE", "%7")];
        assert_eq!(
            Endpoint::from_lookup(&|key| value_at(&incomplete, key)),
            Endpoint::Tmux {
                pane: "%7".to_string()
            }
        );
    }

    #[test]
    fn turn_events_map_to_inbox_categories() {
        let mut turn = TurnState::default();
        assert!(
            map_event(
                &EventKind::User {
                    text: "hi".to_string()
                },
                &mut turn
            )
            .is_none()
        );
        assert!(
            map_event(
                &EventKind::AssistantText {
                    text: "working on it".to_string()
                },
                &mut turn
            )
            .is_none()
        );
        assert_eq!(
            map_event(
                &EventKind::ToolCall {
                    call_id: "c1".to_string(),
                    name: "bash".to_string(),
                    arguments: serde_json::json!({})
                },
                &mut turn
            ),
            Some(MoshiAction::ToolRunning {
                tool: "bash".to_string()
            })
        );
        assert_eq!(
            map_event(
                &EventKind::ToolResult {
                    call_id: "c1".to_string(),
                    name: "bash".to_string(),
                    output: "ok".to_string(),
                    failed: false,
                    protocol_help_required: false
                },
                &mut turn
            ),
            Some(MoshiAction::ToolFinished {
                tool: "bash".to_string(),
                failed: false
            })
        );
        assert_eq!(
            map_event(&EventKind::TurnFinished, &mut turn),
            Some(MoshiAction::TaskComplete {
                failed: false,
                snippet: Some("working on it".to_string())
            })
        );

        // The next turn starts clean, and an error flips its completion.
        assert_eq!(
            map_event(&EventKind::TurnFinished, &mut turn),
            Some(MoshiAction::TaskComplete {
                failed: false,
                snippet: None
            })
        );
        assert!(
            map_event(
                &EventKind::Error {
                    text: "boom".to_string()
                },
                &mut turn
            )
            .is_none()
        );
        assert_eq!(
            map_event(&EventKind::TurnFinished, &mut turn),
            Some(MoshiAction::TaskComplete {
                failed: true,
                snippet: None
            })
        );
    }

    #[test]
    fn frames_match_the_documented_envelope() {
        let context = context_fixture();
        let endpoint = Endpoint::Tmux {
            pane: "%7".to_string(),
        };
        let now = DateTime::from_timestamp(1_700_000_000, 0).unwrap();

        let started = serde_json::to_value(frame_for(
            &MoshiAction::SessionStarted,
            &context,
            &endpoint,
            now,
        ))
        .unwrap();
        assert_eq!(started["type"], "session.update");
        assert_eq!(started["source"], "uri-agent");
        assert_eq!(started["sessionId"], "sess-1");
        assert_eq!(started["category"], "session_started");
        assert_eq!(started["title"], "Session started");
        assert_eq!(started["cwd"], "/tmp/project");
        assert_eq!(started["projectName"], "project");
        assert_eq!(started["modelName"], "test-model");
        assert_eq!(started["terminalKind"], "tmux");
        assert_eq!(started["tmuxPane"], "%7");
        assert_eq!(started["requestedAt"], "2023-11-14T22:13:20Z");

        let completed = serde_json::to_value(frame_for(
            &MoshiAction::TaskComplete {
                failed: true,
                snippet: Some("partial answer".to_string()),
            },
            &context,
            &endpoint,
            now,
        ))
        .unwrap();
        assert_eq!(completed["category"], "task_complete");
        assert_eq!(completed["title"], "Turn failed");
        assert_eq!(completed["message"], "partial answer");

        let tool = serde_json::to_value(frame_for(
            &MoshiAction::ToolRunning {
                tool: "bash".to_string(),
            },
            &context,
            &endpoint,
            now,
        ))
        .unwrap();
        assert_eq!(tool["category"], "tool_running");
        assert_eq!(tool["title"], "Running bash");
        assert_eq!(tool["toolName"], "bash");

        let closed = serde_json::to_value(frame_for(
            &MoshiAction::SessionClosed,
            &context,
            &Endpoint::None,
            now,
        ))
        .unwrap();
        assert_eq!(closed["type"], "session.closed");
        assert!(closed.get("category").is_none());
        assert!(closed.get("terminalKind").is_none());

        let bind =
            serde_json::to_value(frame_for(&MoshiAction::Bind, &context, &endpoint, now)).unwrap();
        assert_eq!(bind["type"], "session.bind");
        assert!(bind["agentPid"].is_u64());
    }

    #[test]
    fn truncate_respects_character_boundaries() {
        let long = "中".repeat(TITLE_MAX + 10);
        assert_eq!(truncate(&long, TITLE_MAX).chars().count(), TITLE_MAX);
        assert_eq!(truncate("  padded  ", TITLE_MAX), "padded");
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
        let session = session_fixture(&temp.path().join("sessions.db"), &cwd, "sess-1").await;

        let reporter = MoshiReporter::from_parts(
            socket.clone(),
            Endpoint::Tmux {
                pane: "%7".to_string(),
            },
        );
        reporter.start(session.clone()).await;

        // The attach frames prove the worker subscribed to the session, so
        // later appends cannot race the subscription.
        let frames = wait_for_frames(&received, 2).await;
        assert_eq!(frames[0]["type"], "session.bind");
        assert_eq!(frames[0]["tmuxPane"], "%7");
        assert_eq!(frames[1]["category"], "session_started");
        assert_eq!(frames[1]["sessionId"], "sess-1");
        assert_eq!(frames[1]["modelName"], "test-model");

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

        let frames = wait_for_frames(&received, 4).await;
        assert_eq!(frames[2]["category"], "tool_running");
        assert_eq!(frames[2]["toolName"], "bash");
        // The tool result landed inside the throttle window and was dropped.
        assert_eq!(frames[3]["category"], "task_complete");
        assert_eq!(frames[3]["title"], "all done");

        reporter.shutdown().await;
        let frames = wait_for_frames(&received, 5).await;
        assert_eq!(frames[4]["type"], "session.closed");
        assert_eq!(frames[4]["sessionId"], "sess-1");
        server.abort();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn reporter_swaps_sessions_and_stays_inert_without_a_daemon() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().join("project");
        tokio::fs::create_dir_all(&cwd).await.unwrap();
        let database = temp.path().join("sessions.db");
        let first = session_fixture(&database, &cwd, "first-session").await;
        let second = session_fixture(&database, &cwd, "second-session").await;

        // A socket path with no listener keeps every delivery a silent no-op.
        let reporter = MoshiReporter::from_parts(temp.path().join("missing.sock"), Endpoint::None);
        reporter.start(first).await;
        reporter.start(second.clone()).await;
        assert!(reporter.inner.worker.lock().await.is_some());

        tokio::time::timeout(Duration::from_secs(2), reporter.shutdown())
            .await
            .expect("shutdown must not hang");
        assert!(reporter.inner.worker.lock().await.is_none());
        drop(second);
    }
}
