use crate::agent::{AgentSpec, SubmitKind};
use crate::catalog::ThinkingLevel;
use crate::config::display_path;
use crate::skill::SkillSnapshot;
use crate::task::{TaskReport, TaskStatus};
use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use rig::message::{Message, UserContent};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::fs;
use tokio::sync::{Mutex, broadcast};
use tokio_rusqlite::{
    Connection,
    rusqlite::{Connection as SqliteConnection, OpenFlags, OptionalExtension, params},
};
use uuid::Uuid;

const SESSION_DATABASE_FILE: &str = "sessions-v3.db";
const RESUME_INDEX_VERSION: u32 = 2;
const MAX_EVENT_PAGE: usize = 512;
const RESUME_EVENT_KINDS: &[&str] = &[
    "session_created",
    "agent_spec_updated",
    "user",
    "assistant_text",
    "assistant_reasoning",
    "tool_call",
    "tool_result",
    "model_message",
    "task",
    "model_retry",
    "usage",
    "error",
    "compaction",
];

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct SessionTuiState {
    pub(crate) head_sequence: Option<u64>,
    pub(crate) latest_compaction_sequence: Option<u64>,
    pub(crate) usage: SessionTuiUsage,
    pub(crate) token_calibration: SessionTuiTokenCalibration,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct SessionTuiUsage {
    pub(crate) input: u64,
    pub(crate) output: u64,
    pub(crate) cache_read: u64,
    pub(crate) cache_write: u64,
    pub(crate) cost: f64,
    pub(crate) last_cache_hit: Option<f64>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct SessionTuiTokenCalibration {
    pub(crate) visible_units: u64,
    pub(crate) output_tokens: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SessionEventHeader {
    pub(crate) sequence: u64,
    pub(crate) starts_turn: bool,
    pub(crate) finishes_turn: bool,
}

#[derive(Clone, Debug)]
pub enum SessionChoice {
    New,
    Latest,
    Existing(String),
}

#[derive(Clone, Debug)]
pub struct SessionSummary {
    pub id: String,
    pub updated_at: DateTime<Utc>,
    pub provider: String,
    pub model: String,
    pub thinking: ThinkingLevel,
    pub preview: String,
}

#[derive(Clone, Debug)]
pub(crate) struct PendingInput {
    pub(crate) id: i64,
    pub(crate) kind: SubmitKind,
    pub(crate) text: String,
    pub(crate) content: Vec<UserContent>,
    pub(crate) visible: bool,
}

#[derive(Clone, Debug)]
pub struct ArchivedSessionSummary {
    pub id: String,
    pub updated_at: DateTime<Utc>,
    pub cwd: PathBuf,
    pub provider: String,
    pub model: String,
    pub thinking: ThinkingLevel,
    pub first_message: String,
    pub message_count: usize,
}

#[derive(Clone, Debug)]
pub struct ArchivedSession {
    pub summary: ArchivedSessionSummary,
    pub events: Vec<SessionEvent>,
}

/// Read-only access to saved sessions for linked extensions.
///
/// Archive reads never initialize, migrate, or otherwise write the database.
#[derive(Clone, Debug)]
pub struct SessionArchive {
    database_path: PathBuf,
    project: PathBuf,
}

impl SessionArchive {
    pub fn for_project(cwd: &Path) -> Self {
        Self::at(session_database_path(cwd), cwd)
    }

    pub(crate) fn at(database_path: PathBuf, cwd: &Path) -> Self {
        Self {
            database_path,
            project: cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf()),
        }
    }

    pub async fn list_for_project(&self) -> Result<Vec<ArchivedSessionSummary>> {
        let project = self.project.clone();
        Ok(self
            .list_all()
            .await?
            .into_iter()
            .filter(|session| {
                if cfg!(windows) {
                    session
                        .cwd
                        .to_string_lossy()
                        .eq_ignore_ascii_case(&project.to_string_lossy())
                } else {
                    session.cwd == project
                }
            })
            .collect())
    }

    pub async fn list_all(&self) -> Result<Vec<ArchivedSessionSummary>> {
        let Some(connection) = open_archive_database(&self.database_path).await? else {
            return Ok(Vec::new());
        };
        connection
            .call(|db| {
                let mut statement = db.prepare(
                    "SELECT id, updated_at, cwd, provider, model, thinking,
                        (SELECT payload_json FROM events
                         WHERE events.session_id = sessions.id AND kind = 'user'
                         ORDER BY sequence ASC LIMIT 1),
                        (SELECT COUNT(*) FROM events
                         WHERE events.session_id = sessions.id AND kind = 'user')
                     FROM sessions
                     ORDER BY updated_at DESC, id DESC",
                )?;
                let rows = statement.query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, Option<String>>(6)?,
                        row.get::<_, i64>(7)?,
                    ))
                })?;
                let mut sessions = Vec::new();
                for row in rows {
                    let (id, updated_at, cwd, provider, model, thinking, payload, message_count) =
                        row?;
                    let first_message = payload
                        .and_then(|payload| serde_json::from_str::<EventKind>(&payload).ok())
                        .and_then(|kind| match kind {
                            EventKind::User { text } => Some(text),
                            _ => None,
                        })
                        .unwrap_or_default();
                    let updated_at = DateTime::parse_from_rfc3339(&updated_at)
                        .map(|value| value.with_timezone(&Utc))
                        .unwrap_or(DateTime::<Utc>::UNIX_EPOCH);
                    sessions.push(ArchivedSessionSummary {
                        id,
                        updated_at,
                        cwd: PathBuf::from(cwd),
                        provider,
                        model,
                        thinking: thinking.parse().unwrap_or_default(),
                        first_message,
                        message_count: usize::try_from(message_count).unwrap_or_default(),
                    });
                }
                Ok::<_, tokio_rusqlite::rusqlite::Error>(sessions)
            })
            .await
            .context("cannot list archived sessions")
    }

    pub async fn load(&self, id: &str) -> Result<Option<ArchivedSession>> {
        validate_session_id(id)?;
        let Some(connection) = open_archive_database(&self.database_path).await? else {
            return Ok(None);
        };
        let id = id.to_string();
        connection
            .call(move |db| {
                let summary = db
                    .query_row(
                        "SELECT id, updated_at, cwd, provider, model, thinking,
                            (SELECT payload_json FROM events
                             WHERE events.session_id = sessions.id AND kind = 'user'
                             ORDER BY sequence ASC LIMIT 1),
                            (SELECT COUNT(*) FROM events
                             WHERE events.session_id = sessions.id AND kind = 'user')
                         FROM sessions WHERE id = ?1",
                        [&id],
                        |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, String>(1)?,
                                row.get::<_, String>(2)?,
                                row.get::<_, String>(3)?,
                                row.get::<_, String>(4)?,
                                row.get::<_, String>(5)?,
                                row.get::<_, Option<String>>(6)?,
                                row.get::<_, i64>(7)?,
                            ))
                        },
                    )
                    .optional()?;
                let Some((id, updated_at, cwd, provider, model, thinking, payload, message_count)) =
                    summary
                else {
                    return Ok::<_, tokio_rusqlite::rusqlite::Error>(None);
                };
                let mut statement = db.prepare(
                    "SELECT sequence, at, payload_json FROM events
                     WHERE session_id = ?1 ORDER BY sequence",
                )?;
                let rows = statement.query_map([&id], |row| {
                    let sequence = row.get::<_, i64>(0)?;
                    let at = row.get::<_, String>(1)?;
                    let payload = row.get::<_, String>(2)?;
                    let at = DateTime::parse_from_rfc3339(&at)
                        .map_err(|error| {
                            tokio_rusqlite::rusqlite::Error::FromSqlConversionFailure(
                                1,
                                tokio_rusqlite::rusqlite::types::Type::Text,
                                Box::new(error),
                            )
                        })?
                        .with_timezone(&Utc);
                    let kind = serde_json::from_str(&payload).map_err(|error| {
                        tokio_rusqlite::rusqlite::Error::FromSqlConversionFailure(
                            2,
                            tokio_rusqlite::rusqlite::types::Type::Text,
                            Box::new(error),
                        )
                    })?;
                    Ok(SessionEvent {
                        sequence: sequence as u64,
                        at,
                        kind,
                    })
                })?;
                let events = rows.collect::<Result<Vec<_>, _>>()?;
                let first_message = payload
                    .and_then(|payload| serde_json::from_str::<EventKind>(&payload).ok())
                    .and_then(|kind| match kind {
                        EventKind::User { text } => Some(text),
                        _ => None,
                    })
                    .unwrap_or_default();
                let updated_at = DateTime::parse_from_rfc3339(&updated_at)
                    .map(|value| value.with_timezone(&Utc))
                    .unwrap_or(DateTime::<Utc>::UNIX_EPOCH);
                Ok(Some(ArchivedSession {
                    summary: ArchivedSessionSummary {
                        id,
                        updated_at,
                        cwd: PathBuf::from(cwd),
                        provider,
                        model,
                        thinking: thinking.parse().unwrap_or_default(),
                        first_message,
                        message_count: usize::try_from(message_count).unwrap_or_default(),
                    },
                    events,
                }))
            })
            .await
            .context("cannot read archived session")
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct SessionModelSettings {
    pub provider: String,
    pub model: String,
    pub thinking: ThinkingLevel,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct SessionEvent {
    pub sequence: u64,
    pub at: DateTime<Utc>,
    #[serde(flatten)]
    pub kind: EventKind,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct SessionContext {
    pub system_prompt: String,
    pub skills: Vec<SkillSnapshot>,
}

#[derive(Clone, Debug)]
pub enum SessionUpdate {
    Persisted(SessionEvent),
    Transient(EventKind),
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EventKind {
    SessionCreated {
        spec: AgentSpec,
    },
    SessionContext {
        context: SessionContext,
    },
    User {
        text: String,
    },
    AssistantText {
        text: String,
    },
    AssistantReasoning {
        text: String,
    },
    /// Provisional tool-call name or argument text used for live output-rate
    /// accounting. The completed `ToolCall` remains the durable transcript.
    AssistantToolCallDelta {
        text: String,
    },
    ToolCall {
        call_id: String,
        name: String,
        arguments: Value,
    },
    ToolResult {
        call_id: String,
        name: String,
        output: String,
        failed: bool,
        #[serde(default)]
        protocol_help_required: bool,
    },
    ModelMessage {
        message: Message,
    },
    AgentSpecUpdated {
        spec: AgentSpec,
        context: SessionContext,
    },
    Task {
        id: String,
        protocol: String,
        label: String,
        status: TaskStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output: Option<String>,
    },
    Notice {
        text: String,
    },
    ModelRetry {
        attempt: usize,
        max_retries: usize,
        delay_ms: u64,
        reason: String,
    },
    /// Token usage and USD cost reported by one model response.
    Usage {
        input: u64,
        output: u64,
        /// Output tokens spent on reasoning, when reported separately. This
        /// is a subset of `output` after provider normalization.
        #[serde(default)]
        reasoning: u64,
        cache_read: u64,
        cache_write: u64,
        cost: f64,
        /// Provider-reported total tokens for the completed request, before
        /// any API-specific normalization used for price accounting.
        total: u64,
        /// Whether this usage belongs to a successful ordinary assistant
        /// message and is therefore valid as a context-meter baseline.
        context: bool,
        provider: String,
        model: String,
    },
    Error {
        text: String,
    },
    Compaction {
        summary: String,
        tokens_before: usize,
        replacement_history: Vec<Message>,
        manual: bool,
    },
    TurnFinished,
}

struct State {
    events: Vec<SessionEvent>,
    persisted: bool,
    head_sequence: Option<u64>,
    spec: AgentSpec,
    context: Option<SessionContext>,
    derived: ResumeState,
    replay: ReplayState,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
struct ResumeState {
    through_sequence: Option<u64>,
    context_sequence: Option<u64>,
    #[serde(default)]
    latest_compaction_sequence: Option<u64>,
    spec: Option<AgentSpec>,
    has_user: bool,
    successful_help_reads: HashSet<String>,
    pending_help_reads: HashMap<String, String>,
    tasks: HashMap<String, TaskPointers>,
    usage: UsageTotals,
    token_calibration: TokenCalibration,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
struct TaskPointers {
    first_sequence: u64,
    latest_sequence: u64,
    output_sequence: Option<u64>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
struct UsageTotals {
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
    cost: f64,
    last_cache_hit: Option<f64>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
struct TokenCalibration {
    visible_units: u64,
    output_tokens: u64,
    pending_visible_units: u64,
    pending_reasoning_visible: bool,
    pending_usage: Option<(u64, u64)>,
}

#[derive(Clone, Debug, Default)]
struct ReplayState {
    history: Vec<Message>,
    usage_before_assistant: Vec<Option<UsageBaseline>>,
    pending_usage: Option<UsageBaseline>,
    after_compaction: bool,
    latest_compaction_summary: Option<String>,
}

#[derive(Clone, Debug)]
struct UsageBaseline {
    total: usize,
    context: bool,
    provider: String,
    model: String,
}

struct RestoredState {
    context: Option<SessionContext>,
    derived: ResumeState,
    replay: ReplayState,
    tail: Vec<SessionEvent>,
}

enum EventPage {
    All,
    After(u64, usize),
    Before(u64, usize),
    Tail(usize),
}

fn restore_persisted_state(
    db: &mut SqliteConnection,
    id: &str,
    authoritative_head: u64,
) -> tokio_rusqlite::rusqlite::Result<RestoredState> {
    let context_event = db
        .query_row(
            "SELECT sequence, payload_json FROM events
             WHERE session_id = ?1
               AND kind IN ('session_context', 'agent_spec_updated')
             ORDER BY sequence DESC LIMIT 1",
            [id],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;
    let context_sequence = context_event
        .as_ref()
        .and_then(|(sequence, _)| u64::try_from(*sequence).ok());
    let context = context_event
        .map(|(_, payload)| {
            let kind = serde_json::from_str::<EventKind>(&payload).map_err(|error| {
                tokio_rusqlite::rusqlite::Error::FromSqlConversionFailure(
                    1,
                    tokio_rusqlite::rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })?;
            match kind {
                EventKind::SessionContext { context }
                | EventKind::AgentSpecUpdated { context, .. } => Ok(context),
                _ => Err(tokio_rusqlite::rusqlite::Error::InvalidQuery),
            }
        })
        .transpose()?;
    let has_creation = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM events
         WHERE session_id = ?1 AND kind = 'session_created')",
        [id],
        |row| row.get::<_, bool>(0),
    )?;
    if !has_creation {
        return Err(tokio_rusqlite::rusqlite::Error::InvalidQuery);
    }

    let latest_compaction = db
        .query_row(
            "SELECT sequence, at, payload_json FROM events
             WHERE session_id = ?1 AND kind = 'compaction'
             ORDER BY sequence DESC LIMIT 1",
            [id],
            stored_event_from_row,
        )
        .optional()?;
    let checkpoint = db
        .query_row(
            "SELECT version, through_sequence, payload_json, checksum
             FROM session_resume_index WHERE session_id = ?1",
            [id],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )
        .optional()
        .ok()
        .flatten()
        .and_then(|(version, through, payload, checksum)| {
            let through = u64::try_from(through).ok()?;
            if version != i64::from(RESUME_INDEX_VERSION) || through > authoritative_head {
                return None;
            }
            if resume_checksum(id, through, &payload) != checksum {
                return None;
            }
            let mut state = serde_json::from_str::<ResumeState>(&payload).ok()?;
            if state.through_sequence != Some(through) {
                return None;
            }
            let is_compaction = db
                .query_row(
                    "SELECT kind = 'compaction' FROM events
                     WHERE session_id = ?1 AND sequence = ?2",
                    params![id, through as i64],
                    |row| row.get::<_, bool>(0),
                )
                .optional()
                .ok()
                .flatten()
                .unwrap_or(false);
            if is_compaction {
                state.latest_compaction_sequence = Some(through);
            }
            is_compaction.then_some((through, std::mem::take(&mut state)))
        });

    let (mut derived, cursor) = if let Some((through, state)) = checkpoint {
        (state, Some(through))
    } else if let Some(compaction) = &latest_compaction {
        let mut state = ResumeState::default();
        for event in query_events(
            db,
            id,
            None,
            Some(compaction.sequence),
            Some(RESUME_EVENT_KINDS),
        )? {
            apply_resume_event(&mut state, &event);
        }
        state.context_sequence = context_sequence;
        if let Ok(payload) = serde_json::to_string(&state) {
            let _ = persist_rebuilt_resume_index(db, id, compaction.sequence, &payload);
        }
        (state, Some(compaction.sequence))
    } else {
        (ResumeState::default(), None)
    };

    let tail = query_events(db, id, cursor, None, Some(RESUME_EVENT_KINDS))?;
    for event in &tail {
        apply_resume_event(&mut derived, event);
    }
    derived.context_sequence = derived.context_sequence.or(context_sequence);
    derived.through_sequence = Some(authoritative_head);

    let mut replay = ReplayState::default();
    let replay_cursor = latest_compaction.as_ref().map(|event| event.sequence);
    if let Some(compaction) = latest_compaction {
        apply_replay_event(&mut replay, &compaction.kind);
    }
    for event in query_events(
        db,
        id,
        replay_cursor,
        None,
        Some(&["usage", "model_message"]),
    )? {
        apply_replay_event(&mut replay, &event.kind);
    }

    Ok(RestoredState {
        context,
        derived,
        replay,
        tail,
    })
}

fn query_events(
    db: &SqliteConnection,
    id: &str,
    after: Option<u64>,
    through: Option<u64>,
    kinds: Option<&[&str]>,
) -> tokio_rusqlite::rusqlite::Result<Vec<SessionEvent>> {
    let after = after.map_or(-1, |sequence| sequence as i64);
    let through = through.map_or(i64::MAX, |sequence| sequence as i64);
    let mut statement = db.prepare(
        "SELECT sequence, at, payload_json FROM events
         WHERE session_id = ?1 AND sequence > ?2 AND sequence <= ?3
           AND (?4 = '' OR instr(',' || ?4 || ',', ',' || kind || ',') > 0)
         ORDER BY sequence",
    )?;
    let kinds = kinds.map_or_else(String::new, |kinds| kinds.join(","));
    statement
        .query_map(params![id, after, through, kinds], stored_event_from_row)?
        .collect()
}

fn stored_event_from_row(
    row: &tokio_rusqlite::rusqlite::Row<'_>,
) -> tokio_rusqlite::rusqlite::Result<SessionEvent> {
    let sequence = row.get::<_, i64>(0)?;
    let at = row.get::<_, String>(1)?;
    let payload = row.get::<_, String>(2)?;
    let at = DateTime::parse_from_rfc3339(&at)
        .map_err(|error| {
            tokio_rusqlite::rusqlite::Error::FromSqlConversionFailure(
                1,
                tokio_rusqlite::rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?
        .with_timezone(&Utc);
    let kind = serde_json::from_str(&payload).map_err(|error| {
        tokio_rusqlite::rusqlite::Error::FromSqlConversionFailure(
            2,
            tokio_rusqlite::rusqlite::types::Type::Text,
            Box::new(error),
        )
    })?;
    Ok(SessionEvent {
        sequence: sequence as u64,
        at,
        kind,
    })
}

fn resume_checksum(session_id: &str, through: u64, payload: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(RESUME_INDEX_VERSION.to_be_bytes());
    digest.update((session_id.len() as u64).to_be_bytes());
    digest.update(session_id.as_bytes());
    digest.update(through.to_be_bytes());
    digest.update(payload.as_bytes());
    format!("{:x}", digest.finalize())
}

fn persist_rebuilt_resume_index(
    db: &SqliteConnection,
    session_id: &str,
    through: u64,
    payload: &str,
) -> tokio_rusqlite::rusqlite::Result<()> {
    let checksum = resume_checksum(session_id, through, payload);
    db.execute(
        "INSERT INTO session_resume_index
         (session_id, version, through_sequence, payload_json, checksum)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(session_id) DO UPDATE SET
           version = excluded.version,
           through_sequence = excluded.through_sequence,
           payload_json = excluded.payload_json,
           checksum = excluded.checksum
         WHERE excluded.through_sequence >= session_resume_index.through_sequence",
        params![
            session_id,
            i64::from(RESUME_INDEX_VERSION),
            through as i64,
            payload,
            checksum
        ],
    )?;
    Ok(())
}

#[derive(Clone, Debug)]
pub(crate) struct ModelContext {
    pub history: Vec<Message>,
    pub latest_api_usage: Option<(usize, usize)>,
    pub after_compaction: bool,
}

#[derive(Clone)]
pub struct Session {
    id: String,
    created: bool,
    project: String,
    project_directory: PathBuf,
    directory: PathBuf,
    database_path: PathBuf,
    connection: Connection,
    state: Arc<Mutex<State>>,
    events: broadcast::Sender<SessionUpdate>,
    #[cfg(test)]
    event_read_audit: Arc<std::sync::Mutex<SessionEventReadAudit>>,
}

#[cfg(test)]
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct SessionEventReadAudit {
    pub(crate) payload_pages: Vec<Vec<u64>>,
    pub(crate) header_pages: Vec<Vec<u64>>,
}

impl Session {
    pub async fn persisted_spec(cwd: &Path, id: &str) -> Result<Option<AgentSpec>> {
        validate_session_id(id)?;
        let project = cwd
            .canonicalize()
            .unwrap_or_else(|_| cwd.to_path_buf())
            .to_string_lossy()
            .into_owned();
        let (_, connection) = open_database(session_database_path(cwd)).await?;
        let id = id.to_string();
        connection
            .call(move |db| {
                let payload = db
                    .query_row(
                        "SELECT events.payload_json
                         FROM sessions JOIN events ON events.session_id = sessions.id
                         WHERE sessions.id = ?1 AND sessions.cwd = ?2
                           AND events.kind = 'session_created'
                         ORDER BY events.sequence LIMIT 1",
                        params![id, project],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()?;
                payload
                    .map(|payload| {
                        let kind =
                            serde_json::from_str::<EventKind>(&payload).map_err(|error| {
                                tokio_rusqlite::rusqlite::Error::FromSqlConversionFailure(
                                    0,
                                    tokio_rusqlite::rusqlite::types::Type::Text,
                                    Box::new(error),
                                )
                            })?;
                        match kind {
                            EventKind::SessionCreated { spec } => Ok(spec),
                            _ => Err(tokio_rusqlite::rusqlite::Error::InvalidQuery),
                        }
                    })
                    .transpose()
            })
            .await
            .context("cannot read persisted Agent spec")
    }

    pub async fn open(
        requested: Option<&str>,
        cwd: &Path,
        spec: AgentSpec,
        context: SessionContext,
    ) -> Result<Self> {
        Self::open_at_with_spec(
            session_database_path(cwd),
            requested,
            cwd,
            spec,
            Some(context),
        )
        .await
    }

    pub async fn open_deferred(
        requested: Option<&str>,
        cwd: &Path,
        spec: AgentSpec,
    ) -> Result<Self> {
        Self::open_at_with_spec(session_database_path(cwd), requested, cwd, spec, None).await
    }

    #[cfg(test)]
    pub(crate) async fn open_at(
        database_path: PathBuf,
        requested: Option<&str>,
        cwd: &Path,
        provider: &str,
        model: &str,
        context: SessionContext,
    ) -> Result<Self> {
        Self::open_at_with_thinking(
            database_path,
            requested,
            cwd,
            provider,
            model,
            ThinkingLevel::default(),
            Some(context),
        )
        .await
    }

    #[cfg(test)]
    pub(crate) async fn open_at_deferred(
        database_path: PathBuf,
        requested: Option<&str>,
        cwd: &Path,
        provider: &str,
        model: &str,
    ) -> Result<Self> {
        Self::open_at_with_thinking(
            database_path,
            requested,
            cwd,
            provider,
            model,
            ThinkingLevel::default(),
            None,
        )
        .await
    }

    #[cfg(test)]
    async fn open_at_with_thinking(
        database_path: PathBuf,
        requested: Option<&str>,
        cwd: &Path,
        provider: &str,
        model: &str,
        thinking: ThinkingLevel,
        context: Option<SessionContext>,
    ) -> Result<Self> {
        Self::open_at_with_spec(
            database_path,
            requested,
            cwd,
            AgentSpec::root(provider, model, thinking, cwd),
            context,
        )
        .await
    }

    async fn open_at_with_spec(
        database_path: PathBuf,
        requested: Option<&str>,
        cwd: &Path,
        mut spec: AgentSpec,
        context: Option<SessionContext>,
    ) -> Result<Self> {
        let (directory, connection) = open_database(database_path.clone()).await?;
        let project_directory = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
        let project = project_directory.to_string_lossy().into_owned();

        if let Some(id) = requested.filter(|id| *id != "latest") {
            validate_session_id(id)?;
        }
        let requested = requested.map(str::to_owned);
        let project_for_selection = project.clone();
        let id = connection
            .call(move |db| {
                if requested.as_deref() == Some("latest") {
                    Ok::<_, tokio_rusqlite::rusqlite::Error>(
                        db.query_row(
                            "SELECT id FROM sessions WHERE cwd = ?1 AND depth = 1
                             ORDER BY updated_at DESC, id DESC LIMIT 1",
                            [project_for_selection],
                            |row| row.get(0),
                        )
                        .optional()?
                        .unwrap_or_else(new_session_id),
                    )
                } else if let Some(id) = requested {
                    Ok::<_, tokio_rusqlite::rusqlite::Error>(id)
                } else {
                    Ok::<_, tokio_rusqlite::rusqlite::Error>(new_session_id())
                }
            })
            .await
            .context("cannot select session")?;

        let project_for_lookup = project.clone();
        let id_for_lookup = id.clone();
        let stored_session = connection
            .call(move |db| {
                db.query_row(
                    "SELECT cwd = ?2, head_sequence,
                       EXISTS(SELECT 1 FROM events
                         WHERE events.session_id = sessions.id AND kind = 'session_created')
                     FROM sessions WHERE id = ?1",
                    params![id_for_lookup, project_for_lookup],
                    |row| {
                        Ok((
                            row.get::<_, bool>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, bool>(2)?,
                        ))
                    },
                )
                .optional()
            })
            .await
            .context("cannot validate session project")?;
        if stored_session.as_ref().is_some_and(|stored| !stored.0) {
            return Err(anyhow!("session {id} belongs to a different project"));
        }
        if stored_session.as_ref().is_some_and(|stored| !stored.2) {
            return Err(anyhow!(
                "session {id} has no creation event and cannot be resumed"
            ));
        }
        let created_session = stored_session.is_none();
        let mut existing = Vec::new();
        let mut derived = ResumeState::default();
        let mut replay = ReplayState::default();
        let mut frozen_context = None;
        let mut head_sequence = None;
        let mut restored_spec = None;

        if created_session {
            spec.working_directory.clone_from(&project_directory);
            let at = Utc::now();
            let created = EventKind::SessionCreated { spec: spec.clone() };
            existing.push(SessionEvent {
                sequence: 0,
                at,
                kind: created,
            });
            if let Some(context) = context {
                existing.push(SessionEvent {
                    sequence: 1,
                    at,
                    kind: EventKind::SessionContext { context },
                });
            }
            for event in &existing {
                apply_resume_event(&mut derived, event);
                apply_replay_event(&mut replay, &event.kind);
                if let EventKind::SessionContext { context } = &event.kind {
                    frozen_context = Some(context.clone());
                }
            }
            restored_spec = agent_spec_from_events(&existing);
            head_sequence = existing.last().map(|event| event.sequence);
        } else if let Some((_, stored_head, _)) = stored_session {
            let authoritative_head = u64::try_from(stored_head)
                .map_err(|_| anyhow!("session {id} has an invalid event head"))?;
            head_sequence = Some(authoritative_head);
            let lookup_id = id.clone();
            let restored = connection
                .call(move |db| restore_persisted_state(db, &lookup_id, authoritative_head))
                .await
                .context("cannot restore session state")?;
            frozen_context = restored.context;
            restored_spec = restored.derived.spec.clone();
            derived = restored.derived;
            replay = restored.replay;
            existing = restored.tail;
        }
        let spec = restored_spec
            .ok_or_else(|| anyhow!("session {id} has no creation event and cannot be resumed"))?;
        if !created_session && frozen_context.is_none() {
            return Err(anyhow!(
                "session {id} has no frozen context and cannot be resumed"
            ));
        }
        let (events, _) = broadcast::channel(512);
        let session = Self {
            id,
            created: created_session,
            project,
            project_directory,
            directory,
            database_path,
            connection,
            state: Arc::new(Mutex::new(State {
                events: existing,
                persisted: !created_session,
                head_sequence,
                spec,
                context: frozen_context,
                derived,
                replay,
            })),
            events,
            #[cfg(test)]
            event_read_audit: Arc::default(),
        };
        Ok(session)
    }

    pub fn id(&self) -> &str {
        &self.id
    }
    pub fn is_new(&self) -> bool {
        self.created
    }

    pub async fn is_persisted(&self) -> bool {
        self.state.lock().await.persisted
    }

    pub(crate) async fn add_pending_input(
        &self,
        kind: SubmitKind,
        text: &str,
        content: &[UserContent],
        visible: bool,
    ) -> Result<i64> {
        let session_id = self.id.clone();
        let kind = match kind {
            SubmitKind::Prompt => "prompt",
            SubmitKind::Steer => "steer",
        };
        let text = text.to_string();
        let content = serde_json::to_string(content).context("cannot serialize pending input")?;
        let created_at = Utc::now().to_rfc3339();
        let mut state = self.state.lock().await;
        if state.persisted {
            drop(state);
            return self
                .connection
                .call(move |db| {
                    db.execute(
                        "INSERT INTO pending_inputs
                         (session_id, kind, text, content_json, visible, created_at)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                        params![session_id, kind, text, content, visible, created_at],
                    )?;
                    Ok::<_, tokio_rusqlite::rusqlite::Error>(db.last_insert_rowid())
                })
                .await
                .context("cannot persist pending Agent input");
        }
        if state.context.is_none() {
            bail!("cannot persist a session before its startup context is ready");
        }

        let project = self.project.clone();
        let spec = state.spec.clone();
        let head_sequence = state.head_sequence.map_or(-1, |sequence| sequence as i64);
        let stored_events = state
            .events
            .iter()
            .map(|event| {
                Ok::<_, anyhow::Error>((
                    event.sequence as i64,
                    event.at.to_rfc3339(),
                    payload_kind(&event.kind).to_string(),
                    serde_json::to_string(&event.kind).context("cannot serialize session event")?,
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        let pending_id = self
            .connection
            .call(move |db| {
                let transaction = db.transaction()?;
                let draft = transaction
                    .query_row(
                        "SELECT draft FROM pending_drafts WHERE cwd = ?1",
                        [&project],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()?
                    .unwrap_or_default();
                transaction.execute(
                    "INSERT INTO sessions
                     (id, created_at, updated_at, cwd, provider, model, thinking,
                      parent_session_id, depth, head_sequence, draft)
                     VALUES (?1, ?2, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                    params![
                        session_id,
                        created_at,
                        project,
                        spec.provider,
                        spec.model,
                        spec.thinking.to_string(),
                        spec.parent_session_id,
                        i64::from(spec.depth()),
                        head_sequence,
                        draft,
                    ],
                )?;
                for (sequence, event_at, event_kind, payload) in stored_events {
                    transaction.execute(
                        "INSERT INTO events
                         (session_id, sequence, at, kind, payload_json)
                         VALUES (?1, ?2, ?3, ?4, ?5)",
                        params![session_id, sequence, event_at, event_kind, payload],
                    )?;
                }
                transaction.execute(
                    "INSERT INTO pending_inputs
                     (session_id, kind, text, content_json, visible, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![session_id, kind, text, content, visible, created_at],
                )?;
                let pending_id = transaction.last_insert_rowid();
                transaction.execute("DELETE FROM pending_drafts WHERE cwd = ?1", [project])?;
                transaction.commit()?;
                Ok::<_, tokio_rusqlite::rusqlite::Error>(pending_id)
            })
            .await
            .context("cannot persist pending Agent input")?;
        state.persisted = true;
        Ok(pending_id)
    }

    pub(crate) async fn pending_inputs(&self) -> Result<Vec<PendingInput>> {
        if !self.is_persisted().await {
            return Ok(Vec::new());
        }
        let session_id = self.id.clone();
        self.connection
            .call(move |db| {
                let mut statement = db.prepare(
                    "SELECT id, kind, text, content_json, visible
                     FROM pending_inputs WHERE session_id = ?1 ORDER BY id",
                )?;
                statement
                    .query_map([session_id], |row| {
                        let kind = match row.get::<_, String>(1)?.as_str() {
                            "prompt" => SubmitKind::Prompt,
                            "steer" => SubmitKind::Steer,
                            _ => return Err(tokio_rusqlite::rusqlite::Error::InvalidQuery),
                        };
                        let content =
                            serde_json::from_str(&row.get::<_, String>(3)?).map_err(|error| {
                                tokio_rusqlite::rusqlite::Error::FromSqlConversionFailure(
                                    3,
                                    tokio_rusqlite::rusqlite::types::Type::Text,
                                    Box::new(error),
                                )
                            })?;
                        Ok(PendingInput {
                            id: row.get(0)?,
                            kind,
                            text: row.get(2)?,
                            content,
                            visible: row.get(4)?,
                        })
                    })?
                    .collect::<Result<Vec<_>, _>>()
            })
            .await
            .context("cannot restore pending Agent inputs")
    }

    pub(crate) async fn remove_pending_input(&self, pending_id: i64) -> Result<bool> {
        let session_id = self.id.clone();
        self.connection
            .call(move |db| {
                Ok::<_, tokio_rusqlite::rusqlite::Error>(
                    db.execute(
                        "DELETE FROM pending_inputs WHERE session_id = ?1 AND id = ?2",
                        params![session_id, pending_id],
                    )? > 0,
                )
            })
            .await
            .context("cannot remove pending Agent input")
    }

    pub(crate) async fn update_pending_input_kind(
        &self,
        pending_id: i64,
        kind: SubmitKind,
    ) -> Result<bool> {
        let session_id = self.id.clone();
        let kind = match kind {
            SubmitKind::Prompt => "prompt",
            SubmitKind::Steer => "steer",
        };
        self.connection
            .call(move |db| {
                Ok::<_, tokio_rusqlite::rusqlite::Error>(
                    db.execute(
                        "UPDATE pending_inputs SET kind = ?3
                         WHERE session_id = ?1 AND id = ?2",
                        params![session_id, pending_id, kind],
                    )? > 0,
                )
            })
            .await
            .context("cannot update pending Agent input")
    }

    pub async fn initialize_context(&self, context: SessionContext) -> Result<()> {
        let mut state = self.state.lock().await;
        if state.context.is_some() {
            return Ok(());
        }
        if state.persisted {
            bail!("cannot initialize missing context for a persisted session");
        }
        let event = SessionEvent {
            sequence: state
                .head_sequence
                .map_or(0, |sequence| sequence.saturating_add(1)),
            at: Utc::now(),
            kind: EventKind::SessionContext { context },
        };
        if let EventKind::SessionContext { context } = &event.kind {
            state.context = Some(context.clone());
        }
        state.head_sequence = Some(event.sequence);
        apply_resume_event(&mut state.derived, &event);
        apply_replay_event(&mut state.replay, &event.kind);
        state.events.push(event.clone());
        drop(state);
        self.publish_persisted(&[event]);
        Ok(())
    }
    pub fn project_directory(&self) -> &Path {
        &self.project_directory
    }
    pub fn directory(&self) -> &Path {
        &self.directory
    }
    pub fn database_path(&self) -> &Path {
        &self.database_path
    }
    pub async fn draft(&self) -> String {
        let persisted = self.state.lock().await.persisted;
        let key = if persisted {
            self.id.clone()
        } else {
            self.project.clone()
        };
        self.connection
            .call(move |db| {
                let query = if persisted {
                    "SELECT draft FROM sessions WHERE id = ?1"
                } else {
                    "SELECT draft FROM pending_drafts WHERE cwd = ?1"
                };
                db.query_row(query, [key], |row| row.get(0)).optional()
            })
            .await
            .ok()
            .flatten()
            .unwrap_or_default()
    }

    pub async fn save_draft(&self, text: &str) -> Result<()> {
        let persisted = self.state.lock().await.persisted;
        let key = if persisted {
            self.id.clone()
        } else {
            self.project.clone()
        };
        let text = text.to_string();
        self.connection
            .call(move |db| {
                if persisted {
                    db.execute(
                        "UPDATE sessions SET draft = ?2 WHERE id = ?1",
                        params![key, text],
                    )?;
                } else if text.is_empty() {
                    db.execute("DELETE FROM pending_drafts WHERE cwd = ?1", [key])?;
                } else {
                    db.execute(
                        "INSERT INTO pending_drafts (cwd, draft) VALUES (?1, ?2)
                         ON CONFLICT(cwd) DO UPDATE SET draft = excluded.draft",
                        params![key, text],
                    )?;
                }
                Ok::<_, tokio_rusqlite::rusqlite::Error>(())
            })
            .await
            .context("cannot save draft")
    }

    pub async fn list_for_project(&self) -> Result<Vec<SessionSummary>> {
        list_project_sessions(self.database_path.clone(), Path::new(&self.project)).await
    }

    pub fn subscribe(&self) -> broadcast::Receiver<SessionUpdate> {
        self.events.subscribe()
    }

    /// Publish provisional UI state without adding it to durable replay. The
    /// completed response boundary later replaces these deltas in the transcript.
    pub fn publish_transient(&self, kind: EventKind) {
        let _ = self.events.send(SessionUpdate::Transient(kind));
    }

    pub async fn snapshot(&self) -> Result<Vec<SessionEvent>> {
        let state = self.state.lock().await;
        if !state.persisted {
            return Ok(state.events.clone());
        }
        drop(state);
        self.query_event_page(EventPage::All).await
    }

    pub(crate) async fn tui_state(&self) -> SessionTuiState {
        let state = self.state.lock().await;
        SessionTuiState {
            head_sequence: state.head_sequence,
            latest_compaction_sequence: state.derived.latest_compaction_sequence,
            usage: SessionTuiUsage {
                input: state.derived.usage.input,
                output: state.derived.usage.output,
                cache_read: state.derived.usage.cache_read,
                cache_write: state.derived.usage.cache_write,
                cost: state.derived.usage.cost,
                last_cache_hit: state.derived.usage.last_cache_hit,
            },
            token_calibration: SessionTuiTokenCalibration {
                visible_units: state.derived.token_calibration.visible_units,
                output_tokens: state.derived.token_calibration.output_tokens,
            },
        }
    }

    pub async fn task_reports(&self) -> Result<Vec<TaskReport>> {
        let (persisted, tasks, in_memory) = {
            let state = self.state.lock().await;
            (
                state.persisted,
                state.derived.tasks.clone(),
                state.events.clone(),
            )
        };
        if !persisted {
            return Ok(task_reports_from_events(&in_memory));
        }
        let id = self.id.clone();
        self.connection
            .call(move |db| {
                let mut events = HashMap::<u64, SessionEvent>::new();
                let mut statement = db.prepare(
                    "SELECT sequence, at, payload_json FROM events
                     WHERE session_id = ?1 AND sequence = ?2",
                )?;
                let mut valid = true;
                for (task_id, pointers) in &tasks {
                    let expected = [
                        Some(pointers.first_sequence),
                        Some(pointers.latest_sequence),
                        pointers.output_sequence,
                    ];
                    for sequence in expected.into_iter().flatten() {
                        let event = if let Some(event) = events.get(&sequence) {
                            Some(event.clone())
                        } else {
                            statement
                                .query_row(params![id, sequence as i64], stored_event_from_row)
                                .optional()?
                        };
                        let Some(event) = event else {
                            valid = false;
                            continue;
                        };
                        if !matches!(&event.kind, EventKind::Task { id, .. } if id == task_id) {
                            valid = false;
                        }
                        events.insert(sequence, event);
                    }
                }
                if !valid {
                    let mut statement = db.prepare(
                        "SELECT sequence, at, payload_json FROM events
                         WHERE session_id = ?1 AND kind = 'task' ORDER BY sequence",
                    )?;
                    let events = statement
                        .query_map([id], stored_event_from_row)?
                        .collect::<Result<Vec<_>, _>>()?;
                    return Ok::<_, tokio_rusqlite::rusqlite::Error>(task_reports_from_events(
                        &events,
                    ));
                }
                let mut events = events.into_values().collect::<Vec<_>>();
                events.sort_by_key(|event| event.sequence);
                Ok(task_reports_from_events(&events))
            })
            .await
            .context("cannot restore session task reports")
    }

    pub async fn model_settings(&self) -> SessionModelSettings {
        let spec = self.state.lock().await.spec.clone();
        SessionModelSettings {
            provider: spec.provider,
            model: spec.model,
            thinking: spec.thinking,
        }
    }

    pub async fn spec(&self) -> AgentSpec {
        self.state.lock().await.spec.clone()
    }

    pub async fn context(&self) -> SessionContext {
        self.state
            .lock()
            .await
            .context
            .clone()
            .expect("session context is validated when the session opens")
    }

    pub async fn has_user_message(&self) -> bool {
        self.state.lock().await.derived.has_user
    }

    pub async fn successful_protocol_help_reads(&self) -> HashSet<String> {
        self.state
            .lock()
            .await
            .derived
            .successful_help_reads
            .clone()
    }

    pub async fn events_after(&self, sequence: u64, limit: usize) -> Result<Vec<SessionEvent>> {
        if sequence > i64::MAX as u64 {
            return Ok(Vec::new());
        }
        self.query_event_page(EventPage::After(sequence, limit.min(MAX_EVENT_PAGE)))
            .await
    }

    pub async fn events_before(&self, sequence: u64, limit: usize) -> Result<Vec<SessionEvent>> {
        if sequence > i64::MAX as u64 {
            return self.tail_events(limit).await;
        }
        self.query_event_page(EventPage::Before(sequence, limit.min(MAX_EVENT_PAGE)))
            .await
    }

    pub async fn tail_events(&self, limit: usize) -> Result<Vec<SessionEvent>> {
        self.query_event_page(EventPage::Tail(limit.min(MAX_EVENT_PAGE)))
            .await
    }

    pub(crate) async fn event_headers_before(
        &self,
        sequence: u64,
        limit: usize,
    ) -> Result<Vec<SessionEventHeader>> {
        let sequence = sequence.min(i64::MAX as u64);
        let limit = limit.min(MAX_EVENT_PAGE);
        let state = self.state.lock().await;
        if !state.persisted {
            let mut headers = state
                .events
                .iter()
                .rev()
                .filter(|event| event.sequence < sequence)
                .take(limit)
                .map(event_header)
                .collect::<Vec<_>>();
            headers.reverse();
            #[cfg(test)]
            self.record_header_page(&headers);
            return Ok(headers);
        }
        drop(state);

        let id = self.id.clone();
        let headers = self
            .connection
            .call(move |db| {
                let mut statement = db.prepare(
                    "SELECT sequence, kind FROM (
                       SELECT sequence, kind FROM events
                       WHERE session_id = ?1 AND sequence < ?2
                       ORDER BY sequence DESC LIMIT ?3
                     ) ORDER BY sequence",
                )?;
                statement
                    .query_map(params![id, sequence as i64, limit as i64], |row| {
                        let sequence = row.get::<_, i64>(0)?;
                        let kind = row.get::<_, String>(1)?;
                        Ok(SessionEventHeader {
                            sequence: sequence as u64,
                            starts_turn: kind == "user",
                            finishes_turn: kind == "turn_finished",
                        })
                    })?
                    .collect::<Result<Vec<_>, _>>()
            })
            .await
            .context("cannot read session event headers")?;
        #[cfg(test)]
        self.record_header_page(&headers);
        Ok(headers)
    }

    #[cfg(test)]
    pub(crate) fn reset_event_read_audit(&self) {
        *self.event_read_audit.lock().unwrap() = SessionEventReadAudit::default();
    }

    #[cfg(test)]
    pub(crate) fn event_read_audit(&self) -> SessionEventReadAudit {
        self.event_read_audit.lock().unwrap().clone()
    }

    #[cfg(test)]
    fn record_header_page(&self, headers: &[SessionEventHeader]) {
        self.event_read_audit
            .lock()
            .unwrap()
            .header_pages
            .push(headers.iter().map(|header| header.sequence).collect());
    }

    pub async fn model_history(&self) -> Vec<Message> {
        self.model_context("", "").await.history
    }

    pub(crate) async fn model_context(&self, provider: &str, model: &str) -> ModelContext {
        let state = self.state.lock().await;
        let latest_api_usage = state
            .replay
            .usage_before_assistant
            .iter()
            .enumerate()
            .filter_map(|(index, usage)| {
                let usage = usage.as_ref()?;
                (usage.context
                    && usage.total > 0
                    && (provider.is_empty()
                        || usage.provider.is_empty()
                        || usage.provider == provider)
                    && (model.is_empty() || usage.model.is_empty() || usage.model == model))
                    .then_some((index, usage.total))
            })
            .next_back();
        ModelContext {
            history: state.replay.history.clone(),
            latest_api_usage,
            after_compaction: state.replay.after_compaction,
        }
    }

    pub async fn latest_compaction_summary(&self) -> Option<String> {
        self.state
            .lock()
            .await
            .replay
            .latest_compaction_summary
            .clone()
    }

    pub async fn append_compaction(
        &self,
        summary: String,
        tokens_before: usize,
        replacement_history: Vec<Message>,
        manual: bool,
    ) -> Result<SessionEvent> {
        self.append_compaction_with_spec(summary, tokens_before, replacement_history, manual, None)
            .await
    }

    pub async fn append_compaction_with_spec(
        &self,
        summary: String,
        tokens_before: usize,
        replacement_history: Vec<Message>,
        manual: bool,
        updated: Option<(AgentSpec, SessionContext)>,
    ) -> Result<SessionEvent> {
        let mut events = Vec::with_capacity(2);
        if let Some((spec, context)) = updated {
            events.push(EventKind::AgentSpecUpdated { spec, context });
        }
        events.push(EventKind::Compaction {
            summary,
            tokens_before,
            replacement_history,
            manual,
        });
        self.append_batch(events)
            .await?
            .into_iter()
            .next_back()
            .ok_or_else(|| anyhow!("compaction append produced no event"))
    }

    pub async fn update_new_model_settings(
        &self,
        provider: &str,
        model: &str,
        thinking: ThinkingLevel,
    ) -> Result<()> {
        let mut state = self.state.lock().await;
        if state.spec.provider == provider
            && state.spec.model == model
            && state.spec.thinking == thinking
        {
            return Ok(());
        }
        if state.persisted || state.derived.has_user {
            bail!("an Agent's model and thinking cannot change after its first submission");
        }
        state.spec.provider = provider.to_string();
        state.spec.model = model.to_string();
        state.spec.thinking = thinking;
        let spec = state.spec.clone();
        let Some(created) = state
            .events
            .iter_mut()
            .find(|event| matches!(event.kind, EventKind::SessionCreated { .. }))
        else {
            bail!("new session has no creation event");
        };
        created.kind = EventKind::SessionCreated { spec: spec.clone() };
        state.derived.spec = Some(spec);
        Ok(())
    }

    pub async fn append(&self, kind: EventKind) -> Result<SessionEvent> {
        self.append_batch(vec![kind])
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("session append produced no event"))
    }

    /// Append a related event boundary in one SQLite transaction and publish
    /// it only after commit. This keeps transcript, replay, usage, and terminal
    /// state from observing partially persisted boundaries after a crash.
    pub async fn append_batch(&self, kinds: Vec<EventKind>) -> Result<Vec<SessionEvent>> {
        self.append_batch_inner(kinds, None).await
    }

    pub(crate) async fn append_batch_consuming_pending(
        &self,
        pending_id: i64,
        kinds: Vec<EventKind>,
    ) -> Result<Vec<SessionEvent>> {
        self.append_batch_inner(kinds, Some(pending_id)).await
    }

    async fn append_batch_inner(
        &self,
        kinds: Vec<EventKind>,
        consume_pending: Option<i64>,
    ) -> Result<Vec<SessionEvent>> {
        if kinds.is_empty() {
            return Ok(Vec::new());
        }
        let mut state = self.state.lock().await;
        let at = Utc::now();
        let at_text = at.to_rfc3339();
        let first_sequence = state
            .head_sequence
            .map_or(0, |sequence| sequence.saturating_add(1));
        let events = kinds
            .into_iter()
            .enumerate()
            .map(|(offset, kind)| SessionEvent {
                sequence: first_sequence.saturating_add(offset as u64),
                at,
                kind,
            })
            .collect::<Vec<_>>();
        if events.iter().any(|event| starts_session(&event.kind)) && state.context.is_none() {
            bail!("cannot start a session before its startup context is ready");
        }
        for event in &events {
            if let EventKind::AgentSpecUpdated { spec, .. } = &event.kind
                && (spec.provider != state.spec.provider
                    || spec.model != state.spec.model
                    || spec.thinking != state.spec.thinking
                    || spec.working_directory != state.spec.working_directory
                    || spec.parent_session_id != state.spec.parent_session_id
                    || spec.depth() != state.spec.depth()
                    || spec.max_output_tokens != state.spec.max_output_tokens)
            {
                bail!("compaction may only update an Agent's prompt, tools, and protocols");
            }
        }
        let mut next_spec = state.spec.clone();
        apply_agent_spec(&mut next_spec, events.iter().map(|event| &event.kind));

        if !state.persisted {
            if consume_pending.is_some() {
                bail!("cannot consume a pending input from an unpersisted session");
            }
            if !events.iter().any(|event| starts_session(&event.kind)) {
                state.spec = next_spec;
                apply_committed_events(&mut state, &events, false);
                self.publish_persisted(&events);
                drop(state);
                return Ok(events);
            }

            let mut stored_events = Vec::with_capacity(state.events.len() + events.len());
            for stored in state.events.iter().chain(events.iter()) {
                stored_events.push((
                    stored.sequence as i64,
                    stored.at.to_rfc3339(),
                    payload_kind(&stored.kind).to_string(),
                    serde_json::to_string(&stored.kind)
                        .context("cannot serialize session event")?,
                ));
            }
            let id = self.id.clone();
            let project = self.project.clone();
            let provider = next_spec.provider.clone();
            let model = next_spec.model.clone();
            let thinking = next_spec.thinking.to_string();
            let parent_session_id = next_spec.parent_session_id.clone();
            let depth = i64::from(next_spec.depth());
            let head_sequence = events
                .last()
                .expect("nonempty batch has a final event")
                .sequence;
            self.connection
                .call(move |db| {
                    let transaction = db.transaction()?;
                    let draft = transaction
                        .query_row(
                            "SELECT draft FROM pending_drafts WHERE cwd = ?1",
                            [&project],
                            |row| row.get::<_, String>(0),
                        )
                        .optional()?
                        .unwrap_or_default();
                    transaction.execute(
                        "INSERT INTO sessions
                         (id, created_at, updated_at, cwd, provider, model, thinking,
                          parent_session_id, depth, head_sequence, draft)
                         VALUES (?1, ?2, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                        params![
                            id,
                            at_text,
                            project,
                            provider,
                            model,
                            thinking,
                            parent_session_id,
                            depth,
                            head_sequence as i64,
                            draft
                        ],
                    )?;
                    for (sequence, event_at, kind_name, payload) in stored_events {
                        transaction.execute(
                            "INSERT INTO events
                             (session_id, sequence, at, kind, payload_json)
                             VALUES (?1, ?2, ?3, ?4, ?5)",
                            params![id, sequence, event_at, kind_name, payload],
                        )?;
                    }
                    transaction.execute("DELETE FROM pending_drafts WHERE cwd = ?1", [project])?;
                    transaction.commit()?;
                    Ok::<_, tokio_rusqlite::rusqlite::Error>(())
                })
                .await
                .context("cannot create session")?;
            state.persisted = true;
            state.spec = next_spec;
            let checkpoint = apply_committed_events(&mut state, &events, true);
            self.publish_persisted(&events);
            drop(state);
            if let Some((through, payload)) = checkpoint {
                self.persist_resume_index(through, payload).await;
            }
            return Ok(events);
        }

        let id = self.id.clone();
        let stored_events = events
            .iter()
            .map(|event| {
                Ok::<_, anyhow::Error>((
                    event.sequence as i64,
                    payload_kind(&event.kind).to_string(),
                    serde_json::to_string(&event.kind).context("cannot serialize session event")?,
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        let provider = next_spec.provider.clone();
        let model = next_spec.model.clone();
        let thinking = next_spec.thinking.to_string();
        let expected_head = state.head_sequence.map_or(-1, |sequence| sequence as i64);
        let head_sequence = events
            .last()
            .expect("nonempty batch has a final event")
            .sequence as i64;
        self.connection
            .call(move |db| {
                let transaction = db.transaction()?;
                let head: i64 = transaction.query_row(
                    "SELECT head_sequence FROM sessions WHERE id = ?1",
                    [&id],
                    |row| row.get(0),
                )?;
                if head != expected_head {
                    return Err(tokio_rusqlite::rusqlite::Error::InvalidQuery);
                }
                for (sequence, kind_name, payload) in stored_events {
                    transaction.execute(
                        "INSERT INTO events (session_id, sequence, at, kind, payload_json)
                         VALUES (?1, ?2, ?3, ?4, ?5)",
                        params![id, sequence, at_text, kind_name, payload],
                    )?;
                }
                transaction.execute(
                    "UPDATE sessions
                     SET updated_at = ?2, head_sequence = ?3,
                         provider = ?4, model = ?5, thinking = ?6
                     WHERE id = ?1",
                    params![id, at_text, head_sequence, provider, model, thinking],
                )?;
                if let Some(pending_id) = consume_pending
                    && transaction.execute(
                        "DELETE FROM pending_inputs WHERE session_id = ?1 AND id = ?2",
                        params![id, pending_id],
                    )? != 1
                {
                    return Err(tokio_rusqlite::rusqlite::Error::InvalidQuery);
                }
                transaction.commit()?;
                Ok::<_, tokio_rusqlite::rusqlite::Error>(())
            })
            .await
            .context("cannot append session event batch")?;
        state.spec = next_spec;
        let checkpoint = apply_committed_events(&mut state, &events, true);
        self.publish_persisted(&events);
        drop(state);
        if let Some((through, payload)) = checkpoint {
            self.persist_resume_index(through, payload).await;
        }
        Ok(events)
    }

    fn publish_persisted(&self, events: &[SessionEvent]) {
        for event in events {
            let _ = self.events.send(SessionUpdate::Persisted(event.clone()));
        }
    }

    async fn persist_resume_index(&self, through: u64, payload: String) {
        let id = self.id.clone();
        let checksum = resume_checksum(&id, through, &payload);
        let _ = self
            .connection
            .call(move |db| {
                db.execute(
                    "INSERT INTO session_resume_index
                     (session_id, version, through_sequence, payload_json, checksum)
                     VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT(session_id) DO UPDATE SET
                       version = excluded.version,
                       through_sequence = excluded.through_sequence,
                       payload_json = excluded.payload_json,
                       checksum = excluded.checksum
                     WHERE excluded.through_sequence >= session_resume_index.through_sequence",
                    params![
                        id,
                        i64::from(RESUME_INDEX_VERSION),
                        through as i64,
                        payload,
                        checksum
                    ],
                )?;
                Ok::<_, tokio_rusqlite::rusqlite::Error>(())
            })
            .await;
    }

    async fn query_event_page(&self, page: EventPage) -> Result<Vec<SessionEvent>> {
        let state = self.state.lock().await;
        if !state.persisted {
            let events = match page {
                EventPage::All => state.events.clone(),
                EventPage::After(sequence, limit) => state
                    .events
                    .iter()
                    .filter(|event| event.sequence > sequence)
                    .take(limit)
                    .cloned()
                    .collect(),
                EventPage::Before(sequence, limit) => {
                    let mut events = state
                        .events
                        .iter()
                        .rev()
                        .filter(|event| event.sequence < sequence)
                        .take(limit)
                        .cloned()
                        .collect::<Vec<_>>();
                    events.reverse();
                    events
                }
                EventPage::Tail(limit) => {
                    let start = state.events.len().saturating_sub(limit);
                    state.events[start..].to_vec()
                }
            };
            #[cfg(test)]
            self.event_read_audit
                .lock()
                .unwrap()
                .payload_pages
                .push(events.iter().map(|event| event.sequence).collect());
            return Ok(events);
        }
        drop(state);

        let id = self.id.clone();
        let events = self
            .connection
            .call(move |db| {
                let (sql, cursor, limit) = match page {
                    EventPage::All => (
                        "SELECT sequence, at, payload_json FROM events
                         WHERE session_id = ?1 ORDER BY sequence",
                        0_i64,
                        i64::MAX,
                    ),
                    EventPage::After(sequence, limit) => (
                        "SELECT sequence, at, payload_json FROM events
                         WHERE session_id = ?1 AND sequence > ?2
                         ORDER BY sequence LIMIT ?3",
                        sequence as i64,
                        limit as i64,
                    ),
                    EventPage::Before(sequence, limit) => (
                        "SELECT sequence, at, payload_json FROM (
                           SELECT sequence, at, payload_json FROM events
                           WHERE session_id = ?1 AND sequence < ?2
                           ORDER BY sequence DESC LIMIT ?3
                         ) ORDER BY sequence",
                        sequence as i64,
                        limit as i64,
                    ),
                    EventPage::Tail(limit) => (
                        "SELECT sequence, at, payload_json FROM (
                           SELECT sequence, at, payload_json FROM events
                           WHERE session_id = ?1 ORDER BY sequence DESC LIMIT ?3
                         ) ORDER BY sequence",
                        0_i64,
                        limit as i64,
                    ),
                };
                let mut statement = db.prepare(sql)?;
                let rows = match page {
                    EventPage::All => statement.query_map([&id], stored_event_from_row)?,
                    _ => statement.query_map(params![id, cursor, limit], stored_event_from_row)?,
                };
                rows.collect::<Result<Vec<_>, _>>()
            })
            .await
            .context("cannot read session event page")?;
        #[cfg(test)]
        self.event_read_audit
            .lock()
            .unwrap()
            .payload_pages
            .push(events.iter().map(|event| event.sequence).collect());
        Ok(events)
    }
}

fn event_header(event: &SessionEvent) -> SessionEventHeader {
    SessionEventHeader {
        sequence: event.sequence,
        starts_turn: matches!(&event.kind, EventKind::User { .. }),
        finishes_turn: matches!(&event.kind, EventKind::TurnFinished),
    }
}

async fn list_project_sessions(database_path: PathBuf, cwd: &Path) -> Result<Vec<SessionSummary>> {
    let project = cwd
        .canonicalize()
        .unwrap_or_else(|_| cwd.to_path_buf())
        .to_string_lossy()
        .into_owned();
    let (_, connection) = open_database(database_path).await?;
    connection
        .call(move |db| {
            let mut statement = db.prepare(
                "SELECT id, updated_at, provider, model, thinking,
                    (SELECT payload_json FROM events
                     WHERE events.session_id = sessions.id AND kind = 'user'
                     ORDER BY sequence DESC LIMIT 1)
                 FROM sessions WHERE cwd = ?1 AND depth = 1
                 ORDER BY updated_at DESC, id DESC",
            )?;
            let rows = statement.query_map([project], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, Option<String>>(5)?,
                ))
            })?;
            let mut sessions = Vec::new();
            for row in rows {
                let (id, updated_at, provider, model, thinking, payload) = row?;
                let preview = payload
                    .and_then(|payload| serde_json::from_str::<EventKind>(&payload).ok())
                    .and_then(|kind| match kind {
                        EventKind::User { text } => Some(text),
                        _ => None,
                    })
                    .unwrap_or_else(|| "empty session".to_string());
                let updated_at = DateTime::parse_from_rfc3339(&updated_at)
                    .map(|value| value.with_timezone(&Utc))
                    .unwrap_or_else(|_| Utc::now());
                sessions.push(SessionSummary {
                    id,
                    updated_at,
                    provider,
                    model,
                    thinking: thinking.parse().unwrap_or_default(),
                    preview,
                });
            }
            Ok::<_, tokio_rusqlite::rusqlite::Error>(sessions)
        })
        .await
        .context("cannot list sessions")
}

fn session_database_path(fallback: &Path) -> PathBuf {
    session_database_path_from(
        macos_session_config_directory(),
        dirs::data_dir().as_deref(),
        fallback,
    )
}

fn macos_session_config_directory() -> Option<PathBuf> {
    if cfg!(target_os = "macos") {
        crate::config::config_directory().ok()
    } else {
        None
    }
}

fn session_database_path_from(
    config_directory: Option<PathBuf>,
    platform_data_dir: Option<&Path>,
    fallback: &Path,
) -> PathBuf {
    if let Some(directory) = config_directory {
        return directory.join(SESSION_DATABASE_FILE);
    }
    platform_data_dir
        .map(|path| path.join("uri-agent").join(SESSION_DATABASE_FILE))
        .unwrap_or_else(|| fallback.join(".uri-agent").join(SESSION_DATABASE_FILE))
}

async fn open_archive_database(path: &Path) -> Result<Option<Connection>> {
    if !fs::try_exists(path)
        .await
        .with_context(|| format!("cannot inspect session database: {}", display_path(path)))?
    {
        return Ok(None);
    }
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .await
        .map(Some)
        .with_context(|| format!("cannot open session archive: {}", display_path(path)))
}

async fn open_database(database_path: PathBuf) -> Result<(PathBuf, Connection)> {
    let directory = database_path
        .parent()
        .unwrap_or(Path::new("."))
        .to_path_buf();
    fs::create_dir_all(&directory).await.with_context(|| {
        format!(
            "cannot create session data directory: {}",
            display_path(&directory)
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).await?;
    }
    let connection = Connection::open(&database_path).await.with_context(|| {
        format!(
            "cannot open session database: {}",
            display_path(&database_path)
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&database_path, std::fs::Permissions::from_mode(0o600)).await?;
    }
    connection
        .call(|db| {
            db.execute_batch(
                "PRAGMA foreign_keys = ON;
                 PRAGMA journal_mode = WAL;
                 CREATE TABLE IF NOT EXISTS sessions (
                   id TEXT PRIMARY KEY, created_at TEXT NOT NULL, updated_at TEXT NOT NULL,
                   cwd TEXT NOT NULL, provider TEXT NOT NULL, model TEXT NOT NULL,
                   thinking TEXT NOT NULL,
                   parent_session_id TEXT,
                   depth INTEGER NOT NULL CHECK (depth BETWEEN 1 AND 2),
                   head_sequence INTEGER NOT NULL,
                   draft TEXT NOT NULL,
                   FOREIGN KEY(parent_session_id) REFERENCES sessions(id)
                 );
                 CREATE TABLE IF NOT EXISTS events (
                   session_id TEXT NOT NULL, sequence INTEGER NOT NULL, at TEXT NOT NULL,
                   kind TEXT NOT NULL, payload_json TEXT NOT NULL,
                   PRIMARY KEY(session_id, sequence),
                   FOREIGN KEY(session_id) REFERENCES sessions(id) ON DELETE CASCADE
                 );
                 CREATE INDEX IF NOT EXISTS events_session_kind_sequence
                   ON events(session_id, kind, sequence);
                 CREATE TABLE IF NOT EXISTS session_resume_index (
                   session_id TEXT PRIMARY KEY,
                   version INTEGER NOT NULL,
                   through_sequence INTEGER NOT NULL,
                   payload_json TEXT NOT NULL,
                   checksum TEXT NOT NULL,
                   FOREIGN KEY(session_id) REFERENCES sessions(id) ON DELETE CASCADE
                 );
                 CREATE TABLE IF NOT EXISTS pending_inputs (
                   id INTEGER PRIMARY KEY AUTOINCREMENT,
                   session_id TEXT NOT NULL,
                   kind TEXT NOT NULL CHECK (kind IN ('prompt', 'steer')),
                   text TEXT NOT NULL,
                   content_json TEXT NOT NULL,
                   visible INTEGER NOT NULL,
                   created_at TEXT NOT NULL,
                   FOREIGN KEY(session_id) REFERENCES sessions(id) ON DELETE CASCADE
                 );
                 CREATE INDEX IF NOT EXISTS pending_inputs_session_id_id
                   ON pending_inputs(session_id, id);
                 CREATE TABLE IF NOT EXISTS pending_drafts (
                   cwd TEXT PRIMARY KEY, draft TEXT NOT NULL
                 );",
            )?;
            let has_checksum = {
                let mut statement = db.prepare("PRAGMA table_info(session_resume_index)")?;
                statement
                    .query_map([], |row| row.get::<_, String>(1))?
                    .collect::<Result<Vec<_>, _>>()?
                    .iter()
                    .any(|column| column == "checksum")
            };
            if !has_checksum {
                db.execute(
                    "ALTER TABLE session_resume_index
                     ADD COLUMN checksum TEXT NOT NULL DEFAULT ''",
                    [],
                )?;
            }
            Ok::<_, tokio_rusqlite::rusqlite::Error>(())
        })
        .await
        .context("cannot initialize session database")?;
    Ok((directory, connection))
}

fn payload_kind(kind: &EventKind) -> &'static str {
    match kind {
        EventKind::SessionCreated { .. } => "session_created",
        EventKind::SessionContext { .. } => "session_context",
        EventKind::AgentSpecUpdated { .. } => "agent_spec_updated",
        EventKind::User { .. } => "user",
        EventKind::AssistantText { .. } => "assistant_text",
        EventKind::AssistantReasoning { .. } => "assistant_reasoning",
        EventKind::AssistantToolCallDelta { .. } => "assistant_tool_call_delta",
        EventKind::ToolCall { .. } => "tool_call",
        EventKind::ToolResult { .. } => "tool_result",
        EventKind::ModelMessage { .. } => "model_message",
        EventKind::Task { .. } => "task",
        EventKind::Notice { .. } => "notice",
        EventKind::ModelRetry { .. } => "model_retry",
        EventKind::Usage { .. } => "usage",
        EventKind::Error { .. } => "error",
        EventKind::Compaction { .. } => "compaction",
        EventKind::TurnFinished => "turn_finished",
    }
}

fn starts_session(kind: &EventKind) -> bool {
    matches!(kind, EventKind::User { .. })
}

fn apply_committed_events(
    state: &mut State,
    events: &[SessionEvent],
    compact_memory: bool,
) -> Option<(u64, String)> {
    let mut checkpoint = None;
    for event in events {
        apply_resume_event(&mut state.derived, event);
        apply_replay_event(&mut state.replay, &event.kind);
        match &event.kind {
            EventKind::SessionContext { context } | EventKind::AgentSpecUpdated { context, .. } => {
                state.context = Some(context.clone());
            }
            _ => {}
        }
        state.head_sequence = Some(event.sequence);
        state.events.push(event.clone());
        if matches!(event.kind, EventKind::Compaction { .. }) {
            checkpoint = serde_json::to_string(&state.derived)
                .ok()
                .map(|payload| (event.sequence, payload));
            if compact_memory {
                state.events.clear();
            }
        }
    }
    checkpoint
}

fn apply_replay_event(replay: &mut ReplayState, kind: &EventKind) {
    match kind {
        EventKind::Compaction {
            summary,
            replacement_history,
            ..
        } => {
            replay.history.clone_from(replacement_history);
            replay.usage_before_assistant = vec![None; replacement_history.len()];
            replay.pending_usage = None;
            replay.after_compaction = true;
            replay.latest_compaction_summary = Some(summary.clone());
        }
        EventKind::Usage {
            total,
            context,
            provider,
            model,
            ..
        } => {
            replay.pending_usage = Some(UsageBaseline {
                total: *total as usize,
                context: *context,
                provider: provider.clone(),
                model: model.clone(),
            });
        }
        EventKind::ModelMessage { message } => {
            let usage = matches!(message, Message::Assistant { .. })
                .then(|| replay.pending_usage.take())
                .flatten();
            replay.history.push(message.clone());
            replay.usage_before_assistant.push(usage);
        }
        _ => {}
    }
}

fn apply_resume_event(state: &mut ResumeState, event: &SessionEvent) {
    state.through_sequence = Some(event.sequence);
    match &event.kind {
        EventKind::SessionCreated { spec } => {
            state.spec = Some(spec.clone());
        }
        EventKind::SessionContext { .. } => state.context_sequence = Some(event.sequence),
        EventKind::AgentSpecUpdated { spec, .. } => {
            state.spec = Some(spec.clone());
            state.context_sequence = Some(event.sequence);
        }
        EventKind::User { .. } => state.has_user = true,
        EventKind::ToolCall {
            call_id,
            name,
            arguments,
        } => {
            state.pending_help_reads.remove(call_id);
            if name == "read"
                && let (Some(uri), Some("")) = (
                    arguments.get("uri").and_then(Value::as_str),
                    arguments.get("body").and_then(Value::as_str),
                )
                && let Ok((protocol, "help")) = crate::protocol::split_address(uri)
            {
                state
                    .pending_help_reads
                    .insert(call_id.clone(), protocol.to_string());
            }
            state.token_calibration.pending_visible_units = state
                .token_calibration
                .pending_visible_units
                .saturating_add(crate::text_metrics::visible_units(name) as u64)
                .saturating_add(crate::text_metrics::visible_units(&arguments.to_string()) as u64);
        }
        EventKind::ToolResult {
            call_id,
            name,
            failed,
            ..
        } => {
            if let Some(protocol) = state.pending_help_reads.remove(call_id)
                && name == "read"
                && !failed
            {
                state.successful_help_reads.insert(protocol);
            }
        }
        EventKind::Task { id, output, .. } => {
            let pointers = state.tasks.entry(id.clone()).or_insert(TaskPointers {
                first_sequence: event.sequence,
                latest_sequence: event.sequence,
                output_sequence: None,
            });
            pointers.latest_sequence = event.sequence;
            if output.is_some() {
                pointers.output_sequence = Some(event.sequence);
            }
        }
        EventKind::Usage {
            input,
            output,
            reasoning,
            cache_read,
            cache_write,
            cost,
            context,
            ..
        } => {
            state.usage.input = state.usage.input.saturating_add(*input);
            state.usage.output = state.usage.output.saturating_add(*output);
            state.usage.cache_read = state.usage.cache_read.saturating_add(*cache_read);
            state.usage.cache_write = state.usage.cache_write.saturating_add(*cache_write);
            state.usage.cost += cost;
            let prompt_tokens = input
                .saturating_add(*cache_read)
                .saturating_add(*cache_write);
            state.usage.last_cache_hit =
                (prompt_tokens > 0).then(|| *cache_read as f64 / prompt_tokens as f64 * 100.0);
            if *context {
                state.token_calibration.pending_usage = Some((*output, *reasoning));
            }
        }
        EventKind::AssistantText { text } => {
            state.token_calibration.pending_visible_units = state
                .token_calibration
                .pending_visible_units
                .saturating_add(crate::text_metrics::visible_units(text) as u64);
        }
        EventKind::AssistantReasoning { text } => {
            state.token_calibration.pending_reasoning_visible |= !text.is_empty();
            state.token_calibration.pending_visible_units = state
                .token_calibration
                .pending_visible_units
                .saturating_add(crate::text_metrics::visible_units(text) as u64);
        }
        EventKind::ModelMessage {
            message: Message::Assistant { .. },
        } => {
            if let Some((output, reasoning)) = state.token_calibration.pending_usage.take() {
                let output = if state.token_calibration.pending_reasoning_visible {
                    output
                } else {
                    output.saturating_sub(reasoning)
                };
                if output > 0 && state.token_calibration.pending_visible_units > 0 {
                    state.token_calibration.visible_units = state
                        .token_calibration
                        .visible_units
                        .saturating_add(state.token_calibration.pending_visible_units);
                    state.token_calibration.output_tokens =
                        state.token_calibration.output_tokens.saturating_add(output);
                }
            }
            state.token_calibration.pending_visible_units = 0;
            state.token_calibration.pending_reasoning_visible = false;
        }
        EventKind::Compaction { .. } => {
            state.latest_compaction_sequence = Some(event.sequence);
            state.token_calibration.pending_visible_units = 0;
            state.token_calibration.pending_reasoning_visible = false;
            state.token_calibration.pending_usage = None;
        }
        EventKind::ModelRetry { .. } | EventKind::Error { .. } => {
            state.token_calibration.pending_visible_units = 0;
            state.token_calibration.pending_reasoning_visible = false;
            state.token_calibration.pending_usage = None;
        }
        _ => {}
    }
}

fn task_reports_from_events(events: &[SessionEvent]) -> Vec<TaskReport> {
    struct ReportState {
        first_sequence: u64,
        protocol: String,
        label: String,
        status: TaskStatus,
        started_at: DateTime<Utc>,
        updated_at: DateTime<Utc>,
        output: String,
    }

    let mut reports = HashMap::<String, ReportState>::new();
    for event in events {
        let EventKind::Task {
            id,
            protocol,
            label,
            status,
            output,
        } = &event.kind
        else {
            continue;
        };
        let report = reports.entry(id.clone()).or_insert_with(|| ReportState {
            first_sequence: event.sequence,
            protocol: protocol.clone(),
            label: label.clone(),
            status: *status,
            started_at: event.at,
            updated_at: event.at,
            output: String::new(),
        });
        report.protocol.clone_from(protocol);
        report.label.clone_from(label);
        report.status = *status;
        report.updated_at = event.at;
        if let Some(output) = output {
            report.output.clone_from(output);
        }
    }

    let mut reports = reports
        .into_iter()
        .map(|(id, report)| {
            (
                report.first_sequence,
                TaskReport {
                    id,
                    protocol: report.protocol,
                    label: report.label,
                    status: if report.status.terminal() {
                        report.status
                    } else {
                        TaskStatus::Cancelled
                    },
                    started_at: report.started_at,
                    finished_at: report.updated_at,
                    content: report.output.into_bytes(),
                },
            )
        })
        .collect::<Vec<_>>();
    reports.sort_by(|(left_sequence, left), (right_sequence, right)| {
        left_sequence
            .cmp(right_sequence)
            .then_with(|| left.id.cmp(&right.id))
    });
    reports.into_iter().map(|(_, report)| report).collect()
}

fn apply_agent_spec<'a>(spec: &mut AgentSpec, kinds: impl IntoIterator<Item = &'a EventKind>) {
    for kind in kinds {
        match kind {
            EventKind::SessionCreated { spec: updated }
            | EventKind::AgentSpecUpdated { spec: updated, .. } => {
                spec.clone_from(updated);
            }
            _ => {}
        }
    }
}

fn agent_spec_from_events(events: &[SessionEvent]) -> Option<AgentSpec> {
    let mut spec = events.iter().find_map(|event| match &event.kind {
        EventKind::SessionCreated { spec } => Some(spec.clone()),
        _ => None,
    })?;
    apply_agent_spec(&mut spec, events.iter().map(|event| &event.kind));
    Some(spec)
}

fn validate_session_id(id: &str) -> Result<()> {
    if id.is_empty()
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
    {
        return Err(anyhow!("invalid session ID"));
    }
    Ok(())
}

fn new_session_id() -> String {
    Uuid::now_v7().simple().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(label: &str) -> SessionContext {
        SessionContext {
            system_prompt: format!("system {label}"),
            skills: vec![SkillSnapshot {
                name: "Review".to_string(),
                description: format!("description {label}"),
                path: PathBuf::from(format!("/skills/{label}/SKILL.md")),
            }],
        }
    }

    async fn session(path: &Path, requested: Option<&str>) -> Session {
        Session::open_at(
            path.to_path_buf(),
            requested,
            Path::new("/work"),
            "test",
            "model",
            context("initial"),
        )
        .await
        .unwrap()
    }

    fn eager_model_context(events: &[SessionEvent], provider: &str, model: &str) -> ModelContext {
        let latest_compaction = events
            .iter()
            .rposition(|event| matches!(event.kind, EventKind::Compaction { .. }));
        let mut history = latest_compaction
            .and_then(|index| match &events[index].kind {
                EventKind::Compaction {
                    replacement_history,
                    ..
                } => Some(replacement_history.clone()),
                _ => None,
            })
            .unwrap_or_default();
        let mut latest_api_usage = None;
        let mut pending_usage = None;
        for event in events
            .iter()
            .skip(latest_compaction.map_or(0, |index| index + 1))
        {
            match &event.kind {
                EventKind::Usage {
                    total,
                    context,
                    provider: usage_provider,
                    model: usage_model,
                    ..
                } => {
                    pending_usage = (*context
                        && *total > 0
                        && (provider.is_empty()
                            || usage_provider.is_empty()
                            || usage_provider == provider)
                        && (model.is_empty() || usage_model.is_empty() || usage_model == model))
                        .then_some(*total as usize);
                }
                EventKind::ModelMessage { message } => {
                    history.push(message.clone());
                    if matches!(message, Message::Assistant { .. })
                        && let Some(tokens) = pending_usage.take()
                    {
                        latest_api_usage = Some((history.len() - 1, tokens));
                    }
                }
                _ => {}
            }
        }
        ModelContext {
            history,
            latest_api_usage,
            after_compaction: latest_compaction.is_some(),
        }
    }

    #[tokio::test]
    async fn events_persist_in_order_and_reopen() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("sessions.db");
        let first = session(&path, Some("ordered")).await;
        first
            .append(EventKind::User {
                text: "hello".into(),
            })
            .await
            .unwrap();
        first.append(EventKind::TurnFinished).await.unwrap();
        drop(first);
        let reopened = session(&path, Some("ordered")).await;
        let events = reopened.snapshot().await.unwrap();
        assert_eq!(
            events
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            vec![0, 1, 2, 3]
        );
        assert!(matches!(events[3].kind, EventKind::TurnFinished));
    }

    #[tokio::test]
    async fn terminal_task_reports_are_restored_and_interrupted_tasks_settle_cancelled() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("sessions.db");
        let first = session(&path, Some("task-reports")).await;
        first
            .append(EventKind::User {
                text: "run background work".into(),
            })
            .await
            .unwrap();
        first
            .append_batch(vec![
                EventKind::Task {
                    id: "00a".into(),
                    protocol: "bash".into(),
                    label: "completed command".into(),
                    status: TaskStatus::Running,
                    output: None,
                },
                EventKind::Task {
                    id: "00a".into(),
                    protocol: "bash".into(),
                    label: "completed command".into(),
                    status: TaskStatus::Completed,
                    output: Some("complete output".into()),
                },
                EventKind::Task {
                    id: "00b".into(),
                    protocol: "bash".into(),
                    label: "interrupted command".into(),
                    status: TaskStatus::Running,
                    output: None,
                },
            ])
            .await
            .unwrap();
        drop(first);

        let reopened = session(&path, Some("task-reports")).await;
        let reports = reopened.task_reports().await.unwrap();
        let completed = reports.iter().find(|report| report.id == "00a").unwrap();
        assert_eq!(completed.status, TaskStatus::Completed);
        assert_eq!(completed.content, b"complete output");
        let interrupted = reports.iter().find(|report| report.id == "00b").unwrap();
        assert_eq!(interrupted.status, TaskStatus::Cancelled);
        assert!(interrupted.content.is_empty());
    }

    #[tokio::test]
    async fn invalid_task_pointers_rebuild_exact_reports_and_task_read_failures_propagate() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("sessions.db");
        let opened = session(&path, Some("task-pointer-validation")).await;
        opened
            .append_batch(vec![
                EventKind::User {
                    text: "tasks".into(),
                },
                EventKind::Task {
                    id: "first".into(),
                    protocol: "bash".into(),
                    label: "first task".into(),
                    status: TaskStatus::Completed,
                    output: Some("first output".into()),
                },
                EventKind::Task {
                    id: "second".into(),
                    protocol: "bash".into(),
                    label: "second task".into(),
                    status: TaskStatus::Completed,
                    output: Some("second output".into()),
                },
            ])
            .await
            .unwrap();
        opened
            .append_compaction("tasks".into(), 10, Vec::new(), false)
            .await
            .unwrap();
        drop(opened);

        let resumed = session(&path, Some("task-pointer-validation")).await;
        let expected = resumed.task_reports().await.unwrap();
        resumed
            .state
            .lock()
            .await
            .derived
            .tasks
            .get_mut("first")
            .unwrap()
            .latest_sequence = u64::MAX;
        assert_eq!(resumed.task_reports().await.unwrap(), expected);

        let second_sequence = resumed.state.lock().await.derived.tasks["second"].first_sequence;
        resumed
            .state
            .lock()
            .await
            .derived
            .tasks
            .get_mut("first")
            .unwrap()
            .latest_sequence = second_sequence;
        assert_eq!(resumed.task_reports().await.unwrap(), expected);

        let first_sequence = resumed.state.lock().await.derived.tasks["first"].first_sequence;
        resumed
            .connection
            .call(move |db| {
                db.execute(
                    "UPDATE events SET payload_json = '{bad task payload'
                     WHERE session_id = 'task-pointer-validation' AND sequence = ?1",
                    [first_sequence as i64],
                )?;
                Ok::<_, tokio_rusqlite::rusqlite::Error>(())
            })
            .await
            .unwrap();
        let error = resumed.task_reports().await.unwrap_err();
        assert!(format!("{error:#}").contains("cannot restore session task reports"));
    }

    #[tokio::test]
    async fn persisted_snapshot_and_frozen_context_deserialization_failures_propagate() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("sessions.db");
        let opened = session(&path, Some("strict-session-reads")).await;
        let notice = opened
            .append_batch(vec![
                EventKind::User {
                    text: "persist".into(),
                },
                EventKind::Notice {
                    text: "break me".into(),
                },
            ])
            .await
            .unwrap()
            .pop()
            .unwrap();
        opened
            .connection
            .call(move |db| {
                db.execute(
                    "UPDATE events SET payload_json = '{bad snapshot payload'
                     WHERE session_id = 'strict-session-reads' AND sequence = ?1",
                    [notice.sequence as i64],
                )?;
                Ok::<_, tokio_rusqlite::rusqlite::Error>(())
            })
            .await
            .unwrap();
        assert!(opened.snapshot().await.is_err());
        drop(opened);

        let context_session = session(&path, Some("strict-context")).await;
        context_session
            .append(EventKind::User {
                text: "persist context".into(),
            })
            .await
            .unwrap();
        context_session
            .connection
            .call(|db| {
                db.execute(
                    "UPDATE events SET payload_json = '{bad context payload'
                     WHERE session_id = 'strict-context' AND kind = 'session_context'",
                    [],
                )?;
                Ok::<_, tokio_rusqlite::rusqlite::Error>(())
            })
            .await
            .unwrap();
        drop(context_session);
        let error = match Session::open_at(
            path,
            Some("strict-context"),
            Path::new("/work"),
            "test",
            "model",
            context("ignored"),
        )
        .await
        {
            Ok(_) => panic!("malformed frozen context unexpectedly resumed"),
            Err(error) => error,
        };
        assert!(format!("{error:#}").contains("cannot restore session state"));
    }

    #[test]
    fn historical_task_events_without_output_remain_readable() {
        let event = serde_json::from_value::<EventKind>(serde_json::json!({
            "kind": "task",
            "id": "001",
            "protocol": "bash",
            "label": "historical",
            "status": "completed"
        }))
        .unwrap();
        assert!(matches!(event, EventKind::Task { output: None, .. }));
    }

    #[tokio::test]
    async fn model_history_is_restored() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("sessions.db");
        let first = session(&path, Some("history")).await;
        first
            .append(EventKind::User {
                text: "hello".into(),
            })
            .await
            .unwrap();
        first
            .append(EventKind::ModelMessage {
                message: Message::user("hello"),
            })
            .await
            .unwrap();
        drop(first);
        assert_eq!(
            session(&path, Some("history"))
                .await
                .model_history()
                .await
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn provider_usage_baseline_is_restored_with_its_assistant_message() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("sessions.db");
        let first = session(&path, Some("usage-baseline")).await;
        first
            .append_batch(vec![
                EventKind::User {
                    text: "question".to_string(),
                },
                EventKind::ModelMessage {
                    message: Message::user("question"),
                },
                EventKind::Usage {
                    input: 1_000,
                    output: 200,
                    reasoning: 0,
                    cache_read: 34,
                    cache_write: 0,
                    cost: 0.0,
                    total: 1_234,
                    context: true,
                    provider: "test".to_string(),
                    model: "model".to_string(),
                },
                EventKind::ModelMessage {
                    message: Message::assistant("answer"),
                },
                EventKind::ModelMessage {
                    message: Message::user("follow-up"),
                },
            ])
            .await
            .unwrap();
        drop(first);

        let reopened = session(&path, Some("usage-baseline")).await;
        let context = reopened.model_context("test", "model").await;
        assert_eq!(context.history.len(), 3);
        assert_eq!(context.latest_api_usage, Some((1, 1_234)));
        assert!(!context.after_compaction);
    }

    #[tokio::test]
    async fn session_context_is_frozen_on_creation_and_reused_on_resume() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("sessions.db");
        let original = Session::open_at(
            path.clone(),
            Some("frozen"),
            Path::new("/work"),
            "test",
            "model",
            context("original"),
        )
        .await
        .unwrap();
        assert_eq!(original.context().await.system_prompt, "system original");
        original
            .append(EventKind::User {
                text: "freeze this session".into(),
            })
            .await
            .unwrap();
        drop(original);

        let resumed = Session::open_at(
            path,
            Some("frozen"),
            Path::new("/work"),
            "test",
            "model",
            context("changed"),
        )
        .await
        .unwrap();
        let frozen = resumed.context().await;
        assert_eq!(frozen.system_prompt, "system original");
        assert_eq!(frozen.skills[0].description, "description original");
        assert_eq!(
            frozen.skills[0].path,
            Path::new("/skills/original/SKILL.md")
        );
    }

    #[tokio::test]
    async fn deferred_context_is_required_and_persisted_with_the_first_message() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("sessions.db");
        let opened = Session::open_at_with_thinking(
            path.clone(),
            Some("deferred-context"),
            Path::new("/work"),
            "test",
            "model",
            ThinkingLevel::default(),
            None,
        )
        .await
        .unwrap();

        assert!(
            !opened
                .snapshot()
                .await
                .unwrap()
                .iter()
                .any(|event| matches!(event.kind, EventKind::SessionContext { .. }))
        );
        let error = opened
            .append(EventKind::User {
                text: "too early".into(),
            })
            .await
            .unwrap_err();
        assert!(error.to_string().contains("startup context is ready"));

        opened
            .initialize_context(context("deferred"))
            .await
            .unwrap();
        opened
            .append(EventKind::User {
                text: "persist atomically".into(),
            })
            .await
            .unwrap();
        let kinds = opened
            .connection
            .call(|database| {
                let mut statement =
                    database.prepare("SELECT kind FROM events ORDER BY sequence")?;
                statement
                    .query_map([], |row| row.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()
            })
            .await
            .unwrap();
        assert_eq!(kinds, vec!["session_created", "session_context", "user"]);
        drop(opened);

        let resumed = Session::open_at(
            path,
            Some("deferred-context"),
            Path::new("/work"),
            "test",
            "model",
            context("changed"),
        )
        .await
        .unwrap();
        let frozen = resumed.context().await;
        assert_eq!(frozen.system_prompt, "system deferred");
        assert_eq!(frozen.skills[0].description, "description deferred");
    }

    #[tokio::test]
    async fn session_without_a_frozen_context_is_not_reinterpreted() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("sessions.db");
        let original = session(&path, Some("missing-context")).await;
        original
            .append(EventKind::User {
                text: "persist session".into(),
            })
            .await
            .unwrap();
        original
            .connection
            .call(|database| {
                database.execute(
                    "DELETE FROM events
                     WHERE session_id = 'missing-context' AND kind = 'session_context'",
                    [],
                )?;
                Ok::<_, tokio_rusqlite::rusqlite::Error>(())
            })
            .await
            .unwrap();
        drop(original);

        let error = match Session::open_at(
            path,
            Some("missing-context"),
            Path::new("/work"),
            "test",
            "model",
            context("current-disk-state"),
        )
        .await
        {
            Ok(_) => panic!("session without a frozen context was resumed"),
            Err(error) => error,
        };

        assert!(error.to_string().contains("has no frozen context"));
    }

    #[tokio::test]
    async fn compaction_replaces_model_replay_without_deleting_raw_events() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("sessions.db");
        let compacted = session(&path, Some("compacted")).await;
        compacted
            .append(EventKind::User {
                text: "start history".into(),
            })
            .await
            .unwrap();
        compacted
            .append(EventKind::ModelMessage {
                message: Message::user("old history"),
            })
            .await
            .unwrap();
        let replacement = vec![Message::user("durable summary")];
        compacted
            .append_compaction("summary".to_string(), 42, replacement.clone(), false)
            .await
            .unwrap();
        compacted
            .append(EventKind::ModelMessage {
                message: Message::user("new history"),
            })
            .await
            .unwrap();

        assert_eq!(compacted.model_history().await.len(), 2);
        assert_eq!(compacted.model_history().await[0], replacement[0]);
        assert!(compacted.snapshot().await.unwrap().iter().any(|event| {
            matches!(
                &event.kind,
                EventKind::ModelMessage { message } if message == &Message::user("old history")
            )
        }));

        drop(compacted);
        let reopened = session(&path, Some("compacted")).await;
        assert_eq!(reopened.model_history().await.len(), 2);
        let context = reopened.model_context("test", "model").await;
        assert!(context.latest_api_usage.is_none());
        assert!(context.after_compaction);
    }

    #[tokio::test]
    async fn lazy_replay_matches_the_eager_reference_across_compactions_and_usage_pairing() {
        use rig::message::{
            AssistantContent, ToolCall, ToolCallId, ToolFunction, ToolResult, ToolResultContent,
            UserContent,
        };

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("sessions.db");
        let opened = session(&path, Some("differential-replay")).await;
        let call_id = ToolCallId::new("call-17").unwrap();
        let tool_call = Message::Assistant {
            id: Some("assistant-provider-id".into()),
            content: vec![AssistantContent::ToolCall(ToolCall::new(
                call_id.clone(),
                ToolFunction::new("read".into(), serde_json::json!({"uri": "file://help"})),
            ))],
        };
        let tool_result = Message::User {
            content: vec![UserContent::ToolResult(ToolResult {
                call: call_id,
                provider: None,
                name: "read".into(),
                content: vec![ToolResultContent::text("help output")],
            })],
        };
        opened
            .append_batch(vec![
                EventKind::User { text: "old".into() },
                EventKind::ModelMessage {
                    message: Message::user("old"),
                },
                EventKind::Usage {
                    input: 10,
                    output: 2,
                    reasoning: 0,
                    cache_read: 1,
                    cache_write: 0,
                    cost: 0.1,
                    total: 13,
                    context: true,
                    provider: "provider-a".into(),
                    model: "model-a".into(),
                },
                EventKind::ModelMessage {
                    message: Message::assistant("discarded by compaction"),
                },
            ])
            .await
            .unwrap();
        opened
            .append_compaction(
                "first".into(),
                100,
                vec![Message::user("summary one")],
                false,
            )
            .await
            .unwrap();
        opened
            .append_batch(vec![EventKind::ModelMessage {
                message: tool_call.clone(),
            }])
            .await
            .unwrap();
        opened
            .append_compaction(
                "mid-turn".into(),
                80,
                vec![Message::user("summary two"), tool_call],
                false,
            )
            .await
            .unwrap();
        opened
            .append_batch(vec![
                EventKind::ModelMessage {
                    message: tool_result,
                },
                EventKind::Usage {
                    input: 20,
                    output: 4,
                    reasoning: 1,
                    cache_read: 2,
                    cache_write: 3,
                    cost: 0.2,
                    total: 29,
                    context: true,
                    provider: "provider-b".into(),
                    model: "model-b".into(),
                },
                EventKind::Usage {
                    input: 0,
                    output: 0,
                    reasoning: 0,
                    cache_read: 0,
                    cache_write: 0,
                    cost: 0.0,
                    total: 0,
                    context: false,
                    provider: "provider-b".into(),
                    model: "model-b".into(),
                },
                EventKind::ModelMessage {
                    message: Message::assistant("invalid usage replaced the baseline"),
                },
                EventKind::Usage {
                    input: 30,
                    output: 5,
                    reasoning: 0,
                    cache_read: 0,
                    cache_write: 0,
                    cost: 0.3,
                    total: 35,
                    context: true,
                    provider: "provider-b".into(),
                    model: "model-b".into(),
                },
                EventKind::ModelMessage {
                    message: Message::user("usage remains pending across a user message"),
                },
                EventKind::ModelMessage {
                    message: Message::assistant("tail answer"),
                },
            ])
            .await
            .unwrap();

        let events = opened.snapshot().await.unwrap();
        for (provider, model) in [
            ("provider-b", "model-b"),
            ("provider-a", "model-a"),
            ("", ""),
        ] {
            let expected = eager_model_context(&events, provider, model);
            let actual = opened.model_context(provider, model).await;
            assert_eq!(actual.history, expected.history);
            assert_eq!(
                serde_json::to_value(&actual.history).unwrap(),
                serde_json::to_value(&expected.history).unwrap()
            );
            assert_eq!(actual.latest_api_usage, expected.latest_api_usage);
            assert_eq!(actual.after_compaction, expected.after_compaction);
        }
        drop(opened);
        let reopened = session(&path, Some("differential-replay")).await;
        let expected = eager_model_context(&events, "provider-b", "model-b");
        let actual = reopened.model_context("provider-b", "model-b").await;
        assert_eq!(actual.history, expected.history);
        assert_eq!(actual.latest_api_usage, expected.latest_api_usage);
        assert_eq!(actual.after_compaction, expected.after_compaction);
        assert_eq!(actual.latest_api_usage, Some((5, 35)));
    }

    #[tokio::test]
    async fn resume_index_fallbacks_rebuild_without_changing_authoritative_events() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("sessions.db");
        let opened = session(&path, Some("index-fallbacks")).await;
        opened
            .append(EventKind::User {
                text: "hello".into(),
            })
            .await
            .unwrap();
        let first_compaction = opened
            .append_compaction("one".into(), 10, vec![Message::user("one")], false)
            .await
            .unwrap();
        let old_index = opened
            .connection
            .call(|db| {
                db.query_row(
                    "SELECT version, through_sequence, payload_json, checksum
                     FROM session_resume_index WHERE session_id = 'index-fallbacks'",
                    [],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                        ))
                    },
                )
            })
            .await
            .unwrap();
        opened
            .append_batch(vec![
                EventKind::ToolCall {
                    call_id: "help".into(),
                    name: "read".into(),
                    arguments: serde_json::json!({"uri":"file://help","body":""}),
                },
                EventKind::ToolResult {
                    call_id: "help".into(),
                    name: "read".into(),
                    output: "ok".into(),
                    failed: false,
                    protocol_help_required: false,
                },
                EventKind::Task {
                    id: "task".into(),
                    protocol: "bash".into(),
                    label: "work".into(),
                    status: TaskStatus::Completed,
                    output: Some("result".into()),
                },
                EventKind::Task {
                    id: "interrupted".into(),
                    protocol: "bash".into(),
                    label: "partial".into(),
                    status: TaskStatus::Running,
                    output: Some("partial output".into()),
                },
                EventKind::Task {
                    id: "interrupted".into(),
                    protocol: "bash".into(),
                    label: "partial".into(),
                    status: TaskStatus::Running,
                    output: None,
                },
                EventKind::ToolCall {
                    call_id: "failed-help".into(),
                    name: "read".into(),
                    arguments: serde_json::json!({"uri":"grep://help","body":""}),
                },
                EventKind::ToolResult {
                    call_id: "failed-help".into(),
                    name: "read".into(),
                    output: "failed".into(),
                    failed: true,
                    protocol_help_required: false,
                },
                EventKind::ToolCall {
                    call_id: "reused".into(),
                    name: "read".into(),
                    arguments: serde_json::json!({"uri":"tasks://help","body":""}),
                },
            ])
            .await
            .unwrap();
        opened
            .append_compaction("two".into(), 20, vec![Message::user("two")], false)
            .await
            .unwrap();
        let latest_index = opened
            .connection
            .call(|db| {
                db.query_row(
                    "SELECT version, through_sequence, payload_json, checksum
                     FROM session_resume_index
                     WHERE session_id = 'index-fallbacks'",
                    [],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                        ))
                    },
                )
            })
            .await
            .unwrap();
        assert!(!latest_index.2.contains("result"));
        opened
            .append(EventKind::ModelMessage {
                message: Message::assistant("tail"),
            })
            .await
            .unwrap();
        opened
            .append(EventKind::ToolCall {
                call_id: "reused".into(),
                name: "exec".into(),
                arguments: serde_json::json!({}),
            })
            .await
            .unwrap();
        opened
            .connection
            .call(|db| {
                db.execute(
                    "UPDATE sessions SET provider = 'stale', model = 'stale', thinking = 'off'
                     WHERE id = 'index-fallbacks'",
                    [],
                )?;
                Ok::<_, tokio_rusqlite::rusqlite::Error>(())
            })
            .await
            .unwrap();
        let authoritative_events = opened
            .connection
            .call(|db| {
                let mut statement = db.prepare(
                    "SELECT sequence, payload_json FROM events
                     WHERE session_id = 'index-fallbacks' ORDER BY sequence",
                )?;
                statement
                    .query_map([], |row| {
                        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                    })?
                    .collect::<Result<Vec<_>, _>>()
            })
            .await
            .unwrap();
        let expected_events = opened.snapshot().await.unwrap();
        let mut expected_derived = ResumeState::default();
        for event in &expected_events {
            apply_resume_event(&mut expected_derived, event);
        }
        let expected_context = eager_model_context(&expected_events, "test", "model");
        drop(opened);

        enum Corruption {
            Absent,
            UnknownVersion,
            InvalidVersionType,
            InvalidThroughType,
            InvalidPayloadType,
            InvalidChecksumType,
            Malformed,
            SemanticPayload,
            ImpossibleHead,
            Stale,
        }
        for corruption in [
            Corruption::Absent,
            Corruption::UnknownVersion,
            Corruption::InvalidVersionType,
            Corruption::InvalidThroughType,
            Corruption::InvalidPayloadType,
            Corruption::InvalidChecksumType,
            Corruption::Malformed,
            Corruption::SemanticPayload,
            Corruption::ImpossibleHead,
            Corruption::Stale,
        ] {
            let database_path = path.clone();
            let old_index = old_index.clone();
            let latest_index = latest_index.clone();
            let mutator = Connection::open(&database_path).await.unwrap();
            mutator
                .call(move |db| {
                    db.execute(
                        "INSERT INTO session_resume_index
                         (session_id, version, through_sequence, payload_json, checksum)
                         VALUES ('index-fallbacks', ?1, ?2, ?3, ?4)
                         ON CONFLICT(session_id) DO UPDATE SET version=excluded.version,
                           through_sequence=excluded.through_sequence,
                           payload_json=excluded.payload_json, checksum=excluded.checksum",
                        params![
                            latest_index.0,
                            latest_index.1,
                            latest_index.2,
                            latest_index.3
                        ],
                    )?;
                    match corruption {
                        Corruption::Absent => {
                            db.execute("DELETE FROM session_resume_index WHERE session_id = 'index-fallbacks'", [])?;
                        }
                        Corruption::UnknownVersion => {
                            db.execute("UPDATE session_resume_index SET version = 999 WHERE session_id = 'index-fallbacks'", [])?;
                        }
                        Corruption::InvalidVersionType => {
                            db.execute("UPDATE session_resume_index SET version = 'bad' WHERE session_id = 'index-fallbacks'", [])?;
                        }
                        Corruption::InvalidThroughType => {
                            db.execute("UPDATE session_resume_index SET through_sequence = 'bad' WHERE session_id = 'index-fallbacks'", [])?;
                        }
                        Corruption::InvalidPayloadType => {
                            db.execute("UPDATE session_resume_index SET payload_json = x'00' WHERE session_id = 'index-fallbacks'", [])?;
                        }
                        Corruption::InvalidChecksumType => {
                            db.execute("UPDATE session_resume_index SET checksum = x'00' WHERE session_id = 'index-fallbacks'", [])?;
                        }
                        Corruption::Malformed => {
                            db.execute("UPDATE session_resume_index SET payload_json = '{bad' WHERE session_id = 'index-fallbacks'", [])?;
                        }
                        Corruption::SemanticPayload => {
                            let payload: String = db.query_row(
                                "SELECT payload_json FROM session_resume_index
                                 WHERE session_id = 'index-fallbacks'",
                                [],
                                |row| row.get(0),
                            )?;
                            let mut payload = serde_json::from_str::<Value>(&payload).unwrap();
                            payload["has_user"] = Value::Bool(false);
                            db.execute(
                                "UPDATE session_resume_index SET payload_json = ?1
                                 WHERE session_id = 'index-fallbacks'",
                                [payload.to_string()],
                            )?;
                        }
                        Corruption::ImpossibleHead => {
                            db.execute("UPDATE session_resume_index SET through_sequence = 999999 WHERE session_id = 'index-fallbacks'", [])?;
                        }
                        Corruption::Stale => {
                            db.execute(
                                "INSERT INTO session_resume_index
                                 (session_id, version, through_sequence, payload_json, checksum)
                                 VALUES ('index-fallbacks', ?1, ?2, ?3, ?4)
                                 ON CONFLICT(session_id) DO UPDATE SET version=excluded.version,
                                   through_sequence=excluded.through_sequence,
                                   payload_json=excluded.payload_json, checksum=excluded.checksum",
                                params![old_index.0, old_index.1, old_index.2, old_index.3],
                            )?;
                        }
                    }
                    Ok::<_, tokio_rusqlite::rusqlite::Error>(())
                })
                .await
                .unwrap();
            drop(mutator);

            let resumed = session(&path, Some("index-fallbacks")).await;
            assert_eq!(resumed.state.lock().await.derived, expected_derived);
            let actual = resumed.model_context("test", "model").await;
            assert_eq!(actual.history, expected_context.history);
            assert_eq!(actual.latest_api_usage, expected_context.latest_api_usage);
            let reports = resumed.task_reports().await.unwrap();
            assert_eq!(
                reports
                    .iter()
                    .find(|report| report.id == "task")
                    .unwrap()
                    .content,
                b"result"
            );
            let interrupted = reports
                .iter()
                .find(|report| report.id == "interrupted")
                .unwrap();
            assert_eq!(interrupted.status, TaskStatus::Cancelled);
            assert_eq!(interrupted.content, b"partial output");
            let help_reads = resumed.successful_protocol_help_reads().await;
            assert!(help_reads.contains("file"));
            assert!(!help_reads.contains("grep"));
            assert!(!help_reads.contains("tasks"));
            assert!(
                !resumed
                    .state
                    .lock()
                    .await
                    .derived
                    .pending_help_reads
                    .contains_key("reused")
            );
            assert!(resumed.has_user_message().await);
            assert_eq!(
                resumed.model_settings().await,
                SessionModelSettings {
                    provider: "test".into(),
                    model: "model".into(),
                    thinking: ThinkingLevel::Off,
                }
            );
            let after = resumed
                .connection
                .call(|db| {
                    let mut statement = db.prepare(
                        "SELECT sequence, payload_json FROM events
                         WHERE session_id = 'index-fallbacks' ORDER BY sequence",
                    )?;
                    statement
                        .query_map([], |row| {
                            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                        })?
                        .collect::<Result<Vec<_>, _>>()
                })
                .await
                .unwrap();
            assert_eq!(after, authoritative_events);
            drop(resumed);
        }
        assert!(first_compaction.sequence < expected_events.last().unwrap().sequence);
    }

    #[tokio::test]
    async fn paging_cursors_concatenate_without_gaps_and_include_later_appends_only_after_cursor() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("sessions.db");
        let opened = session(&path, Some("paging")).await;
        opened
            .append_batch(vec![
                EventKind::User {
                    text: "turn".into(),
                },
                EventKind::ToolCall {
                    call_id: "a".into(),
                    name: "read".into(),
                    arguments: serde_json::json!({}),
                },
                EventKind::Compaction {
                    summary: "mid".into(),
                    tokens_before: 1,
                    replacement_history: vec![],
                    manual: false,
                },
                EventKind::ToolResult {
                    call_id: "a".into(),
                    name: "read".into(),
                    output: "done".into(),
                    failed: false,
                    protocol_help_required: false,
                },
                EventKind::TurnFinished,
            ])
            .await
            .unwrap();
        let initial = opened.snapshot().await.unwrap();
        let first = opened.events_after(0, 2).await.unwrap();
        opened
            .append(EventKind::Notice {
                text: "later".into(),
            })
            .await
            .unwrap();
        let mut forward = first;
        loop {
            let cursor = forward.last().unwrap().sequence;
            let page = opened.events_after(cursor, 2).await.unwrap();
            if page.is_empty() {
                break;
            }
            forward.extend(page);
        }
        let all = opened.snapshot().await.unwrap();
        assert_eq!(forward, all.into_iter().skip(1).collect::<Vec<_>>());

        let boundary = initial.last().unwrap().sequence.saturating_add(1);
        let mut backward = Vec::new();
        let mut cursor = boundary;
        loop {
            let page = opened.events_before(cursor, 2).await.unwrap();
            if page.is_empty() {
                break;
            }
            cursor = page[0].sequence;
            backward.splice(0..0, page);
        }
        assert_eq!(backward, initial);
        let snapshot = opened.snapshot().await.unwrap();
        assert_eq!(
            opened.tail_events(3).await.unwrap(),
            snapshot[snapshot.len() - 3..]
        );
        assert!(opened.events_after(0, 10_000).await.unwrap().len() <= MAX_EVENT_PAGE);
    }

    #[tokio::test]
    async fn cache_failure_does_not_block_or_publish_ahead_of_authoritative_append() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("sessions.db");
        let opened = session(&path, Some("cache-failure")).await;
        opened
            .append(EventKind::User {
                text: "start".into(),
            })
            .await
            .unwrap();
        opened
            .connection
            .call(|db| {
                db.execute_batch(
                    "CREATE TRIGGER reject_resume_index
                     BEFORE INSERT ON session_resume_index
                     BEGIN SELECT RAISE(ABORT, 'cache unavailable'); END;",
                )?;
                Ok::<_, tokio_rusqlite::rusqlite::Error>(())
            })
            .await
            .unwrap();
        let mut updates = opened.subscribe();
        let event = opened
            .append_compaction("safe".into(), 1, vec![Message::user("safe")], false)
            .await
            .unwrap();
        let published = updates.recv().await.unwrap();
        assert!(
            matches!(published, SessionUpdate::Persisted(ref saved) if saved.sequence == event.sequence)
        );
        let stored_head = opened
            .connection
            .call(|db| {
                db.query_row(
                    "SELECT head_sequence FROM sessions WHERE id = 'cache-failure'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
            })
            .await
            .unwrap();
        assert_eq!(stored_head as u64, event.sequence);
        assert!(matches!(
            opened
                .snapshot()
                .await
                .unwrap()
                .last()
                .map(|event| &event.kind),
            Some(EventKind::Compaction { .. })
        ));
        drop(opened);
        let reopened = session(&path, Some("cache-failure")).await;
        assert_eq!(reopened.model_history().await, vec![Message::user("safe")]);
    }

    #[tokio::test]
    async fn concurrent_appends_publish_in_sequence_order() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("sessions.db");
        let opened = session(&path, Some("ordered-publication")).await;
        opened
            .append(EventKind::User {
                text: "start".into(),
            })
            .await
            .unwrap();
        let mut updates = opened.subscribe();
        let barrier = Arc::new(tokio::sync::Barrier::new(17));
        let mut appends = tokio::task::JoinSet::new();
        for index in 0..16 {
            let session = opened.clone();
            let barrier = barrier.clone();
            appends.spawn(async move {
                barrier.wait().await;
                let kind = if index % 2 == 0 {
                    EventKind::Compaction {
                        summary: format!("checkpoint {index}"),
                        tokens_before: index,
                        replacement_history: vec![Message::user(format!("summary {index}"))],
                        manual: false,
                    }
                } else {
                    EventKind::Notice {
                        text: format!("notice {index}"),
                    }
                };
                session.append(kind).await.unwrap()
            });
        }
        barrier.wait().await;

        let mut published = Vec::new();
        while published.len() < 16 {
            if let SessionUpdate::Persisted(event) = updates.recv().await.unwrap() {
                published.push(event.sequence);
            }
        }
        while appends.join_next().await.is_some() {}
        assert!(published.windows(2).all(|pair| pair[1] == pair[0] + 1));
    }

    #[tokio::test]
    async fn delayed_older_checkpoint_cannot_replace_a_newer_resume_index() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("sessions.db");
        let opened = session(&path, Some("monotonic-index")).await;
        opened
            .append(EventKind::User {
                text: "start".into(),
            })
            .await
            .unwrap();
        let first = opened
            .append_compaction("first".into(), 1, vec![Message::user("first")], false)
            .await
            .unwrap();
        let first_payload = opened
            .connection
            .call(|db| {
                db.query_row(
                    "SELECT payload_json FROM session_resume_index
                     WHERE session_id = 'monotonic-index'",
                    [],
                    |row| row.get::<_, String>(0),
                )
            })
            .await
            .unwrap();
        let second = opened
            .append_compaction("second".into(), 2, vec![Message::user("second")], false)
            .await
            .unwrap();

        opened
            .persist_resume_index(first.sequence, first_payload.clone())
            .await;
        opened
            .connection
            .call(move |db| {
                persist_rebuilt_resume_index(db, "monotonic-index", first.sequence, &first_payload)
            })
            .await
            .unwrap();
        let through = opened
            .connection
            .call(|db| {
                db.query_row(
                    "SELECT through_sequence FROM session_resume_index
                     WHERE session_id = 'monotonic-index'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
            })
            .await
            .unwrap();
        assert_eq!(through as u64, second.sequence);
    }

    #[tokio::test]
    async fn compacted_resume_keeps_only_tail_events_and_repeated_context_is_in_memory() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("sessions.db");
        let opened = session(&path, Some("bounded-resume")).await;
        let large = "x".repeat(100_000);
        opened
            .append_batch(vec![
                EventKind::User {
                    text: "start".into(),
                },
                EventKind::AssistantText { text: large },
                EventKind::ModelMessage {
                    message: Message::user("old"),
                },
            ])
            .await
            .unwrap();
        let checkpoint = opened
            .append_compaction(
                "bounded".into(),
                25_000,
                vec![Message::user("summary")],
                false,
            )
            .await
            .unwrap();
        opened
            .append(EventKind::ModelMessage {
                message: Message::assistant("tail"),
            })
            .await
            .unwrap();
        drop(opened);

        let resumed = session(&path, Some("bounded-resume")).await;
        let in_memory = resumed.state.lock().await.events.clone();
        assert_eq!(in_memory.len(), 1);
        assert!(
            in_memory
                .iter()
                .all(|event| event.sequence > checkpoint.sequence)
        );
        let first = resumed.model_context("test", "model").await;
        let second = resumed.model_context("test", "model").await;
        assert_eq!(first.history, second.history);
        assert_eq!(
            first.history,
            vec![Message::user("summary"), Message::assistant("tail")]
        );
    }

    #[tokio::test]
    async fn latest_selects_most_recently_updated_session() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("sessions.db");
        let older = session(&path, Some("older")).await;
        older
            .append(EventKind::User {
                text: "older".into(),
            })
            .await
            .unwrap();
        let newer = session(&path, Some("newer")).await;
        newer
            .append(EventKind::User {
                text: "newer".into(),
            })
            .await
            .unwrap();
        older
            .append(EventKind::Notice {
                text: "updated last".into(),
            })
            .await
            .unwrap();
        let latest = session(&path, Some("latest")).await;
        assert_eq!(latest.id(), older.id());
        drop(newer);
    }

    #[tokio::test]
    async fn resume_index_schema_adds_checksum_to_legacy_table() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("sessions.db");
        {
            let db = SqliteConnection::open(&path).unwrap();
            db.execute_batch(
                "CREATE TABLE session_resume_index (
                   session_id TEXT PRIMARY KEY,
                   version INTEGER NOT NULL,
                   through_sequence INTEGER NOT NULL,
                   payload_json TEXT NOT NULL
                 );",
            )
            .unwrap();
        }

        let (_, connection) = open_database(path).await.unwrap();
        let columns = connection
            .call(|db| {
                let mut statement = db.prepare("PRAGMA table_info(session_resume_index)")?;
                statement
                    .query_map([], |row| row.get::<_, String>(1))?
                    .collect::<Result<Vec<_>, _>>()
            })
            .await
            .unwrap();
        assert!(columns.iter().any(|column| column == "checksum"));
    }

    #[test]
    fn macos_session_database_uses_the_config_directory() {
        let path = session_database_path_from(
            Some(PathBuf::from("/Users/ada/.config/uri-agent")),
            Some(Path::new("/Users/ada/Library/Application Support")),
            Path::new("/work"),
        );
        assert_eq!(
            path,
            PathBuf::from("/Users/ada/.config/uri-agent").join(SESSION_DATABASE_FILE)
        );
    }

    #[test]
    fn platform_session_database_uses_the_data_directory() {
        let path = session_database_path_from(
            None,
            Some(Path::new("/home/ada/.local/share")),
            Path::new("/work"),
        );
        assert_eq!(
            path,
            PathBuf::from("/home/ada/.local/share")
                .join("uri-agent")
                .join(SESSION_DATABASE_FILE)
        );
    }

    #[tokio::test]
    async fn new_session_is_not_persisted_until_the_first_user_message() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("sessions.db");
        let opened = session(&path, Some("deferred")).await;
        opened
            .append(EventKind::Notice {
                text: "startup notice".into(),
            })
            .await
            .unwrap();
        opened
            .update_new_model_settings("changed", "next-model", ThinkingLevel::Off)
            .await
            .unwrap();
        let before: i64 = opened
            .connection
            .call(|db| db.query_row("SELECT count(*) FROM sessions", [], |row| row.get(0)))
            .await
            .unwrap();
        assert_eq!(before, 0);

        opened
            .append(EventKind::User {
                text: "hello".into(),
            })
            .await
            .unwrap();
        let persisted: (i64, i64, String, String) = opened
            .connection
            .call(|db| {
                Ok::<_, tokio_rusqlite::rusqlite::Error>((
                    db.query_row("SELECT count(*) FROM sessions", [], |row| row.get(0))?,
                    db.query_row("SELECT count(*) FROM events", [], |row| row.get(0))?,
                    db.query_row("SELECT provider FROM sessions", [], |row| row.get(0))?,
                    db.query_row("SELECT model FROM sessions", [], |row| row.get(0))?,
                ))
            })
            .await
            .unwrap();
        assert_eq!(persisted, (1, 4, "changed".into(), "next-model".into()));
    }

    #[tokio::test]
    async fn failed_first_pending_input_rolls_back_session_creation_and_model_freeze() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("sessions.db");
        let opened = session(&path, Some("failed-first-pending")).await;
        opened
            .connection
            .call(|db| {
                db.execute_batch(
                    "CREATE TRIGGER fail_pending BEFORE INSERT ON pending_inputs
                     BEGIN SELECT RAISE(FAIL, 'fail pending'); END;",
                )?;
                Ok::<_, tokio_rusqlite::rusqlite::Error>(())
            })
            .await
            .unwrap();

        let error = opened
            .add_pending_input(
                SubmitKind::Prompt,
                "hello",
                &[UserContent::text("hello")],
                true,
            )
            .await
            .unwrap_err();

        assert!(format!("{error:#}").contains("fail pending"));
        assert!(!opened.is_persisted().await);
        let counts: (i64, i64, i64) = opened
            .connection
            .call(|db| {
                Ok::<_, tokio_rusqlite::rusqlite::Error>((
                    db.query_row("SELECT count(*) FROM sessions", [], |row| row.get(0))?,
                    db.query_row("SELECT count(*) FROM events", [], |row| row.get(0))?,
                    db.query_row("SELECT count(*) FROM pending_inputs", [], |row| row.get(0))?,
                ))
            })
            .await
            .unwrap();
        assert_eq!(counts, (0, 0, 0));
        opened
            .update_new_model_settings("changed", "next-model", ThinkingLevel::Off)
            .await
            .unwrap();
        assert_eq!(opened.model_settings().await.provider, "changed");
        assert_eq!(opened.model_settings().await.model, "next-model");
    }

    #[tokio::test]
    async fn an_explicit_session_cannot_cross_project_boundaries() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("sessions.db");
        let other = Session::open_at(
            path.clone(),
            Some("other-project"),
            Path::new("/projects/other"),
            "test",
            "model",
            context("other"),
        )
        .await
        .unwrap();
        other
            .append(EventKind::User {
                text: "other project".into(),
            })
            .await
            .unwrap();

        let error = match Session::open_at(
            path,
            Some("other-project"),
            Path::new("/projects/current"),
            "test",
            "model",
            context("current"),
        )
        .await
        {
            Ok(_) => panic!("cross-project session unexpectedly opened"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("different project"));
    }

    #[tokio::test]
    async fn drafts_persist_without_changing_event_history() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("sessions.db");
        let opened = session(&path, Some("drafty")).await;
        opened
            .append(EventKind::User {
                text: "existing turn".into(),
            })
            .await
            .unwrap();
        opened.save_draft("keep me").await.unwrap();
        drop(opened);
        let reopened = session(&path, Some("drafty")).await;
        assert_eq!(reopened.draft().await, "keep me");
        assert_eq!(reopened.snapshot().await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn pending_draft_persists_without_creating_a_session() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("sessions.db");
        let opened = session(&path, Some("pending-draft")).await;
        opened.save_draft("keep me").await.unwrap();
        drop(opened);

        let reopened = session(&path, Some("another-new-session")).await;
        assert_eq!(reopened.draft().await, "keep me");
        let count: i64 = reopened
            .connection
            .call(|db| db.query_row("SELECT count(*) FROM sessions", [], |row| row.get(0)))
            .await
            .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn project_session_list_stays_inside_the_launch_directory() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("sessions.db");
        let first = session(&path, Some("alpha")).await;
        first
            .append(EventKind::User {
                text: "first question".into(),
            })
            .await
            .unwrap();
        session(&path, Some("beta")).await;
        let other = Session::open_at(
            path.clone(),
            Some("other"),
            Path::new("/projects/other"),
            "test",
            "model",
            context("other"),
        )
        .await
        .unwrap();
        other
            .append(EventKind::User {
                text: "other question".into(),
            })
            .await
            .unwrap();
        let listed = first.list_for_project().await.unwrap();
        let ids = listed
            .iter()
            .map(|item| item.id.as_str())
            .collect::<Vec<_>>();
        assert!(ids.contains(&"alpha"));
        assert!(!ids.contains(&"beta"));
        assert!(!ids.contains(&"other"));
        assert_eq!(
            listed
                .iter()
                .find(|item| item.id == "alpha")
                .unwrap()
                .preview,
            "first question"
        );
    }

    #[tokio::test]
    async fn child_agents_persist_in_the_session_database_but_stay_out_of_tui_lists() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("sessions.db");
        let root = Session::open_at_with_spec(
            path.clone(),
            Some("root-agent"),
            temp.path(),
            AgentSpec::root("provider", "root-model", ThinkingLevel::Off, temp.path()),
            Some(context("root")),
        )
        .await
        .unwrap();
        root.append(EventKind::User {
            text: "root work".into(),
        })
        .await
        .unwrap();

        let mut child_spec = AgentSpec::new(
            "provider",
            "child-model",
            ThinkingLevel::Medium,
            temp.path(),
            "root-agent",
        );
        child_spec.assign_depth(2);
        let child = Session::open_at_with_spec(
            path,
            Some("child-agent"),
            temp.path(),
            child_spec,
            Some(context("child")),
        )
        .await
        .unwrap();
        child
            .append(EventKind::User {
                text: "background work".into(),
            })
            .await
            .unwrap();

        assert_eq!(child.spec().await.depth(), 2);
        assert_eq!(
            child.spec().await.parent_session_id.as_deref(),
            Some("root-agent")
        );
        assert_eq!(
            root.list_for_project()
                .await
                .unwrap()
                .into_iter()
                .map(|session| session.id)
                .collect::<Vec<_>>(),
            ["root-agent"]
        );
        let stored: (i64, Option<String>) = child
            .connection
            .call(|db| {
                db.query_row(
                    "SELECT depth, parent_session_id FROM sessions WHERE id = 'child-agent'",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
            })
            .await
            .unwrap();
        assert_eq!(stored, (2, Some("root-agent".to_string())));
    }

    #[tokio::test]
    async fn model_settings_freeze_on_first_submission_and_restore_on_resume() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("sessions.db");
        let original = Session::open_at_with_thinking(
            path.clone(),
            Some("settings"),
            Path::new("/work"),
            "openai",
            "gpt-old",
            ThinkingLevel::High,
            Some(context("settings")),
        )
        .await
        .unwrap();
        original
            .update_new_model_settings("anthropic", "claude-new", ThinkingLevel::Medium)
            .await
            .unwrap();
        original
            .append(EventKind::User {
                text: "remember the model".into(),
            })
            .await
            .unwrap();
        assert!(
            original
                .snapshot()
                .await
                .unwrap()
                .iter()
                .any(|event| matches!(
                    &event.kind,
                    EventKind::SessionCreated { spec }
                        if spec.provider == "anthropic"
                            && spec.model == "claude-new"
                            && spec.thinking == ThinkingLevel::Medium
                ))
        );
        assert!(
            original
                .update_new_model_settings("other", "other", ThinkingLevel::Off)
                .await
                .is_err()
        );
        drop(original);

        let resumed = Session::open_at_with_thinking(
            path,
            Some("settings"),
            Path::new("/work"),
            "different-default",
            "different-model",
            ThinkingLevel::Off,
            Some(context("ignored")),
        )
        .await
        .unwrap();
        assert_eq!(
            resumed.model_settings().await,
            SessionModelSettings {
                provider: "anthropic".into(),
                model: "claude-new".into(),
                thinking: ThinkingLevel::Medium,
            }
        );
        let summary = resumed.list_for_project().await.unwrap().remove(0);
        assert_eq!(summary.provider, "anthropic");
        assert_eq!(summary.model, "claude-new");
        assert_eq!(summary.thinking, ThinkingLevel::Medium);
    }

    #[test]
    fn session_created_event_requires_thinking() {
        let result = serde_json::from_value::<EventKind>(serde_json::json!({
            "kind": "session_created",
            "cwd": "/work",
            "provider": "openai",
            "model": "gpt"
        }));
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn session_without_a_creation_event_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("sessions.db");
        let original = session(&path, Some("missing-created")).await;
        original
            .append(EventKind::User {
                text: "persist".into(),
            })
            .await
            .unwrap();
        original
            .connection
            .call(|db| {
                db.execute(
                    "DELETE FROM events
                     WHERE session_id = 'missing-created' AND kind = 'session_created'",
                    [],
                )?;
                Ok::<_, tokio_rusqlite::rusqlite::Error>(())
            })
            .await
            .unwrap();
        drop(original);

        let error = match Session::open_at(
            path,
            Some("missing-created"),
            Path::new("/work"),
            "test",
            "model",
            context("ignored"),
        )
        .await
        {
            Ok(_) => panic!("session without a creation event was resumed"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("has no creation event"));
    }

    #[tokio::test]
    async fn related_transcript_and_replay_events_rollback_together() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("sessions.db");
        let opened = session(&path, Some("atomic")).await;
        opened
            .connection
            .call(|db| {
                db.execute_batch(
                    "CREATE TRIGGER reject_model_message
                     BEFORE INSERT ON events
                     WHEN NEW.kind = 'model_message'
                     BEGIN
                       SELECT RAISE(ABORT, 'simulated crash boundary');
                     END;",
                )?;
                Ok::<_, tokio_rusqlite::rusqlite::Error>(())
            })
            .await
            .unwrap();

        let result = opened
            .append_batch(vec![
                EventKind::User {
                    text: "must be atomic".into(),
                },
                EventKind::ModelMessage {
                    message: Message::user("must be atomic"),
                },
            ])
            .await;
        assert!(result.is_err());
        assert!(
            !opened
                .snapshot()
                .await
                .unwrap()
                .iter()
                .any(|event| matches!(
                    event.kind,
                    EventKind::User { .. } | EventKind::ModelMessage { .. }
                ))
        );
        let counts: (i64, i64) = opened
            .connection
            .call(|db| {
                Ok::<_, tokio_rusqlite::rusqlite::Error>((
                    db.query_row("SELECT count(*) FROM sessions", [], |row| row.get(0))?,
                    db.query_row("SELECT count(*) FROM events", [], |row| row.get(0))?,
                ))
            })
            .await
            .unwrap();
        assert_eq!(counts, (0, 0));
    }
}
