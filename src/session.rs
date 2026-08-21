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
    rusqlite::{OptionalExtension, params},
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
    pub preview: String,
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

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EventKind {
    SessionCreated {
        cwd: PathBuf,
        provider: String,
        model: String,
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
    Task {
        id: String,
        protocol: String,
        label: String,
        status: TaskStatus,
    },
    Notice {
        text: String,
    },
    /// Token usage and USD cost reported by one model response.
    Usage {
        input: u64,
        output: u64,
        cache_read: u64,
        cache_write: u64,
        cost: f64,
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
}

#[derive(Clone)]
pub struct Session {
    id: String,
    created: bool,
    directory: PathBuf,
    database_path: PathBuf,
    connection: Connection,
    state: Arc<Mutex<State>>,
    events: broadcast::Sender<SessionEvent>,
}

impl Session {
    pub async fn open(
        requested: Option<&str>,
        cwd: &Path,
        provider: &str,
        model: &str,
        context: SessionContext,
    ) -> Result<Self> {
        Self::open_at(
            session_database_path(cwd),
            requested,
            cwd,
            provider,
            model,
            context,
        )
        .await
    }

    pub(crate) async fn open_at(
        database_path: PathBuf,
        requested: Option<&str>,
        cwd: &Path,
        provider: &str,
        model: &str,
        context: SessionContext,
    ) -> Result<Self> {
        let (directory, connection) = open_database(database_path.clone()).await?;
        let project = cwd
            .canonicalize()
            .unwrap_or_else(|_| cwd.to_path_buf())
            .to_string_lossy()
            .into_owned();

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
            let at_text = at.to_rfc3339();
            let created = EventKind::SessionCreated {
                cwd: cwd.to_path_buf(),
                provider: provider.to_string(),
                model: model.to_string(),
            };
            let frozen = EventKind::SessionContext { context };
            let created_payload =
                serde_json::to_string(&created).context("cannot serialize session creation")?;
            let context_payload =
                serde_json::to_string(&frozen).context("cannot serialize session context")?;
            let id_for_create = id.clone();
            let project_for_create = project.clone();
            let provider_for_create = provider.to_string();
            let model_for_create = model.to_string();
            let at_for_create = at_text.clone();
            connection
                .call(move |db| {
                    let transaction = db.transaction()?;
                    transaction.execute(
                        "INSERT INTO sessions
                         (id, created_at, updated_at, cwd, provider, model, head_sequence, draft)
                         VALUES (?1, ?2, ?2, ?3, ?4, ?5, 1, '')",
                        params![
                            id_for_create,
                            at_for_create,
                            project_for_create,
                            provider_for_create,
                            model_for_create
                        ],
                    )?;
                    transaction.execute(
                        "INSERT INTO events (session_id, sequence, at, kind, payload_json)
                         VALUES (?1, 0, ?2, 'session_created', ?3)",
                        params![id_for_create, at_for_create, created_payload],
                    )?;
                    transaction.execute(
                        "INSERT INTO events (session_id, sequence, at, kind, payload_json)
                         VALUES (?1, 1, ?2, 'session_context', ?3)",
                        params![id_for_create, at_for_create, context_payload],
                    )?;
                    transaction.commit()?;
                    Ok::<_, tokio_rusqlite::rusqlite::Error>(())
                })
                .await
                .context("cannot create session")?;
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

        let (events, _) = broadcast::channel(512);
        let session = Self {
            id,
            created: created_session,
            directory,
            database_path,
            connection,
            state: Arc::new(Mutex::new(State { events: existing })),
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
    pub fn directory(&self) -> &Path {
        &self.directory
    }
    pub fn database_path(&self) -> &Path {
        &self.database_path
    }
    pub async fn draft(&self) -> String {
        let id = self.id.clone();
        self.connection
            .call(move |db| {
                db.query_row("SELECT draft FROM sessions WHERE id = ?1", [id], |row| {
                    row.get(0)
                })
            })
            .await
            .unwrap_or_default()
    }

    pub async fn save_draft(&self, text: &str) -> Result<()> {
        let id = self.id.clone();
        let text = text.to_string();
        self.connection
            .call(move |db| {
                db.execute(
                    "UPDATE sessions SET draft = ?2 WHERE id = ?1",
                    params![id, text],
                )?;
                Ok::<_, tokio_rusqlite::rusqlite::Error>(())
            })
            .await
            .context("cannot save draft")
    }

    pub async fn list_for_project(&self) -> Result<Vec<SessionSummary>> {
        list_project_sessions(
            self.database_path.clone(),
            Path::new(&self.project_cwd().await),
        )
        .await
    }

    async fn project_cwd(&self) -> String {
        let id = self.id.clone();
        self.connection
            .call(move |db| {
                db.query_row("SELECT cwd FROM sessions WHERE id = ?1", [id], |row| {
                    row.get(0)
                })
            })
            .await
            .unwrap_or_default()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<SessionEvent> {
        self.events.subscribe()
    }
    pub async fn snapshot(&self) -> Vec<SessionEvent> {
        self.state.lock().await.events.clone()
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
        history.extend(
            state
                .events
                .iter()
                .skip(latest_compaction.map_or(0, |index| index + 1))
                .filter_map(|event| match &event.kind {
                    EventKind::ModelMessage { message } => Some(message.clone()),
                    _ => None,
                }),
        );
        history
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

    pub async fn update_model(&self, provider: &str, model: &str) -> Result<()> {
        let id = self.id.clone();
        let provider = provider.to_string();
        let model = model.to_string();
        let updated_at = Utc::now().to_rfc3339();
        self.connection
            .call(move |db| {
                db.execute(
                    "UPDATE sessions SET provider = ?2, model = ?3, updated_at = ?4 WHERE id = ?1",
                    params![id, provider, model, updated_at],
                )?;
                Ok::<_, tokio_rusqlite::rusqlite::Error>(())
            })
            .await
            .context("cannot update session model")
    }

    pub async fn append(&self, kind: EventKind) -> Result<SessionEvent> {
        let mut state = self.state.lock().await;
        let id = self.id.clone();
        let at = Utc::now();
        let at_text = at.to_rfc3339();
        let payload = serde_json::to_string(&kind).context("cannot serialize session event")?;
        let kind_name = payload_kind(&kind).to_string();
        let sequence = self
            .connection
            .call(move |db| {
                let transaction = db.transaction()?;
                let head: i64 = transaction.query_row(
                    "SELECT head_sequence FROM sessions WHERE id = ?1",
                    [&id],
                    |row| row.get(0),
                )?;
                let sequence = head + 1;
                transaction.execute(
                    "INSERT INTO events (session_id, sequence, at, kind, payload_json)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![id, sequence, at_text, kind_name, payload],
                )?;
                transaction.execute(
                    "UPDATE sessions SET updated_at = ?2, head_sequence = ?3 WHERE id = ?1",
                    params![id, at_text, sequence],
                )?;
                transaction.commit()?;
                Ok::<_, tokio_rusqlite::rusqlite::Error>(sequence as u64)
            })
            .await
            .context("cannot append session event")?;
        let event = SessionEvent { sequence, at, kind };
        state.events.push(event.clone());
        drop(state);
        let _ = self.events.send(event.clone());
        Ok(event)
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
                "SELECT id, updated_at, provider, model,
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
                    row.get::<_, Option<String>>(4)?,
                ))
            })?;
            let mut sessions = Vec::new();
            for row in rows {
                let (id, updated_at, provider, model, payload) = row?;
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
                   head_sequence INTEGER NOT NULL,
                   draft TEXT NOT NULL DEFAULT ''
                 );
                 CREATE TABLE IF NOT EXISTS events (
                   session_id TEXT NOT NULL, sequence INTEGER NOT NULL, at TEXT NOT NULL,
                   kind TEXT NOT NULL, payload_json TEXT NOT NULL,
                   PRIMARY KEY(session_id, sequence),
                   FOREIGN KEY(session_id) REFERENCES sessions(id) ON DELETE CASCADE
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
        EventKind::User { .. } => "user",
        EventKind::AssistantText { .. } => "assistant_text",
        EventKind::AssistantReasoning { .. } => "assistant_reasoning",
        EventKind::ToolCall { .. } => "tool_call",
        EventKind::ToolResult { .. } => "tool_result",
        EventKind::ModelMessage { .. } => "model_message",
        EventKind::Task { .. } => "task",
        EventKind::Notice { .. } => "notice",
        EventKind::Usage { .. } => "usage",
        EventKind::Error { .. } => "error",
        EventKind::Compaction { .. } => "compaction",
        EventKind::TurnFinished => "turn_finished",
    }
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
    }

    #[tokio::test]
    async fn latest_selects_most_recently_updated_session() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("sessions.db");
        let older = session(&path, Some("older")).await;
        let newer = session(&path, Some("newer")).await;
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
    async fn an_explicit_session_cannot_cross_project_boundaries() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("sessions.db");
        Session::open_at(
            path.clone(),
            Some("other-project"),
            Path::new("/projects/other"),
            "test",
            "model",
            context("other"),
        )
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
        opened.save_draft("keep me").await.unwrap();
        drop(opened);
        let reopened = session(&path, Some("drafty")).await;
        assert_eq!(reopened.draft().await, "keep me");
        assert_eq!(reopened.snapshot().await.len(), 2);
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
        Session::open_at(
            path.clone(),
            Some("other"),
            Path::new("/projects/other"),
            "test",
            "model",
            context("other"),
        )
        .await
        .unwrap();
        let listed = first.list_for_project().await.unwrap();
        let ids = listed
            .iter()
            .map(|item| item.id.as_str())
            .collect::<Vec<_>>();
        assert!(ids.contains(&"alpha"));
        assert!(ids.contains(&"beta"));
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
}
