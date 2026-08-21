use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{RwLock, broadcast};
use tokio::time;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskStatus {
    Pending,
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl TaskStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

#[derive(Clone, Debug)]
pub struct TaskRecord {
    pub id: String,
    pub protocol: String,
    pub label: String,
    pub status: TaskStatus,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub content: Vec<u8>,
    pub cancellation: CancellationToken,
}

#[derive(Clone, Debug)]
pub struct TaskNotice {
    pub id: String,
    pub protocol: String,
    pub label: String,
    pub status: TaskStatus,
}

#[derive(Clone)]
pub struct TaskManager {
    inner: Arc<RwLock<HashMap<String, TaskRecord>>>,
    notices: broadcast::Sender<TaskNotice>,
}

impl Default for TaskManager {
    fn default() -> Self {
        Self::new()
    }
}

impl TaskManager {
    pub fn new() -> Self {
        let (notices, _) = broadcast::channel(128);
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
            notices,
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<TaskNotice> {
        self.notices.subscribe()
    }

    pub async fn allocate(&self, protocol: &str, label: impl Into<String>) -> TaskRecord {
        let record = TaskRecord {
            id: Uuid::now_v7().simple().to_string(),
            protocol: protocol.to_string(),
            label: label.into(),
            status: TaskStatus::Pending,
            started_at: Utc::now(),
            finished_at: None,
            content: Vec::new(),
            cancellation: CancellationToken::new(),
        };
        self.inner
            .write()
            .await
            .insert(record.id.clone(), record.clone());
        record
    }

    pub async fn spawn<F>(&self, record: TaskRecord, future: F)
    where
        F: Future<Output = anyhow::Result<Vec<u8>>> + Send + 'static,
    {
        self.set_status(&record.id, TaskStatus::Running, None).await;
        let manager = self.clone();
        tokio::spawn(async move {
            let result = tokio::select! {
                _ = record.cancellation.cancelled() => None,
                result = future => Some(result),
            };
            match result {
                None => {
                    manager
                        .set_status(&record.id, TaskStatus::Cancelled, Some(Vec::new()))
                        .await
                }
                Some(Ok(content)) => {
                    manager
                        .set_status(&record.id, TaskStatus::Completed, Some(content))
                        .await
                }
                Some(Err(error)) => {
                    manager
                        .set_status(
                            &record.id,
                            TaskStatus::Failed,
                            Some(error.to_string().into_bytes()),
                        )
                        .await
                }
            }
        });
    }

    async fn set_status(&self, id: &str, status: TaskStatus, content: Option<Vec<u8>>) {
        let notice = {
            let mut records = self.inner.write().await;
            let Some(record) = records.get_mut(id) else {
                return;
            };
            record.status = status;
            if let Some(content) = content {
                record.content = content;
            }
            if status.terminal() {
                record.finished_at = Some(Utc::now());
            }
            TaskNotice {
                id: record.id.clone(),
                protocol: record.protocol.clone(),
                label: record.label.clone(),
                status,
            }
        };
        let _ = self.notices.send(notice);
    }

    pub async fn get(&self, id: &str) -> Option<TaskRecord> {
        self.inner.read().await.get(id).cloned()
    }

    pub async fn wait(&self, id: &str, duration: Duration) -> Option<TaskRecord> {
        let mut notices = self.subscribe();
        let current = self.get(id).await?;
        if current.status.terminal() || duration.is_zero() {
            return Some(current);
        }
        let _ = time::timeout(duration, async {
            loop {
                match notices.recv().await {
                    Ok(notice) if notice.id == id && notice.status.terminal() => break,
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        })
        .await;
        self.get(id).await
    }

    pub async fn list(&self) -> Vec<TaskRecord> {
        let mut records = self
            .inner
            .read()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        records.sort_by_key(|record| std::cmp::Reverse(record.started_at));
        records
    }

    pub async fn cancel(&self, id: &str) -> bool {
        let records = self.inner.read().await;
        let Some(record) = records.get(id) else {
            return false;
        };
        if record.status.terminal() {
            return false;
        }
        record.cancellation.cancel();
        true
    }
}
