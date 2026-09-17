//! Herdr terminal-multiplexer reporting.
//!
//! Herdr is a terminal multiplexer that runs coding agents in owned panes and
//! tracks each pane's lifecycle state. It exports `HERDR_ENV`,
//! `HERDR_PANE_ID`, and `HERDR_BIN_PATH` to processes in its panes. When the
//! TUI starts with that environment, the reporter publishes the visible
//! session's `working` or `idle` state and its stable session ID through
//! Herdr's CLI, so the pane appears in Herdr's agent list with authoritative
//! state instead of screen detection. A second display source also publishes
//! the terminal title the interface applies, renaming the pane's agent entry
//! after the task. Outside Herdr the reporter stays completely inactive, and
//! reporting failures never affect the conversation.

use crate::runtime::AgentRuntime;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::process::Command;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

const POLL_INTERVAL: Duration = Duration::from_millis(500);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(2);
const REPORT_SOURCE: &str = "custom:uri-agent";
const REPORT_AGENT: &str = "uri-agent";
/// Display metadata uses its own source so it never competes with the
/// lifecycle source for pane authority.
const DISPLAY_SOURCE: &str = "custom:uri-agent-display";

/// Seed the report sequence at the current Unix time in milliseconds so a
/// restarted process keeps issuing sequence numbers above the ones Herdr
/// already recorded for this source. Herdr drops reports whose sequence is
/// not increasing, so a restart from 1 could leave every later report
/// ignored and the pane stuck on the previous process's last state.
fn initial_seq() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map_or(1, |since| since.as_millis() as u64)
}

/// Pane lifecycle state reported to Herdr.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HerdrState {
    /// A model turn is running, or queued input is waiting to start one.
    Working,
    /// No turn is running; the conversation is ready for input.
    Idle,
}

impl HerdrState {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::Idle => "idle",
        }
    }

    /// Queued input counts as working because the runtime starts the next
    /// turn from it without waiting for a user decision.
    fn from_snapshot(working: bool, queued: usize) -> Self {
        if working || queued > 0 {
            Self::Working
        } else {
            Self::Idle
        }
    }
}

/// The Herdr CLI binary and pane a reporter reports for.
#[derive(Clone, Debug, PartialEq, Eq)]
struct HerdrTarget {
    bin: PathBuf,
    pane: String,
}

impl HerdrTarget {
    /// Detect a Herdr pane from the current environment. Returns `None`
    /// unless `HERDR_ENV=1` and both `HERDR_BIN_PATH` and `HERDR_PANE_ID`
    /// are present and non-empty, so the integration is a no-op outside
    /// Herdr.
    fn from_env() -> Option<Self> {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    fn from_lookup<F>(lookup: F) -> Option<Self>
    where
        F: Fn(&str) -> Option<String>,
    {
        if lookup("HERDR_ENV").as_deref() != Some("1") {
            return None;
        }
        let bin = lookup("HERDR_BIN_PATH").filter(|value| !value.is_empty())?;
        let pane = lookup("HERDR_PANE_ID").filter(|value| !value.is_empty())?;
        Some(Self {
            bin: PathBuf::from(bin),
            pane,
        })
    }

    fn report_arguments(&self, state: HerdrState, session_id: &str, seq: u64) -> Vec<String> {
        vec![
            "pane",
            "report-agent",
            self.pane.as_str(),
            "--source",
            REPORT_SOURCE,
            "--agent",
            REPORT_AGENT,
            "--state",
            state.as_str(),
            "--agent-session-id",
            session_id,
            "--seq",
            seq.to_string().as_str(),
        ]
        .into_iter()
        .map(String::from)
        .collect()
    }

    /// The release sequence must be strictly above the last report sequence:
    /// Herdr drops a release whose sequence is not newer than the source's
    /// recorded sequence, and then keeps the pane's agent entry until the
    /// pane closes.
    fn release_arguments(&self, seq: u64) -> Vec<String> {
        vec![
            "pane",
            "release-agent",
            self.pane.as_str(),
            "--source",
            REPORT_SOURCE,
            "--agent",
            REPORT_AGENT,
            "--seq",
            seq.to_string().as_str(),
        ]
        .into_iter()
        .map(String::from)
        .collect()
    }

    /// Publish the session's terminal title as display metadata: the pane's
    /// agent entry is renamed to the title and the same text is exposed as
    /// the `summary` token for Herdr sidebar rows. The guard keeps both tied
    /// to this reporter's lifecycle source, so they disappear with it.
    fn metadata_arguments(&self, title: &str, seq: u64) -> Vec<String> {
        vec![
            "pane",
            "report-metadata",
            self.pane.as_str(),
            "--source",
            DISPLAY_SOURCE,
            "--applies-to-source",
            REPORT_SOURCE,
            "--display-agent",
            title,
            "--token",
            format!("summary={title}").as_str(),
            "--seq",
            seq.to_string().as_str(),
        ]
        .into_iter()
        .map(String::from)
        .collect()
    }

    fn metadata_clear_arguments(&self, seq: u64) -> Vec<String> {
        vec![
            "pane",
            "report-metadata",
            self.pane.as_str(),
            "--source",
            DISPLAY_SOURCE,
            "--applies-to-source",
            REPORT_SOURCE,
            "--clear-display-agent",
            "--clear-token",
            "summary",
            "--seq",
            seq.to_string().as_str(),
        ]
        .into_iter()
        .map(String::from)
        .collect()
    }
}

/// Reports the TUI's visible session lifecycle to a Herdr pane.
///
/// One reporter serves the whole TUI process. Session switches swap the
/// watched runtime, and the reported session ID follows the visible
/// conversation. Reports only spawn Herdr's CLI and ignore failures, so a
/// missing or older Herdr cannot affect the conversation.
pub struct HerdrReporter {
    inner: Arc<HerdrInner>,
}

struct HerdrInner {
    target: HerdrTarget,
    runtime: RwLock<Option<Arc<AgentRuntime>>>,
    title: RwLock<Option<watch::Receiver<String>>>,
    generation: AtomicU64,
    seq: AtomicU64,
    cancellation: CancellationToken,
    worker: tokio::sync::Mutex<Option<JoinHandle<()>>>,
}

impl HerdrReporter {
    /// Create a reporter for the current environment, or `None` outside a
    /// Herdr pane.
    pub fn from_env() -> Option<Self> {
        Some(Self::from_target(HerdrTarget::from_env()?))
    }

    fn from_target(target: HerdrTarget) -> Self {
        Self {
            inner: Arc::new(HerdrInner {
                target,
                runtime: RwLock::new(None),
                title: RwLock::new(None),
                generation: AtomicU64::new(0),
                seq: AtomicU64::new(initial_seq()),
                cancellation: CancellationToken::new(),
                worker: tokio::sync::Mutex::new(None),
            }),
        }
    }

    /// Point the reporter at the session now shown in the pane. Repeated
    /// calls swap the watched runtime and terminal-title receiver without
    /// restarting the reporting worker.
    pub async fn start(&self, runtime: Arc<AgentRuntime>, terminal_title: watch::Receiver<String>) {
        *self
            .inner
            .runtime
            .write()
            .expect("herdr runtime lock poisoned") = Some(runtime);
        *self.inner.title.write().expect("herdr title lock poisoned") = Some(terminal_title);
        self.inner.generation.fetch_add(1, Ordering::Relaxed);
        let mut worker = self.inner.worker.lock().await;
        if worker.is_none() {
            let inner = self.inner.clone();
            *worker = Some(tokio::spawn(async move { run(inner).await }));
        }
    }

    /// Stop reporting and release the pane's lifecycle authority so Herdr
    /// stops treating this process as a live agent source.
    pub async fn shutdown(&self) {
        self.inner.cancellation.cancel();
        if let Some(worker) = self.inner.worker.lock().await.take() {
            let _ = worker.await;
        }
        let seq = self.inner.seq.fetch_add(1, Ordering::Relaxed) + 1;
        run_command(&self.inner.target, self.inner.target.release_arguments(seq)).await;
    }
}

impl HerdrInner {
    fn current_runtime(&self) -> Option<Arc<AgentRuntime>> {
        self.runtime
            .read()
            .expect("herdr runtime lock poisoned")
            .clone()
    }

    fn current_title(&self) -> Option<String> {
        self.title
            .read()
            .expect("herdr title lock poisoned")
            .as_ref()
            .map(|receiver| receiver.borrow().clone())
    }
}

async fn run(inner: Arc<HerdrInner>) {
    let mut reported: Option<(HerdrState, String)> = None;
    let mut reported_title: Option<String> = None;
    let mut generation = inner.generation.load(Ordering::Relaxed);
    loop {
        tokio::select! {
            () = inner.cancellation.cancelled() => break,
            _ = tokio::time::sleep(POLL_INTERVAL) => {}
        }
        let current = inner.generation.load(Ordering::Relaxed);
        if current != generation {
            // A session switch replaced the watched session. Forget its
            // state and retract its title so the pane does not keep the
            // previous conversation's name.
            generation = current;
            reported = None;
            if reported_title.take().is_some() {
                let seq = inner.seq.fetch_add(1, Ordering::Relaxed) + 1;
                let arguments = inner.target.metadata_clear_arguments(seq);
                run_command(&inner.target, arguments).await;
            }
        }
        let Some(runtime) = inner.current_runtime() else {
            continue;
        };
        let (working, queued) = runtime.herdr_snapshot().await;
        let state = HerdrState::from_snapshot(working, queued);
        let session_id = runtime.session().id().to_string();
        if reported
            .as_ref()
            .is_none_or(|last| last.0 != state || last.1 != session_id)
        {
            // Herdr ignores stale sequence numbers from one source, so the
            // strictly increasing counter keeps last-writer-wins even when
            // two dispatched reports race inside Herdr's CLI.
            let seq = inner.seq.fetch_add(1, Ordering::Relaxed) + 1;
            let arguments = inner.target.report_arguments(state, &session_id, seq);
            dispatch(&inner.target, arguments);
            reported = Some((state, session_id));
        }
        let title = inner.current_title().filter(|title| !title.is_empty());
        if title.is_some() && title != reported_title {
            let seq = inner.seq.fetch_add(1, Ordering::Relaxed) + 1;
            let arguments = inner
                .target
                .metadata_arguments(title.as_deref().unwrap_or_default(), seq);
            dispatch(&inner.target, arguments);
            reported_title = title;
        }
    }
    // Retract the title before the caller releases the lifecycle source:
    // the metadata guard only matches while that source still holds pane
    // authority.
    if reported_title.is_some() {
        let seq = inner.seq.fetch_add(1, Ordering::Relaxed) + 1;
        let arguments = inner.target.metadata_clear_arguments(seq);
        run_command(&inner.target, arguments).await;
    }
}

fn dispatch(target: &HerdrTarget, arguments: Vec<String>) {
    let mut command = Command::new(&target.bin);
    command
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Ok(mut child) = command.spawn() {
        tokio::spawn(async move {
            let _ = child.wait().await;
        });
    }
}

/// Run one Herdr CLI call to completion. Used where later commands depend on
/// the effect, such as clearing display metadata before releasing the pane.
async fn run_command(target: &HerdrTarget, arguments: Vec<String>) {
    let mut command = Command::new(&target.bin);
    command
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Ok(mut child) = command.spawn() {
        let _ = tokio::time::timeout(COMMAND_TIMEOUT, child.wait()).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::ModelLimits;
    use crate::plugin::ModelToolRegistry;
    use crate::protocol::ProtocolRegistry;
    use crate::session::{Session, SessionContext};
    use crate::task::TaskManager;

    fn value_at(values: &[(&str, &str)], key: &str) -> Option<String> {
        values
            .iter()
            .find(|(name, _)| *name == key)
            .map(|(_, value)| (*value).to_string())
    }

    #[test]
    fn target_requires_the_complete_herdr_environment() {
        let complete = [
            ("HERDR_ENV", "1"),
            ("HERDR_BIN_PATH", "/usr/local/bin/herdr"),
            ("HERDR_PANE_ID", "w1:p2"),
        ];
        let target = HerdrTarget::from_lookup(|key| value_at(&complete, key)).unwrap();
        assert_eq!(target.pane, "w1:p2");
        assert_eq!(target.bin, PathBuf::from("/usr/local/bin/herdr"));

        // Missing any one variable, or a non-1 gate, keeps the reporter off.
        assert!(HerdrTarget::from_lookup(|key| value_at(&[], key)).is_none());
        for index in 0..complete.len() {
            let partial: Vec<(&str, &str)> = complete
                .iter()
                .enumerate()
                .filter_map(|(position, pair)| (position != index).then_some(*pair))
                .collect();
            assert!(
                HerdrTarget::from_lookup(|key| value_at(&partial, key)).is_none(),
                "reporter must stay off without {}",
                complete[index].0
            );
        }
        let disabled = [
            ("HERDR_ENV", "0"),
            ("HERDR_BIN_PATH", "/usr/local/bin/herdr"),
            ("HERDR_PANE_ID", "w1:p2"),
        ];
        assert!(HerdrTarget::from_lookup(|key| value_at(&disabled, key)).is_none());
        let blank = [
            ("HERDR_ENV", "1"),
            ("HERDR_BIN_PATH", ""),
            ("HERDR_PANE_ID", "w1:p2"),
        ];
        assert!(HerdrTarget::from_lookup(|key| value_at(&blank, key)).is_none());
    }

    #[test]
    fn queued_input_reports_working() {
        assert_eq!(HerdrState::from_snapshot(false, 0), HerdrState::Idle);
        assert_eq!(HerdrState::from_snapshot(true, 0), HerdrState::Working);
        assert_eq!(HerdrState::from_snapshot(false, 2), HerdrState::Working);
        assert_eq!(HerdrState::from_snapshot(true, 1), HerdrState::Working);
    }

    #[test]
    fn report_and_release_arguments_address_one_stable_source() {
        let target = HerdrTarget {
            bin: PathBuf::from("/bin/herdr"),
            pane: "w1:p2".to_string(),
        };
        let report = target.report_arguments(HerdrState::Working, "sess-1", 7);
        assert_eq!(
            report,
            [
                "pane",
                "report-agent",
                "w1:p2",
                "--source",
                "custom:uri-agent",
                "--agent",
                "uri-agent",
                "--state",
                "working",
                "--agent-session-id",
                "sess-1",
                "--seq",
                "7",
            ]
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>()
        );
        let idle = target.report_arguments(HerdrState::Idle, "sess-1", 8);
        assert!(idle.contains(&"--state".to_string()));
        assert!(idle.contains(&"idle".to_string()));

        let release = target.release_arguments(8);
        assert_eq!(
            release,
            [
                "pane",
                "release-agent",
                "w1:p2",
                "--source",
                "custom:uri-agent",
                "--agent",
                "uri-agent",
                "--seq",
                "8",
            ]
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>()
        );
        // A release must carry a sequence above the last report sequence:
        // Herdr silently drops an unsequenced or stale-sequenced release and
        // keeps the pane's agent entry until the pane closes.
        assert!(release.last().is_some_and(|value| value != "7"));
    }

    #[test]
    fn metadata_arguments_rename_through_a_guarded_display_source() {
        let target = HerdrTarget {
            bin: PathBuf::from("/bin/herdr"),
            pane: "w1:p2".to_string(),
        };
        let report = target.metadata_arguments("Fix parser recovery", 9);
        assert_eq!(
            report,
            [
                "pane",
                "report-metadata",
                "w1:p2",
                "--source",
                "custom:uri-agent-display",
                "--applies-to-source",
                "custom:uri-agent",
                "--display-agent",
                "Fix parser recovery",
                "--token",
                "summary=Fix parser recovery",
                "--seq",
                "9",
            ]
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>()
        );
        let clear = target.metadata_clear_arguments(10);
        assert_eq!(
            clear,
            [
                "pane",
                "report-metadata",
                "w1:p2",
                "--source",
                "custom:uri-agent-display",
                "--applies-to-source",
                "custom:uri-agent",
                "--clear-display-agent",
                "--clear-token",
                "summary",
                "--seq",
                "10",
            ]
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>()
        );
    }

    async fn runtime_fixture(
        database: &std::path::Path,
        cwd: &std::path::Path,
        id: &str,
    ) -> Arc<AgentRuntime> {
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

    #[tokio::test]
    async fn reporter_swaps_the_watched_runtime_and_shuts_down() {
        let temp = tempfile::tempdir().unwrap();
        let database = temp.path().join("sessions.db");
        let cwd = temp.path().join("project");
        tokio::fs::create_dir_all(&cwd).await.unwrap();
        let first = runtime_fixture(&database, &cwd, "first-session").await;
        let second = runtime_fixture(&database, &cwd, "second-session").await;

        // A Herdr binary that does not exist keeps dispatch a silent no-op.
        let reporter = HerdrReporter::from_target(HerdrTarget {
            bin: PathBuf::from("/nonexistent/herdr"),
            pane: "w1:p1".to_string(),
        });
        let (first_title, first_receiver) = watch::channel(String::new());
        let (_second_title, second_receiver) = watch::channel(String::new());
        first_title.send("First task".to_string()).unwrap();
        reporter.start(first, first_receiver).await;
        assert!(reporter.inner.worker.lock().await.is_some());
        assert_eq!(
            reporter.inner.current_title().as_deref(),
            Some("First task")
        );

        reporter.start(second, second_receiver).await;
        let current = reporter.inner.current_runtime().unwrap();
        assert_eq!(current.session().id(), "second-session");
        assert_eq!(reporter.inner.current_title().as_deref(), Some(""));

        tokio::time::timeout(Duration::from_secs(2), reporter.shutdown())
            .await
            .expect("shutdown must not hang");
        assert!(reporter.inner.worker.lock().await.is_none());
    }
}
