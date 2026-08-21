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

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SessionEvent {
    pub sequence: u64,
    pub at: DateTime<Utc>,
    #[serde(flatten)]
    pub kind: EventKind,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EventKind {
    SessionCreated {
        cwd: PathBuf,
        provider: String,
        model: String,
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
    Error {
        text: String,
    },
    TurnFinished,
}

struct State {
    events: Vec<SessionEvent>,
}

#[derive(Clone)]
pub struct Session {
    id: String,
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
    ) -> Result<Self> {
        let directory = dirs::data_dir()
            .map(|path| path.join("uri-agent"))
            .unwrap_or_else(|| cwd.join(".uri-agent"));
        Self::open_at(
            directory.join("sessions.db"),
            requested,
            cwd,
            provider,
            model,
        )
        .await
    }

    pub(crate) async fn open_at(
        database_path: PathBuf,
        requested: Option<&str>,
        cwd: &Path,
        provider: &str,
        model: &str,
    ) -> Result<Self> {
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
        let connection = Connection::open(&database_path).await.with_context(|| {
            format!("cannot open session database: {}", database_path.display())
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
                   head_sequence INTEGER NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS events (
                   session_id TEXT NOT NULL, sequence INTEGER NOT NULL, at TEXT NOT NULL,
                   kind TEXT NOT NULL, payload_json TEXT NOT NULL,
                   PRIMARY KEY(session_id, sequence),
                   FOREIGN KEY(session_id) REFERENCES sessions(id) ON DELETE CASCADE
                 );",
                )?;
                Ok::<_, tokio_rusqlite::rusqlite::Error>(())
            })
            .await
            .context("cannot initialize session database")?;

        if let Some(id) = requested.filter(|id| *id != "latest") {
            validate_session_id(id)?;
        }
        let requested = requested.map(str::to_owned);
        let id = connection
            .call(move |db| {
                if requested.as_deref() == Some("latest") {
                    Ok::<_, tokio_rusqlite::rusqlite::Error>(
                        db.query_row(
                            "SELECT id FROM sessions ORDER BY updated_at DESC, id DESC LIMIT 1",
                            [],
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

        let (events, _) = broadcast::channel(512);
        let session = Self {
            id,
            directory,
            database_path,
            connection,
            state: Arc::new(Mutex::new(State { events: existing })),
            events,
        };
        if session.snapshot().await.is_empty() {
            let now = Utc::now().to_rfc3339();
            let id = session.id.clone();
            let cwd_string = cwd.to_string_lossy().into_owned();
            let provider_string = provider.to_string();
            let model_string = model.to_string();
            session
                .connection
                .call(move |db| {
                    db.execute(
                        "INSERT OR IGNORE INTO sessions
                     (id, created_at, updated_at, cwd, provider, model, head_sequence)
                     VALUES (?1, ?2, ?2, ?3, ?4, ?5, -1)",
                        params![id, now, cwd_string, provider_string, model_string],
                    )?;
                    Ok::<_, tokio_rusqlite::rusqlite::Error>(())
                })
                .await
                .context("cannot create session")?;
            session
                .append(EventKind::SessionCreated {
                    cwd: cwd.to_path_buf(),
                    provider: provider.to_string(),
                    model: model.to_string(),
                })
                .await?;
        }
        Ok(session)
    }

    pub fn id(&self) -> &str {
        &self.id
    }
    pub fn directory(&self) -> &Path {
        &self.directory
    }
    pub fn database_path(&self) -> &Path {
        &self.database_path
    }
    pub fn subscribe(&self) -> broadcast::Receiver<SessionEvent> {
        self.events.subscribe()
    }
    pub async fn snapshot(&self) -> Vec<SessionEvent> {
        self.state.lock().await.events.clone()
    }

    pub async fn model_history(&self) -> Vec<Message> {
        self.state
            .lock()
            .await
            .events
            .iter()
            .filter_map(|event| match &event.kind {
                EventKind::ModelMessage { message } => Some(message.clone()),
                _ => None,
            })
            .collect()
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

fn payload_kind(kind: &EventKind) -> &'static str {
    match kind {
        EventKind::SessionCreated { .. } => "session_created",
        EventKind::User { .. } => "user",
        EventKind::AssistantText { .. } => "assistant_text",
        EventKind::AssistantReasoning { .. } => "assistant_reasoning",
        EventKind::ToolCall { .. } => "tool_call",
        EventKind::ToolResult { .. } => "tool_result",
        EventKind::ModelMessage { .. } => "model_message",
        EventKind::Task { .. } => "task",
        EventKind::Notice { .. } => "notice",
        EventKind::Error { .. } => "error",
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

    async fn session(path: &Path, requested: Option<&str>) -> Session {
        Session::open_at(
            path.to_path_buf(),
            requested,
            Path::new("/work"),
            "test",
            "model",
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
            vec![0, 1, 2]
        );
        assert!(matches!(events[2].kind, EventKind::TurnFinished));
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
}
