use crate::catalog::ThinkingLevel;
use crate::skill::SkillSnapshot;
use crate::task::TaskStatus;
use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use rig::message::Message;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::fs;
use tokio::sync::{Mutex, broadcast};
use tokio_rusqlite::{
    Connection,
    rusqlite::{OpenFlags, OptionalExtension, params},
};
use uuid::Uuid;

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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionModelSettings {
    pub provider: String,
    pub model: String,
    pub thinking: ThinkingLevel,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SessionEvent {
    pub sequence: u64,
    pub at: DateTime<Utc>,
    #[serde(flatten)]
    pub kind: EventKind,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SessionContext {
    pub system_prompt: String,
    pub skills: Vec<SkillSnapshot>,
}

#[derive(Clone, Debug)]
pub enum SessionUpdate {
    Persisted(SessionEvent),
    Transient(EventKind),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EventKind {
    SessionCreated {
        cwd: PathBuf,
        provider: String,
        model: String,
        #[serde(default)]
        thinking: ThinkingLevel,
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
    },
    ModelMessage {
        message: Message,
    },
    ModelSettingsChanged {
        provider: String,
        model: String,
        thinking: ThinkingLevel,
    },
    Task {
        id: String,
        protocol: String,
        label: String,
        status: TaskStatus,
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
        cache_read: u64,
        cache_write: u64,
        cost: f64,
        /// Provider-reported total tokens for the completed request, before
        /// any API-specific normalization used for price accounting.
        #[serde(default)]
        total: u64,
        /// Whether this usage belongs to a successful ordinary assistant
        /// message and is therefore valid as a context-meter baseline.
        #[serde(default = "default_true")]
        context: bool,
        #[serde(default)]
        provider: String,
        #[serde(default)]
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
    model_settings: SessionModelSettings,
}

fn default_true() -> bool {
    true
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
}

impl Session {
    pub async fn open(
        requested: Option<&str>,
        cwd: &Path,
        provider: &str,
        model: &str,
        thinking: ThinkingLevel,
        context: SessionContext,
    ) -> Result<Self> {
        Self::open_at_with_thinking(
            session_database_path(cwd),
            requested,
            cwd,
            provider,
            model,
            thinking,
            context,
        )
        .await
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
            context,
        )
        .await
    }

    async fn open_at_with_thinking(
        database_path: PathBuf,
        requested: Option<&str>,
        cwd: &Path,
        provider: &str,
        model: &str,
        thinking: ThinkingLevel,
        context: SessionContext,
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
                            "SELECT id FROM sessions WHERE cwd = ?1
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
        let belongs_to_project = connection
            .call(move |db| {
                db.query_row(
                    "SELECT cwd = ?2 FROM sessions WHERE id = ?1",
                    params![id_for_lookup, project_for_lookup],
                    |row| row.get::<_, bool>(0),
                )
                .optional()
            })
            .await
            .context("cannot validate session project")?;
        if belongs_to_project == Some(false) {
            return Err(anyhow!("session {id} belongs to a different project"));
        }

        let lookup_id = id.clone();
        let existing = connection
            .call(move |db| {
                let mut statement = db.prepare(
                    "SELECT sequence, at, payload_json FROM events
                 WHERE session_id = ?1 ORDER BY sequence",
                )?;
                let rows = statement.query_map([lookup_id], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })?;
                let mut events = Vec::new();
                for row in rows {
                    let (sequence, at, payload): (i64, String, String) = row?;
                    events.push(SessionEvent {
                        sequence: sequence as u64,
                        at: DateTime::parse_from_rfc3339(&at)
                            .map_err(|e| {
                                tokio_rusqlite::rusqlite::Error::FromSqlConversionFailure(
                                    2,
                                    tokio_rusqlite::rusqlite::types::Type::Text,
                                    Box::new(e),
                                )
                            })?
                            .with_timezone(&Utc),
                        kind: serde_json::from_str(&payload).map_err(|e| {
                            tokio_rusqlite::rusqlite::Error::FromSqlConversionFailure(
                                4,
                                tokio_rusqlite::rusqlite::types::Type::Text,
                                Box::new(e),
                            )
                        })?,
                    });
                }
                Ok::<_, tokio_rusqlite::rusqlite::Error>(events)
            })
            .await
            .context("cannot restore session events")?;

        let mut existing = existing;
        let created_session = existing.is_empty();
        if created_session {
            let at = Utc::now();
            let created = EventKind::SessionCreated {
                cwd: cwd.to_path_buf(),
                provider: provider.to_string(),
                model: model.to_string(),
                thinking,
            };
            let frozen = EventKind::SessionContext { context };
            existing.extend([
                SessionEvent {
                    sequence: 0,
                    at,
                    kind: created,
                },
                SessionEvent {
                    sequence: 1,
                    at,
                    kind: frozen,
                },
            ]);
        }
        if !existing
            .iter()
            .any(|event| matches!(event.kind, EventKind::SessionContext { .. }))
        {
            return Err(anyhow!(
                "session {id} has no frozen context and cannot be resumed"
            ));
        }

        let stored_settings = if created_session {
            None
        } else {
            let settings_id = id.clone();
            connection
                .call(move |db| {
                    db.query_row(
                        "SELECT provider, model, thinking FROM sessions WHERE id = ?1",
                        [settings_id],
                        |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, String>(1)?,
                                row.get::<_, String>(2)?,
                            ))
                        },
                    )
                    .optional()
                })
                .await
                .context("cannot restore session model settings")?
                .map(|(provider, model, thinking)| SessionModelSettings {
                    provider,
                    model,
                    thinking: thinking.parse().unwrap_or_default(),
                })
        };
        let fallback_settings = stored_settings.unwrap_or_else(|| SessionModelSettings {
            provider: provider.to_string(),
            model: model.to_string(),
            thinking,
        });
        let model_settings = model_settings_from_events(&existing, fallback_settings);
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
                model_settings,
            })),
            events,
        };
        Ok(session)
    }

    pub fn id(&self) -> &str {
        &self.id
    }
    pub fn is_new(&self) -> bool {
        self.created
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

    pub async fn snapshot(&self) -> Vec<SessionEvent> {
        self.state.lock().await.events.clone()
    }

    pub async fn model_settings(&self) -> SessionModelSettings {
        self.state.lock().await.model_settings.clone()
    }

    pub async fn context(&self) -> SessionContext {
        self.state
            .lock()
            .await
            .events
            .iter()
            .find_map(|event| match &event.kind {
                EventKind::SessionContext { context } => Some(context.clone()),
                _ => None,
            })
            .expect("session context is validated when the session opens")
    }

    pub async fn model_history(&self) -> Vec<Message> {
        self.model_context("", "").await.history
    }

    pub(crate) async fn model_context(&self, provider: &str, model: &str) -> ModelContext {
        let state = self.state.lock().await;
        let latest_compaction = state
            .events
            .iter()
            .rposition(|event| matches!(event.kind, EventKind::Compaction { .. }));
        let mut history = latest_compaction
            .and_then(|index| match &state.events[index].kind {
                EventKind::Compaction {
                    replacement_history,
                    ..
                } => Some(replacement_history.clone()),
                _ => None,
            })
            .unwrap_or_default();
        let mut latest_api_usage = None;
        let mut pending_usage = None;
        for event in state
            .events
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

    pub async fn latest_compaction_summary(&self) -> Option<String> {
        self.state
            .lock()
            .await
            .events
            .iter()
            .rev()
            .find_map(|event| match &event.kind {
                EventKind::Compaction { summary, .. } => Some(summary.clone()),
                _ => None,
            })
    }

    pub async fn append_compaction(
        &self,
        summary: String,
        tokens_before: usize,
        replacement_history: Vec<Message>,
        manual: bool,
    ) -> Result<SessionEvent> {
        self.append(EventKind::Compaction {
            summary,
            tokens_before,
            replacement_history,
            manual,
        })
        .await
    }

    pub async fn update_model_settings(
        &self,
        provider: &str,
        model: &str,
        thinking: ThinkingLevel,
    ) -> Result<()> {
        let requested = SessionModelSettings {
            provider: provider.to_string(),
            model: model.to_string(),
            thinking,
        };
        if self.state.lock().await.model_settings == requested {
            return Ok(());
        }
        self.append(EventKind::ModelSettingsChanged {
            provider: requested.provider,
            model: requested.model,
            thinking: requested.thinking,
        })
        .await
        .context("cannot update session model settings")?;
        Ok(())
    }

    #[cfg(test)]
    async fn update_model(&self, provider: &str, model: &str) -> Result<()> {
        let thinking = self.model_settings().await.thinking;
        self.update_model_settings(provider, model, thinking).await
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
        if kinds.is_empty() {
            return Ok(Vec::new());
        }
        let mut state = self.state.lock().await;
        let at = Utc::now();
        let at_text = at.to_rfc3339();
        let first_sequence = state
            .events
            .last()
            .map_or(0, |event| event.sequence.saturating_add(1));
        let events = kinds
            .into_iter()
            .enumerate()
            .map(|(offset, kind)| SessionEvent {
                sequence: first_sequence.saturating_add(offset as u64),
                at,
                kind,
            })
            .collect::<Vec<_>>();
        let mut next_settings = state.model_settings.clone();
        apply_model_settings(&mut next_settings, events.iter().map(|event| &event.kind));

        if !state.persisted {
            if !events.iter().any(|event| starts_session(&event.kind)) {
                state.events.extend(events.iter().cloned());
                state.model_settings = next_settings;
                drop(state);
                self.publish_persisted(&events);
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
            let provider = next_settings.provider.clone();
            let model = next_settings.model.clone();
            let thinking = next_settings.thinking.to_string();
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
                         (id, created_at, updated_at, cwd, provider, model, thinking, head_sequence, draft)
                         VALUES (?1, ?2, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                        params![
                            id,
                            at_text,
                            project,
                            provider,
                            model,
                            thinking,
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
            state.events.extend(events.iter().cloned());
            state.model_settings = next_settings;
            drop(state);
            self.publish_persisted(&events);
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
        let provider = next_settings.provider.clone();
        let model = next_settings.model.clone();
        let thinking = next_settings.thinking.to_string();
        let expected_head = state
            .events
            .last()
            .map_or(-1, |event| event.sequence as i64);
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
                transaction.commit()?;
                Ok::<_, tokio_rusqlite::rusqlite::Error>(())
            })
            .await
            .context("cannot append session event batch")?;
        state.events.extend(events.iter().cloned());
        state.model_settings = next_settings;
        drop(state);
        self.publish_persisted(&events);
        Ok(events)
    }

    fn publish_persisted(&self, events: &[SessionEvent]) {
        for event in events {
            let _ = self.events.send(SessionUpdate::Persisted(event.clone()));
        }
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
                 FROM sessions WHERE cwd = ?1
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
    dirs::data_dir()
        .map(|path| path.join("uri-agent/sessions.db"))
        .unwrap_or_else(|| fallback.join(".uri-agent/sessions.db"))
}

async fn open_archive_database(path: &Path) -> Result<Option<Connection>> {
    if !fs::try_exists(path)
        .await
        .with_context(|| format!("cannot inspect session database: {}", path.display()))?
    {
        return Ok(None);
    }
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .await
        .map(Some)
        .with_context(|| format!("cannot open session archive: {}", path.display()))
}

async fn open_database(database_path: PathBuf) -> Result<(PathBuf, Connection)> {
    let directory = database_path
        .parent()
        .unwrap_or(Path::new("."))
        .to_path_buf();
    fs::create_dir_all(&directory).await.with_context(|| {
        format!(
            "cannot create session data directory: {}",
            directory.display()
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).await?;
    }
    let connection = Connection::open(&database_path)
        .await
        .with_context(|| format!("cannot open session database: {}", database_path.display()))?;
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
                   thinking TEXT NOT NULL DEFAULT 'off',
                   head_sequence INTEGER NOT NULL,
                   draft TEXT NOT NULL DEFAULT ''
                 );
                 CREATE TABLE IF NOT EXISTS events (
                   session_id TEXT NOT NULL, sequence INTEGER NOT NULL, at TEXT NOT NULL,
                   kind TEXT NOT NULL, payload_json TEXT NOT NULL,
                   PRIMARY KEY(session_id, sequence),
                   FOREIGN KEY(session_id) REFERENCES sessions(id) ON DELETE CASCADE
                 );
                 CREATE TABLE IF NOT EXISTS pending_drafts (
                   cwd TEXT PRIMARY KEY, draft TEXT NOT NULL
                 );",
            )?;
            let has_draft = {
                let mut statement = db.prepare("PRAGMA table_info(sessions)")?;
                let names = statement.query_map([], |row| row.get::<_, String>(1))?;
                names.filter_map(Result::ok).any(|name| name == "draft")
            };
            if !has_draft {
                db.execute(
                    "ALTER TABLE sessions ADD COLUMN draft TEXT NOT NULL DEFAULT ''",
                    [],
                )?;
            }
            let has_thinking = {
                let mut statement = db.prepare("PRAGMA table_info(sessions)")?;
                let names = statement.query_map([], |row| row.get::<_, String>(1))?;
                names.filter_map(Result::ok).any(|name| name == "thinking")
            };
            if !has_thinking {
                db.execute(
                    "ALTER TABLE sessions ADD COLUMN thinking TEXT NOT NULL DEFAULT 'off'",
                    [],
                )?;
            }

            // TODO: Remove this compatibility cleanup after legacy empty sessions have aged out.
            let transaction = db.transaction()?;
            transaction.execute(
                "INSERT INTO pending_drafts (cwd, draft)
                 SELECT legacy.cwd, legacy.draft
                 FROM sessions AS legacy
                 WHERE legacy.draft <> ''
                   AND NOT EXISTS (
                     SELECT 1 FROM events
                     WHERE events.session_id = legacy.id AND events.kind = 'user'
                   )
                   AND NOT EXISTS (
                     SELECT 1 FROM sessions AS newer
                     WHERE newer.cwd = legacy.cwd
                       AND NOT EXISTS (
                         SELECT 1 FROM events
                         WHERE events.session_id = newer.id AND events.kind = 'user'
                       )
                       AND (newer.updated_at > legacy.updated_at
                            OR (newer.updated_at = legacy.updated_at AND newer.id > legacy.id))
                   )
                 ON CONFLICT(cwd) DO UPDATE SET draft = excluded.draft",
                [],
            )?;
            transaction.execute(
                "DELETE FROM sessions
                 WHERE NOT EXISTS (
                   SELECT 1 FROM events
                   WHERE events.session_id = sessions.id AND events.kind = 'user'
                 )",
                [],
            )?;
            transaction.commit()?;
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
        EventKind::ModelSettingsChanged { .. } => "model_settings_changed",
        EventKind::User { .. } => "user",
        EventKind::AssistantText { .. } => "assistant_text",
        EventKind::AssistantReasoning { .. } => "assistant_reasoning",
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

fn apply_model_settings<'a>(
    settings: &mut SessionModelSettings,
    kinds: impl IntoIterator<Item = &'a EventKind>,
) {
    for kind in kinds {
        match kind {
            EventKind::SessionCreated {
                provider,
                model,
                thinking,
                ..
            }
            | EventKind::ModelSettingsChanged {
                provider,
                model,
                thinking,
            } => {
                settings.provider.clone_from(provider);
                settings.model.clone_from(model);
                settings.thinking = *thinking;
            }
            _ => {}
        }
    }
}

fn model_settings_from_events(
    events: &[SessionEvent],
    mut fallback: SessionModelSettings,
) -> SessionModelSettings {
    // Older URI Agent versions updated only the materialized session row. If
    // no append-only change exists, that row remains the compatibility truth
    // rather than the original values in SessionCreated.
    if !events
        .iter()
        .any(|event| matches!(&event.kind, EventKind::ModelSettingsChanged { .. }))
    {
        return fallback;
    }
    apply_model_settings(&mut fallback, events.iter().map(|event| &event.kind));
    fallback
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
        let events = reopened.snapshot().await;
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
    async fn session_without_a_frozen_context_is_not_reinterpreted() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("sessions.db");
        let original = session(&path, Some("legacy")).await;
        original
            .append(EventKind::User {
                text: "persist legacy session".into(),
            })
            .await
            .unwrap();
        original
            .connection
            .call(|database| {
                database.execute(
                    "DELETE FROM events WHERE session_id = 'legacy' AND kind = 'session_context'",
                    [],
                )?;
                Ok::<_, tokio_rusqlite::rusqlite::Error>(())
            })
            .await
            .unwrap();
        drop(original);

        let error = match Session::open_at(
            path,
            Some("legacy"),
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
        assert!(compacted.snapshot().await.iter().any(|event| {
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
    async fn storage_file_is_sqlite_with_expected_schema() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("sessions.db");
        let opened = session(&path, None).await;
        assert_eq!(opened.database_path(), path);
        assert_eq!(&std::fs::read(&path).unwrap()[..16], b"SQLite format 3\0");
        let tables: i64 = opened.connection.call(|db| db.query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name IN ('sessions','events')",
            [], |row| row.get(0))).await.unwrap();
        assert_eq!(tables, 2);
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
        opened.update_model("changed", "next-model").await.unwrap();
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
        assert_eq!(persisted, (1, 5, "changed".into(), "next-model".into()));
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
        assert_eq!(reopened.snapshot().await.len(), 3);
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
    async fn legacy_empty_sessions_are_removed_without_losing_their_draft() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("sessions.db");
        let opened = session(&path, Some("setup")).await;
        opened
            .connection
            .call(|db| {
                db.execute(
                    "INSERT INTO sessions
                     (id, created_at, updated_at, cwd, provider, model, head_sequence, draft)
                     VALUES ('legacy-empty', '2026-01-01T00:00:00Z',
                             '2026-01-01T00:00:00Z', '/work', 'test', 'model', 1, 'legacy draft')",
                    [],
                )?;
                db.execute(
                    "INSERT INTO events (session_id, sequence, at, kind, payload_json)
                     VALUES ('legacy-empty', 0, '2026-01-01T00:00:00Z', 'notice',
                             '{\"kind\":\"notice\",\"text\":\"startup\"}')",
                    [],
                )?;
                Ok::<_, tokio_rusqlite::rusqlite::Error>(())
            })
            .await
            .unwrap();
        drop(opened);

        let reopened = session(&path, Some("fresh")).await;
        assert_eq!(reopened.draft().await, "legacy draft");
        let counts: (i64, i64) = reopened
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
    async fn model_settings_are_event_sourced_and_restored_on_resume() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("sessions.db");
        let original = Session::open_at_with_thinking(
            path.clone(),
            Some("settings"),
            Path::new("/work"),
            "openai",
            "gpt-old",
            ThinkingLevel::High,
            context("settings"),
        )
        .await
        .unwrap();
        original
            .append(EventKind::User {
                text: "remember the model".into(),
            })
            .await
            .unwrap();
        original
            .update_model_settings("anthropic", "claude-new", ThinkingLevel::Medium)
            .await
            .unwrap();
        assert!(original.snapshot().await.iter().any(|event| matches!(
            &event.kind,
            EventKind::ModelSettingsChanged { provider, model, thinking }
                if provider == "anthropic"
                    && model == "claude-new"
                    && *thinking == ThinkingLevel::Medium
        )));
        drop(original);

        let resumed = Session::open_at_with_thinking(
            path,
            Some("settings"),
            Path::new("/work"),
            "different-default",
            "different-model",
            ThinkingLevel::Off,
            context("ignored"),
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
    fn legacy_session_created_event_defaults_thinking_to_off() {
        let kind: EventKind = serde_json::from_value(serde_json::json!({
            "kind": "session_created",
            "cwd": "/work",
            "provider": "openai",
            "model": "gpt-old"
        }))
        .unwrap();
        assert!(matches!(
            kind,
            EventKind::SessionCreated {
                thinking: ThinkingLevel::Off,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn legacy_row_only_model_change_remains_resumable() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("sessions.db");
        let original = session(&path, Some("legacy-model")).await;
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
                    "UPDATE sessions SET provider = 'legacy-provider', model = 'legacy-current'
                     WHERE id = 'legacy-model'",
                    [],
                )?;
                Ok::<_, tokio_rusqlite::rusqlite::Error>(())
            })
            .await
            .unwrap();
        drop(original);

        let resumed = session(&path, Some("legacy-model")).await;
        assert_eq!(resumed.model_settings().await.provider, "legacy-provider");
        assert_eq!(resumed.model_settings().await.model, "legacy-current");
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
        assert!(!opened.snapshot().await.iter().any(|event| matches!(
            event.kind,
            EventKind::User { .. } | EventKind::ModelMessage { .. }
        )));
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
