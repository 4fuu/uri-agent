use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::sync::Mutex as SyncMutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::{RwLock, broadcast};
use tokio::task::JoinHandle;
use tokio::time;
use tokio_util::sync::CancellationToken;

pub const MAX_BACKGROUND_TASKS: usize = 16;
const LATEST_OUTPUT_MAX_BYTES: usize = 64 * 1024;

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
    pub background: bool,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub content: Vec<u8>,
    pub latest_output: Vec<u8>,
    pub cancellation: CancellationToken,
    terminal_notification: TerminalNotification,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskReport {
    pub id: String,
    pub protocol: String,
    pub label: String,
    pub status: TaskStatus,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub content: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct TaskNotice {
    pub id: String,
    pub protocol: String,
    pub label: String,
    pub status: TaskStatus,
    pub background: bool,
}

#[derive(Clone, Debug)]
pub enum PromoteBackground {
    Promoted,
    Terminal(TaskRecord),
    AtCapacity,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TerminalNotification {
    Disabled,
    Pending,
    Presented,
    Delivered,
}

#[derive(Clone)]
pub struct TaskManager {
    inner: Arc<RwLock<HashMap<String, TaskRecord>>>,
    workers: Arc<SyncMutex<HashMap<String, JoinHandle<()>>>>,
    notices: broadcast::Sender<TaskNotice>,
    next_id: Arc<AtomicU64>,
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
            workers: Arc::new(SyncMutex::new(HashMap::new())),
            notices,
            next_id: Arc::new(AtomicU64::new(1)),
        }
    }

    pub fn from_reports(reports: impl IntoIterator<Item = TaskReport>) -> Self {
        let mut records = HashMap::new();
        let mut next_id = 1;
        for report in reports {
            if !report.status.terminal() {
                continue;
            }
            if let Ok(sequence) = u64::from_str_radix(&report.id, 16) {
                next_id = next_id.max(sequence.saturating_add(1));
            }
            let mut latest_output = Vec::new();
            append_bounded(&mut latest_output, &report.content);
            records.insert(
                report.id.clone(),
                TaskRecord {
                    id: report.id,
                    protocol: report.protocol,
                    label: report.label,
                    status: report.status,
                    background: true,
                    started_at: report.started_at,
                    finished_at: Some(report.finished_at),
                    content: report.content,
                    latest_output,
                    cancellation: CancellationToken::new(),
                    terminal_notification: TerminalNotification::Delivered,
                },
            );
        }
        let (notices, _) = broadcast::channel(128);
        Self {
            inner: Arc::new(RwLock::new(records)),
            workers: Arc::new(SyncMutex::new(HashMap::new())),
            notices,
            next_id: Arc::new(AtomicU64::new(next_id)),
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<TaskNotice> {
        self.notices.subscribe()
    }

    pub async fn allocate(&self, protocol: &str, label: impl Into<String>) -> TaskRecord {
        self.allocate_record(protocol, label.into(), false).await
    }

    pub async fn allocate_background(
        &self,
        protocol: &str,
        label: impl Into<String>,
    ) -> anyhow::Result<TaskRecord> {
        let label = label.into();
        let mut records = self.inner.write().await;
        if active_background_count(&records) >= MAX_BACKGROUND_TASKS {
            anyhow::bail!("background task limit reached ({MAX_BACKGROUND_TASKS})");
        }
        Ok(self.insert_record(&mut records, protocol, label, true))
    }

    async fn allocate_record(&self, protocol: &str, label: String, background: bool) -> TaskRecord {
        let mut records = self.inner.write().await;
        self.insert_record(&mut records, protocol, label, background)
    }

    fn insert_record(
        &self,
        records: &mut HashMap<String, TaskRecord>,
        protocol: &str,
        label: String,
        background: bool,
    ) -> TaskRecord {
        let record = TaskRecord {
            id: format_task_id(self.next_id.fetch_add(1, Ordering::Relaxed)),
            protocol: protocol.to_string(),
            label,
            status: TaskStatus::Pending,
            background,
            started_at: Utc::now(),
            finished_at: None,
            content: Vec::new(),
            latest_output: Vec::new(),
            cancellation: CancellationToken::new(),
            terminal_notification: if background {
                TerminalNotification::Pending
            } else {
                TerminalNotification::Disabled
            },
        };
        records.insert(record.id.clone(), record.clone());
        record
    }

    pub async fn promote_background(&self, id: &str) -> PromoteBackground {
        let mut records = self.inner.write().await;
        let Some(record) = records.get(id) else {
            return PromoteBackground::AtCapacity;
        };
        if record.status.terminal() {
            return PromoteBackground::Terminal(record.clone());
        }
        if active_background_count(&records) >= MAX_BACKGROUND_TASKS {
            return PromoteBackground::AtCapacity;
        }
        let record = records
            .get_mut(id)
            .expect("the task remains present while the task map is locked");
        record.background = true;
        record.terminal_notification = TerminalNotification::Pending;
        let notice = TaskNotice {
            id: record.id.clone(),
            protocol: record.protocol.clone(),
            label: record.label.clone(),
            status: record.status,
            background: true,
        };
        drop(records);
        let _ = self.notices.send(notice);
        PromoteBackground::Promoted
    }

    pub async fn spawn<F>(&self, record: TaskRecord, future: F)
    where
        F: Future<Output = anyhow::Result<Vec<u8>>> + Send + 'static,
    {
        let manager = self.clone();
        let id = record.id.clone();
        let worker_id = id.clone();
        let handle = tokio::spawn(async move {
            manager
                .set_status(&record.id, TaskStatus::Running, None)
                .await;
            let result = tokio::select! {
                _ = record.cancellation.cancelled() => None,
                result = future => Some(result),
            };
            match result {
                None => {
                    manager
                        .set_status(&record.id, TaskStatus::Cancelled, None)
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
            manager
                .workers
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&worker_id);
        });
        self.workers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(id, handle);
    }

    async fn set_status(&self, id: &str, status: TaskStatus, content: Option<Vec<u8>>) {
        let notice = {
            let mut records = self.inner.write().await;
            let Some(record) = records.get_mut(id) else {
                return;
            };
            if record.status.terminal() {
                return;
            }
            record.status = status;
            if let Some(content) = content {
                record.latest_output.clear();
                append_bounded(&mut record.latest_output, &content);
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
                background: record.background,
            }
        };
        let _ = self.notices.send(notice);
    }

    pub async fn append_latest_output(&self, id: &str, content: &[u8]) {
        if content.is_empty() {
            return;
        }
        let mut records = self.inner.write().await;
        let Some(record) = records.get_mut(id) else {
            return;
        };
        record.content.extend_from_slice(content);
        append_bounded(&mut record.latest_output, content);
    }

    pub async fn get(&self, id: &str) -> Option<TaskRecord> {
        self.inner.read().await.get(id).cloned()
    }

    /// Waits for a task to finish for at most `duration` without cancelling it on timeout.
    /// Protocols decide whether and how to expose this operation in their own targets.
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
            .filter(|record| record.background)
            .cloned()
            .collect::<Vec<_>>();
        records.sort_by_key(|record| std::cmp::Reverse(record.started_at));
        records
    }

    pub async fn remove(&self, id: &str) {
        self.inner.write().await.remove(id);
    }

    pub async fn wait_until_terminal(&self, id: &str) -> Option<TaskRecord> {
        let mut notices = self.subscribe();
        loop {
            let current = self.get(id).await?;
            if current.status.terminal() {
                return Some(current);
            }
            match notices.recv().await {
                Ok(notice) if notice.id == id && notice.status.terminal() => {}
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return self.get(id).await,
            }
        }
    }

    pub async fn pending_terminal_notifications(&self) -> Vec<TaskRecord> {
        let mut records = self
            .inner
            .read()
            .await
            .values()
            .filter(|record| {
                record.status.terminal()
                    && record.terminal_notification == TerminalNotification::Pending
            })
            .cloned()
            .collect::<Vec<_>>();
        records.sort_by_key(|record| record.started_at);
        records
    }

    pub async fn mark_terminal_presented(&self, id: &str) {
        let mut records = self.inner.write().await;
        let Some(record) = records.get_mut(id) else {
            return;
        };
        if record.status.terminal() && record.terminal_notification == TerminalNotification::Pending
        {
            record.terminal_notification = TerminalNotification::Presented;
        }
    }

    pub async fn mark_terminal_notifications_delivered(&self, ids: &[String]) {
        let mut records = self.inner.write().await;
        for id in ids {
            let Some(record) = records.get_mut(id) else {
                continue;
            };
            if record.status.terminal()
                && record.terminal_notification == TerminalNotification::Pending
            {
                record.terminal_notification = TerminalNotification::Delivered;
            }
        }
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

    pub async fn shutdown(&self) {
        let records = self.inner.read().await;
        let ids = records
            .values()
            .filter(|record| !record.status.terminal())
            .map(|record| {
                record.cancellation.cancel();
                record.id.clone()
            })
            .collect::<Vec<_>>();
        drop(records);
        let workers = self
            .workers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .drain()
            .map(|(_, handle)| handle)
            .collect::<Vec<_>>();
        for worker in workers {
            let _ = worker.await;
        }
        for id in ids {
            if self
                .get(&id)
                .await
                .is_some_and(|record| !record.status.terminal())
            {
                self.set_status(&id, TaskStatus::Cancelled, Some(Vec::new()))
                    .await;
            }
        }
    }
}

fn active_background_count(records: &HashMap<String, TaskRecord>) -> usize {
    records
        .values()
        .filter(|record| record.background && !record.status.terminal())
        .count()
}

fn append_bounded(output: &mut Vec<u8>, content: &[u8]) {
    if content.len() >= LATEST_OUTPUT_MAX_BYTES {
        output.clear();
        output.extend_from_slice(&content[content.len() - LATEST_OUTPUT_MAX_BYTES..]);
        return;
    }
    let overflow = output
        .len()
        .saturating_add(content.len())
        .saturating_sub(LATEST_OUTPUT_MAX_BYTES);
    if overflow > 0 {
        output.drain(..overflow);
    }
    output.extend_from_slice(content);
}

fn format_task_id(sequence: u64) -> String {
    format!("{sequence:03x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_ids_are_lowercase_hex_with_a_three_digit_minimum() {
        assert_eq!(format_task_id(1), "001");
        assert_eq!(format_task_id(0xabc), "abc");
        assert_eq!(format_task_id(0xfff), "fff");
        assert_eq!(format_task_id(0x1000), "1000");
    }

    #[tokio::test]
    async fn task_manager_allocates_monotonic_ids() {
        let tasks = TaskManager::new();
        assert_eq!(tasks.allocate("test", "first").await.id, "001");
        assert_eq!(tasks.allocate("test", "second").await.id, "002");
    }

    #[tokio::test]
    async fn restored_reports_remain_readable_and_advance_task_ids() {
        let started_at = Utc::now();
        let finished_at = started_at + chrono::Duration::seconds(1);
        let tasks = TaskManager::from_reports([TaskReport {
            id: "00f".to_string(),
            protocol: "bash".to_string(),
            label: "restored".to_string(),
            status: TaskStatus::Completed,
            started_at,
            finished_at,
            content: b"complete output".to_vec(),
        }]);

        let restored = tasks.get("00f").await.unwrap();
        assert_eq!(restored.status, TaskStatus::Completed);
        assert_eq!(restored.content, b"complete output");
        assert_eq!(restored.finished_at, Some(finished_at));
        assert!(tasks.pending_terminal_notifications().await.is_empty());
        assert_eq!(
            tasks.allocate_background("bash", "next").await.unwrap().id,
            "010"
        );
    }

    #[tokio::test]
    async fn bounded_wait_is_uri_independent_and_does_not_cancel_on_timeout() {
        let tasks = TaskManager::new();
        let record = tasks.allocate("test", "slow task").await;
        let id = record.id.clone();
        tasks
            .spawn(record, async {
                time::sleep(Duration::from_millis(100)).await;
                Ok(b"done".to_vec())
            })
            .await;

        let running = tasks.wait(&id, Duration::from_millis(5)).await.unwrap();
        assert_eq!(running.status, TaskStatus::Running);

        let completed = tasks.wait(&id, Duration::from_secs(1)).await.unwrap();
        assert_eq!(completed.status, TaskStatus::Completed);
        assert_eq!(completed.content, b"done");
    }

    #[tokio::test]
    async fn terminal_notifications_are_opt_in_and_settle_once_presented_or_delivered() {
        let tasks = TaskManager::new();
        let silent = tasks.allocate("test", "silent").await;
        let silent_id = silent.id.clone();
        tasks.spawn(silent, async { Ok(b"silent".to_vec()) }).await;
        tasks
            .wait(&silent_id, Duration::from_secs(1))
            .await
            .unwrap();
        assert!(tasks.pending_terminal_notifications().await.is_empty());

        let presented = tasks
            .allocate_background("bash", "presented")
            .await
            .unwrap();
        let presented_id = presented.id.clone();
        tasks
            .spawn(presented, async { Ok(b"presented".to_vec()) })
            .await;
        tasks
            .wait(&presented_id, Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(
            tasks
                .pending_terminal_notifications()
                .await
                .iter()
                .map(|record| record.id.as_str())
                .collect::<Vec<_>>(),
            [presented_id.as_str()]
        );
        tasks.mark_terminal_presented(&presented_id).await;
        assert!(tasks.pending_terminal_notifications().await.is_empty());

        let delivered = tasks
            .allocate_background("pwsh", "delivered")
            .await
            .unwrap();
        let delivered_id = delivered.id.clone();
        tasks
            .spawn(delivered, async { Ok(b"delivered".to_vec()) })
            .await;
        tasks
            .wait(&delivered_id, Duration::from_secs(1))
            .await
            .unwrap();
        tasks
            .mark_terminal_notifications_delivered(std::slice::from_ref(&delivered_id))
            .await;
        assert!(tasks.pending_terminal_notifications().await.is_empty());
    }

    #[tokio::test]
    async fn background_capacity_is_bounded_and_terminal_tasks_release_it() {
        let tasks = TaskManager::new();
        let mut records = Vec::new();
        for index in 0..MAX_BACKGROUND_TASKS {
            records.push(
                tasks
                    .allocate_background("test", format!("task {index}"))
                    .await
                    .unwrap(),
            );
        }
        assert!(
            tasks
                .allocate_background("test", "over capacity")
                .await
                .unwrap_err()
                .to_string()
                .contains("background task limit reached")
        );

        let first = records.remove(0);
        tasks.spawn(first, async { Ok(Vec::new()) }).await;
        tasks.wait("001", Duration::from_secs(1)).await.unwrap();
        assert!(
            tasks
                .allocate_background("test", "replacement")
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn promotion_never_hides_a_task_that_finished_at_the_boundary() {
        let tasks = TaskManager::new();
        let record = tasks.allocate("bash", "quick").await;
        let id = record.id.clone();
        tasks.spawn(record, async { Ok(b"done".to_vec()) }).await;
        tasks.wait(&id, Duration::from_secs(1)).await.unwrap();

        let PromoteBackground::Terminal(record) = tasks.promote_background(&id).await else {
            panic!("a terminal foreground task must remain foreground");
        };
        assert_eq!(record.status, TaskStatus::Completed);
        assert!(tasks.list().await.is_empty());
        assert!(tasks.pending_terminal_notifications().await.is_empty());
    }

    #[tokio::test]
    async fn cancellation_preserves_output_observed_before_the_process_stops() {
        let tasks = TaskManager::new();
        let record = tasks
            .allocate_background("bash", "cancelled")
            .await
            .unwrap();
        let id = record.id.clone();
        tasks
            .spawn(record, async {
                time::sleep(Duration::from_secs(60)).await;
                Ok(Vec::new())
            })
            .await;
        tasks.append_latest_output(&id, b"partial output").await;

        assert!(tasks.cancel(&id).await);
        let record = tasks.wait_until_terminal(&id).await.unwrap();

        assert_eq!(record.status, TaskStatus::Cancelled);
        assert_eq!(record.content, b"partial output");
    }

    #[tokio::test]
    async fn shutdown_cancels_and_joins_running_tasks() {
        let tasks = TaskManager::new();
        let record = tasks.allocate_background("bash", "long").await.unwrap();
        let id = record.id.clone();
        let pending = tasks
            .allocate_background("bash", "not started")
            .await
            .unwrap();
        let pending_id = pending.id.clone();
        tasks
            .spawn(record, async {
                time::sleep(Duration::from_secs(60)).await;
                Ok(Vec::new())
            })
            .await;

        tasks.shutdown().await;

        assert_eq!(tasks.get(&id).await.unwrap().status, TaskStatus::Cancelled);
        assert_eq!(
            tasks.get(&pending_id).await.unwrap().status,
            TaskStatus::Cancelled
        );
    }
}
