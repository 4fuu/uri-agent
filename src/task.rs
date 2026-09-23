use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::sync::Mutex as SyncMutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::{RwLock, broadcast, mpsc};
use tokio::task::JoinHandle;
use tokio::time;
use tokio_util::sync::CancellationToken;

pub const MAX_BACKGROUND_TASKS: usize = 16;
const LATEST_OUTPUT_MAX_BYTES: usize = 64 * 1024;
const TASK_INPUT_CHANNEL_CAPACITY: usize = 8;
pub const TASK_INPUT_MAX_BYTES: usize = 1024 * 1024;

/// Runtime input delivered to a running task's process stdin.
#[derive(Debug, PartialEq)]
pub enum TaskInput {
    Bytes(Vec<u8>),
    Close,
}

/// The manager-side end of a task's input channel. `Closed` remembers an
/// explicit end-of-file so later sends report that instead of claiming the
/// task never accepted input.
#[derive(Clone, Debug)]
enum TaskInputControl {
    Open(mpsc::Sender<TaskInput>),
    Closed,
}

/// Runtime controls an owning protocol may attach to a task: an input
/// channel for processes that read stdin, and an interrupt signal distinct
/// from cancellation.
#[derive(Clone, Debug, Default)]
pub struct TaskControls {
    input: Option<TaskInputControl>,
    interrupt: Option<CancellationToken>,
}

impl TaskControls {
    /// Builds the controls for an interactive task whose process stdin stays
    /// open. The returned receiver forwards accepted input to that stdin, and
    /// cancelling the returned token must interrupt the process.
    pub fn interactive() -> (Self, mpsc::Receiver<TaskInput>, CancellationToken) {
        let (sender, receiver) = mpsc::channel(TASK_INPUT_CHANNEL_CAPACITY);
        let interrupt = CancellationToken::new();
        (
            Self {
                input: Some(TaskInputControl::Open(sender)),
                interrupt: Some(interrupt.clone()),
            },
            receiver,
            interrupt,
        )
    }
}

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
    controls: TaskControls,
    terminal_notification: TerminalNotification,
}

impl TaskRecord {
    /// Whether the owning protocol registered an interrupt signal, making the
    /// task a candidate for `interrupt` operations.
    pub fn interruptible(&self) -> bool {
        self.controls.interrupt.is_some()
    }

    pub fn terminal_result(self, operation: &str) -> anyhow::Result<Vec<u8>> {
        match self.status {
            TaskStatus::Completed => Ok(self.content),
            TaskStatus::Failed => Err(anyhow::anyhow!(
                String::from_utf8_lossy(&self.content).into_owned()
            )),
            TaskStatus::Cancelled => anyhow::bail!("{operation} was cancelled"),
            TaskStatus::Pending | TaskStatus::Running => {
                anyhow::bail!("{operation} did not reach a terminal state")
            }
        }
    }
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
    Missing,
}

#[derive(Clone, Debug)]
pub enum AutoTask {
    Background(String),
    Terminal(TaskRecord),
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

struct ForegroundTaskGuard {
    tasks: TaskManager,
    id: String,
    cancellation: CancellationToken,
    armed: bool,
}

impl ForegroundTaskGuard {
    fn new(tasks: TaskManager, record: &TaskRecord) -> Self {
        Self {
            tasks,
            id: record.id.clone(),
            cancellation: record.cancellation.clone(),
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ForegroundTaskGuard {
    fn drop(&mut self) {
        if self.armed {
            self.cancellation.cancel();
            let tasks = self.tasks.clone();
            let id = self.id.clone();
            tokio::spawn(async move {
                if tasks
                    .wait_until_terminal(&id)
                    .await
                    .is_some_and(|record| !record.background)
                {
                    tasks.remove(&id).await;
                }
            });
        }
    }
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
                    controls: TaskControls::default(),
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
            controls: TaskControls::default(),
            terminal_notification: if background {
                TerminalNotification::Pending
            } else {
                TerminalNotification::Disabled
            },
        };
        records.insert(record.id.clone(), record.clone());
        record
    }

    /// Promotes a running foreground task to the background.
    ///
    /// The capacity limit bounds how many background tasks callers may start
    /// (see [`TaskManager::allocate_background`]); it deliberately does not
    /// apply here. The promoted process is already running, and refusing the
    /// promotion would leave the caller waiting for it unbounded.
    pub async fn promote_background(&self, id: &str) -> PromoteBackground {
        let mut records = self.inner.write().await;
        let Some(record) = records.get(id) else {
            return PromoteBackground::Missing;
        };
        if record.status.terminal() {
            return PromoteBackground::Terminal(record.clone());
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
        let cancellation = record.cancellation.clone();
        self.spawn_worker(record, async move {
            tokio::select! {
                _ = cancellation.cancelled() => None,
                result = future => Some(result),
            }
        })
        .await;
    }

    /// Runs work that observes the supplied token and settles only after its
    /// cancellation cleanup is complete.
    pub async fn spawn_with_cancellation<W, F>(&self, record: TaskRecord, work: W)
    where
        W: FnOnce(CancellationToken) -> F,
        F: Future<Output = anyhow::Result<Vec<u8>>> + Send + 'static,
    {
        let cancellation = record.cancellation.clone();
        let future = work(cancellation.clone());
        self.spawn_worker(record, async move { Some(future.await) })
            .await;
    }

    pub async fn run_with_auto_background<W, F>(
        &self,
        record: TaskRecord,
        foreground_duration: Duration,
        work: W,
    ) -> anyhow::Result<AutoTask>
    where
        W: FnOnce(CancellationToken) -> F,
        F: Future<Output = anyhow::Result<Vec<u8>>> + Send + 'static,
    {
        debug_assert!(!record.background);
        let id = record.id.clone();
        let mut foreground = ForegroundTaskGuard::new(self.clone(), &record);
        self.spawn_with_cancellation(record, work).await;
        let current = self
            .wait(&id, foreground_duration)
            .await
            .ok_or_else(|| anyhow::anyhow!("task disappeared: {id}"))?;
        let outcome = if current.status.terminal() {
            self.remove(&id).await;
            AutoTask::Terminal(current)
        } else {
            match self.promote_background(&id).await {
                PromoteBackground::Promoted => AutoTask::Background(id),
                PromoteBackground::Terminal(record) => {
                    self.remove(&id).await;
                    AutoTask::Terminal(record)
                }
                PromoteBackground::Missing => {
                    anyhow::bail!("task disappeared: {id}")
                }
            }
        };
        foreground.disarm();
        Ok(outcome)
    }

    async fn spawn_worker<F>(&self, record: TaskRecord, future: F)
    where
        F: Future<Output = Option<anyhow::Result<Vec<u8>>>> + Send + 'static,
    {
        let manager = self.clone();
        let id = record.id.clone();
        let worker_id = id.clone();
        let cancellation = record.cancellation.clone();
        let handle = tokio::spawn(async move {
            manager
                .set_status(&record.id, TaskStatus::Running, None)
                .await;
            let result = future.await;
            let (status, content) = match result {
                None => (TaskStatus::Cancelled, None),
                Some(Ok(content)) => (TaskStatus::Completed, Some(content)),
                Some(Err(error)) => (TaskStatus::Failed, Some(format!("{error:#}").into_bytes())),
            };
            manager
                .set_status_after_work(&record.id, status, content, &cancellation)
                .await;
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
        self.set_status_inner(id, status, content, None).await;
    }

    async fn set_status_after_work(
        &self,
        id: &str,
        status: TaskStatus,
        content: Option<Vec<u8>>,
        cancellation: &CancellationToken,
    ) {
        self.set_status_inner(id, status, content, Some(cancellation))
            .await;
    }

    async fn set_status_inner(
        &self,
        id: &str,
        mut status: TaskStatus,
        mut content: Option<Vec<u8>>,
        cancellation: Option<&CancellationToken>,
    ) {
        let notice = {
            let mut records = self.inner.write().await;
            let Some(record) = records.get_mut(id) else {
                return;
            };
            if record.status.terminal() {
                return;
            }
            if cancellation.is_some_and(CancellationToken::is_cancelled) {
                status = TaskStatus::Cancelled;
                content = None;
            }
            record.status = status;
            if let Some(content) = content {
                record.latest_output.clear();
                append_bounded(&mut record.latest_output, &content);
                record.content = content;
            }
            if status.terminal() {
                record.finished_at = Some(Utc::now());
                // Terminal tasks never accept more input or interrupts, and
                // dropping the channel ends a writer still waiting for either.
                record.controls = TaskControls::default();
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

    /// Attaches runtime controls to an allocated task. Interactive shell
    /// executions call this before their worker starts so input and interrupt
    /// calls cannot race process startup.
    pub async fn set_controls(&self, id: &str, controls: TaskControls) {
        let mut records = self.inner.write().await;
        if let Some(record) = records.get_mut(id) {
            record.controls = controls;
        }
    }

    /// Delivers runtime input to a running task's process stdin. `Close`
    /// shuts the channel after the input reaches the task.
    pub async fn send_input(&self, id: &str, input: TaskInput) -> anyhow::Result<()> {
        if let TaskInput::Bytes(bytes) = &input
            && bytes.len() > TASK_INPUT_MAX_BYTES
        {
            anyhow::bail!("task input exceeds {TASK_INPUT_MAX_BYTES} bytes; send smaller chunks");
        }
        let closes_input = matches!(input, TaskInput::Close);
        {
            let records = self.inner.read().await;
            let Some(record) = records.get(id) else {
                anyhow::bail!("task not found: {id}");
            };
            if record.status.terminal() {
                anyhow::bail!("task {id} is already {}", record.status.as_str());
            }
            match &record.controls.input {
                Some(TaskInputControl::Open(sender)) => match sender.try_send(input) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Full(_)) => anyhow::bail!(
                        "task {id} input buffer is full; wait for the process to consume pending input"
                    ),
                    Err(mpsc::error::TrySendError::Closed(_)) => anyhow::bail!(
                        "task {id} is no longer reading input; its process may have exited; read its current state with a `*** Read: tasks://{id}` request"
                    ),
                },
                Some(TaskInputControl::Closed) => anyhow::bail!(
                    "task {id} input is already closed; wait for completion or use an `*** Exec: tasks://{id}/cancel` request"
                ),
                None => anyhow::bail!(
                    "task {id} does not accept input; rerun the command with interactive=true"
                ),
            }
        }
        if closes_input {
            self.close_input(id).await;
        }
        Ok(())
    }

    /// Marks a task's input closed without touching a still-running process;
    /// the writer that owns the process stdin observes the queued close.
    async fn close_input(&self, id: &str) {
        let mut records = self.inner.write().await;
        if let Some(record) = records.get_mut(id) {
            record.controls.input = Some(TaskInputControl::Closed);
        }
    }

    /// Signals a running interactive task to interrupt its process. Returns
    /// false when the task is unknown, terminal, or has no interrupt signal.
    pub async fn interrupt(&self, id: &str) -> bool {
        let records = self.inner.read().await;
        let Some(record) = records.get(id) else {
            return false;
        };
        if record.status.terminal() {
            return false;
        }
        record.controls.interrupt.as_ref().is_some_and(|interrupt| {
            interrupt.cancel();
            true
        })
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
    async fn automatic_backgrounding_returns_fast_results_inline() {
        let tasks = TaskManager::new();
        let record = tasks.allocate("test", "quick").await;
        let id = record.id.clone();

        let result = tasks
            .run_with_auto_background(record, Duration::from_secs(1), |_| async {
                Ok(b"final result".to_vec())
            })
            .await
            .unwrap();

        let AutoTask::Terminal(result) = result else {
            panic!("quick work unexpectedly became a background task");
        };
        assert_eq!(result.status, TaskStatus::Completed);
        assert_eq!(result.content, b"final result");
        assert!(tasks.get(&id).await.is_none());
    }

    #[tokio::test]
    async fn failed_tasks_preserve_the_full_error_chain() {
        let tasks = TaskManager::new();
        let record = tasks.allocate("test", "failing").await;
        let id = record.id.clone();
        tasks
            .spawn(record, async {
                Err::<Vec<u8>, _>(
                    anyhow::anyhow!("root cause detail").context("outer operation context"),
                )
            })
            .await;

        let failed = tasks.wait(&id, Duration::from_secs(1)).await.unwrap();
        assert_eq!(failed.status, TaskStatus::Failed);
        assert_eq!(
            String::from_utf8(failed.content).unwrap(),
            "outer operation context: root cause detail"
        );
    }

    #[tokio::test]
    async fn automatic_backgrounding_keeps_one_operation_and_its_final_result() {
        let tasks = TaskManager::new();
        let record = tasks.allocate("test", "slow").await;
        let release = Arc::new(tokio::sync::Notify::new());
        let work_release = release.clone();

        let result = tasks
            .run_with_auto_background(record, Duration::from_millis(1), move |_| async move {
                work_release.notified().await;
                Ok(b"final search result".to_vec())
            })
            .await
            .unwrap();

        let AutoTask::Background(id) = result else {
            panic!("slow work unexpectedly completed in the foreground");
        };
        assert!(tasks.get(&id).await.unwrap().background);
        release.notify_one();
        let completed = tasks.wait_until_terminal(&id).await.unwrap();
        assert_eq!(completed.status, TaskStatus::Completed);
        assert_eq!(completed.content, b"final search result");
        assert_eq!(
            tasks
                .pending_terminal_notifications()
                .await
                .iter()
                .map(|record| record.id.as_str())
                .collect::<Vec<_>>(),
            [id.as_str()]
        );
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
    async fn auto_background_promotes_even_when_capacity_is_full() {
        let tasks = TaskManager::new();
        for index in 0..MAX_BACKGROUND_TASKS {
            tasks
                .allocate_background("test", format!("filler {index}"))
                .await
                .unwrap();
        }
        assert!(
            tasks
                .allocate_background("test", "over capacity")
                .await
                .is_err()
        );

        let record = tasks.allocate("test", "foreground").await;
        let id = record.id.clone();
        let outcome = tasks
            .run_with_auto_background(record, Duration::from_millis(20), |_| async {
                time::sleep(Duration::from_secs(5)).await;
                Ok(b"late".to_vec())
            })
            .await
            .unwrap();

        // The promotion must hand back the running task instead of waiting
        // for it to terminate; before this behavior existed the call blocked
        // here until the work finished.
        let AutoTask::Background(promoted) = outcome else {
            panic!("a capacity-full promotion must still return the task id");
        };
        assert_eq!(promoted, id);
        assert!(tasks.list().await.iter().any(|entry| entry.id == id));
        tasks.cancel(&id).await;
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
    async fn interactive_controls_deliver_input_until_the_task_settles() {
        let tasks = TaskManager::new();
        let (controls, mut receiver, interrupt) = TaskControls::interactive();
        let record = tasks.allocate("bash", "interactive").await;
        let id = record.id.clone();
        tasks.set_controls(&id, controls).await;

        assert!(tasks.get(&id).await.unwrap().interruptible());
        tasks
            .send_input(&id, TaskInput::Bytes(b"yes\n".to_vec()))
            .await
            .unwrap();
        assert_eq!(
            receiver.recv().await,
            Some(TaskInput::Bytes(b"yes\n".to_vec()))
        );
        assert!(!interrupt.is_cancelled());

        tasks.send_input(&id, TaskInput::Close).await.unwrap();
        assert_eq!(receiver.recv().await, Some(TaskInput::Close));
        let closed = tasks
            .send_input(&id, TaskInput::Bytes(b"more\n".to_vec()))
            .await
            .unwrap_err()
            .to_string();
        assert!(closed.contains("input is already closed"), "{closed}");
        assert!(
            closed.contains("`*** Exec: tasks://001/cancel` request"),
            "{closed}"
        );

        tasks.spawn(record, async { Ok(b"done".to_vec()) }).await;
        tasks.wait(&id, Duration::from_secs(1)).await.unwrap();

        let settled = tasks
            .send_input(&id, TaskInput::Bytes(b"late\n".to_vec()))
            .await
            .unwrap_err()
            .to_string();
        assert!(settled.contains("already completed"), "{settled}");
        assert!(!tasks.get(&id).await.unwrap().interruptible());
        assert_eq!(receiver.recv().await, None);
    }

    #[tokio::test]
    async fn input_without_registered_controls_is_rejected_with_guidance() {
        let tasks = TaskManager::new();
        let record = tasks.allocate("bash", "plain").await;
        let id = record.id.clone();

        let error = tasks
            .send_input(&id, TaskInput::Bytes(b"x".to_vec()))
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("does not accept input")
                && error.contains("rerun the command with interactive=true"),
            "{error}"
        );
        assert!(!tasks.interrupt(&id).await);

        let oversized = tasks
            .send_input(&id, TaskInput::Bytes(vec![0; TASK_INPUT_MAX_BYTES + 1]))
            .await
            .unwrap_err()
            .to_string();
        assert!(oversized.contains("send smaller chunks"), "{oversized}");
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
    async fn cooperative_cancellation_settles_after_cleanup() {
        let tasks = TaskManager::new();
        let record = tasks
            .allocate_background("bash", "cancelled")
            .await
            .unwrap();
        let id = record.id.clone();
        let cleaned = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let work_cleaned = cleaned.clone();
        tasks
            .spawn_with_cancellation(record, move |cancellation| async move {
                cancellation.cancelled().await;
                time::sleep(Duration::from_millis(20)).await;
                work_cleaned.store(true, Ordering::Release);
                anyhow::bail!("cancelled")
            })
            .await;

        assert!(tasks.cancel(&id).await);
        let record = tasks.wait_until_terminal(&id).await.unwrap();

        assert_eq!(record.status, TaskStatus::Cancelled);
        assert!(cleaned.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn accepted_cooperative_cancellation_wins_the_terminal_commit_race() {
        let tasks = TaskManager::new();
        let record = tasks
            .allocate_background("bash", "racing completion")
            .await
            .unwrap();
        let id = record.id.clone();
        let release = Arc::new(tokio::sync::Notify::new());
        let work_release = release.clone();
        tasks
            .spawn_with_cancellation(record, move |_cancellation| async move {
                work_release.notified().await;
                Ok(b"completed".to_vec())
            })
            .await;
        while tasks.get(&id).await.unwrap().status != TaskStatus::Running {
            tokio::task::yield_now().await;
        }

        let records = tasks.inner.write().await;
        let cancel_started = Arc::new(tokio::sync::Notify::new());
        let cancelling_started = cancel_started.clone();
        let cancelling_tasks = tasks.clone();
        let cancelling_id = id.clone();
        let cancelling = tokio::spawn(async move {
            cancelling_started.notify_one();
            cancelling_tasks.cancel(&cancelling_id).await
        });
        cancel_started.notified().await;
        release.notify_one();
        tokio::task::yield_now().await;
        drop(records);

        assert!(cancelling.await.unwrap());
        let record = tasks.wait_until_terminal(&id).await.unwrap();
        assert_eq!(record.status, TaskStatus::Cancelled);
        assert_ne!(record.content, b"completed");
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
