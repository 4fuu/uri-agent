use crate::task::TaskStatus;
use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use rig::message::Message;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::fs::{self, File, OpenOptions};
use tokio::io::AsyncWriteExt;
use tokio::sync::{Mutex, broadcast};
use uuid::Uuid;

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
    file: File,
}

#[derive(Clone)]
pub struct Session {
    id: String,
    directory: PathBuf,
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
        let sessions = dirs::data_dir()
            .unwrap_or_else(|| cwd.join(".uri-agent"))
            .join("uri-agent/sessions");
        fs::create_dir_all(&sessions)
            .await
            .with_context(|| format!("cannot create session directory: {}", sessions.display()))?;

        let id = match requested {
            Some("latest") => latest_session(&sessions)
                .await?
                .unwrap_or_else(new_session_id),
            Some(id) => {
                validate_session_id(id)?;
                id.to_string()
            }
            None => new_session_id(),
        };
        let directory = sessions.join(&id);
        fs::create_dir_all(&directory).await?;
        let path = directory.join("events.jsonl");
        let existing = match fs::read_to_string(&path).await {
            Ok(content) => {
                let events = parse_events(&path, &content)?;
                if let Some(valid_length) = incomplete_tail(&content) {
                    fs::write(&path, &content.as_bytes()[..valid_length]).await?;
                } else if !content.is_empty() && !content.ends_with('\n') {
                    fs::write(&path, format!("{content}\n")).await?;
                }
                events
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error).context("cannot read session events"),
        };
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await?;
        let (events, _) = broadcast::channel(512);
        let session = Self {
            id,
            directory,
            state: Arc::new(Mutex::new(State {
                events: existing,
                file,
            })),
            events,
        };
        if session.snapshot().await.is_empty() {
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
        let event = {
            let mut state = self.state.lock().await;
            let event = SessionEvent {
                sequence: state
                    .events
                    .last()
                    .map_or(0, |event| event.sequence.saturating_add(1)),
                at: Utc::now(),
                kind,
            };
            let mut encoded = serde_json::to_vec(&event)?;
            encoded.push(b'\n');
            state.file.write_all(&encoded).await?;
            state.events.push(event.clone());
            event
        };
        let _ = self.events.send(event.clone());
        Ok(event)
    }
}

fn parse_events(path: &Path, content: &str) -> Result<Vec<SessionEvent>> {
    let mut events = Vec::new();
    let last_line = content.lines().count().saturating_sub(1);
    let trailing_partial = !content.ends_with('\n');
    for (index, line) in content.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let event = match serde_json::from_str::<SessionEvent>(line) {
            Ok(event) => event,
            Err(_) if trailing_partial && index == last_line => break,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("invalid event at {}:{}", path.display(), index + 1));
            }
        };
        if event.sequence != events.len() as u64 {
            bail!(
                "non-contiguous sequence at {}:{}",
                path.display(),
                index + 1
            );
        }
        events.push(event);
    }
    Ok(events)
}

fn incomplete_tail(content: &str) -> Option<usize> {
    if content.ends_with('\n') {
        return None;
    }
    let start = content.rfind('\n').map_or(0, |index| index + 1);
    serde_json::from_str::<SessionEvent>(&content[start..])
        .is_err()
        .then_some(start)
}

async fn latest_session(directory: &Path) -> Result<Option<String>> {
    let mut entries = fs::read_dir(directory).await?;
    let mut latest = None;
    while let Some(entry) = entries.next_entry().await? {
        if !entry.file_type().await?.is_dir() {
            continue;
        }
        let modified = match fs::metadata(entry.path().join("events.jsonl")).await {
            Ok(metadata) => metadata.modified()?,
            Err(_) => entry.metadata().await?.modified()?,
        };
        if latest
            .as_ref()
            .is_none_or(|(_, latest_modified)| modified > *latest_modified)
        {
            latest = Some((entry.file_name().to_string_lossy().into_owned(), modified));
        }
    }
    Ok(latest.map(|(id, _)| id))
}

fn validate_session_id(id: &str) -> Result<()> {
    if id.is_empty()
        || !id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
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

    #[test]
    fn persisted_events_replay_in_order() {
        let first = SessionEvent {
            sequence: 0,
            at: Utc::now(),
            kind: EventKind::User {
                text: "hello".to_string(),
            },
        };
        let second = SessionEvent {
            sequence: 1,
            at: Utc::now(),
            kind: EventKind::TurnFinished,
        };
        let content = format!(
            "{}\n{}\n",
            serde_json::to_string(&first).unwrap(),
            serde_json::to_string(&second).unwrap()
        );
        let events = parse_events(Path::new("events.jsonl"), &content).unwrap();
        assert_eq!(events.len(), 2);
        assert!(matches!(events[1].kind, EventKind::TurnFinished));
    }

    #[test]
    fn replay_ignores_only_an_interrupted_trailing_write() {
        let event = SessionEvent {
            sequence: 0,
            at: Utc::now(),
            kind: EventKind::TurnFinished,
        };
        let content = format!(
            "{}\n{{\"sequence\":",
            serde_json::to_string(&event).unwrap()
        );
        let events = parse_events(Path::new("events.jsonl"), &content).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(
            incomplete_tail(&content),
            content.rfind('\n').map(|index| index + 1)
        );

        let invalid_middle = format!(
            "{}\nnot-json\n{}\n",
            serde_json::to_string(&event).unwrap(),
            serde_json::to_string(&event).unwrap()
        );
        assert!(parse_events(Path::new("events.jsonl"), &invalid_middle).is_err());
    }
}
