//! Non-interactive execute mode.
//!
//! `uri-agent -x "<prompt>"` runs exactly one prompt turn without the
//! terminal interface, prints the final assistant reply to stdout, and
//! exits. Brief progress — tool calls starting and finishing, task changes,
//! model retries, notices, and errors — goes to stderr one line per event
//! with no ANSI escapes, so the reply stays pipeable while the run stays
//! observable. The turn commits to the session like any other, so
//! `--continue-session` can reopen it later.
//!
//! New execute sessions freeze an execute-mode system prompt fragment
//! ([`crate::prompts::EXECUTE_MODE_PROMPT`]) that tells the model no one can
//! answer questions, so it states assumptions instead of asking and leaves
//! approval-requiring actions undone and reported.

use crate::agent::{AgentHandle, AgentHost, AgentSpec};
use crate::config::Config;
use crate::prompts;
use crate::runtime::{AgentRuntime, TurnOutcome};
use crate::session::{EventKind, SessionChoice, SessionUpdate};
use anyhow::{Result, anyhow, bail};
use std::io::IsTerminal;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

/// Cadence for waiting on a resumed session's recovered pending input.
const SETTLE_POLL_INTERVAL: Duration = Duration::from_millis(100);
/// Progress lines stay readable anywhere: event payloads collapse onto one
/// line bounded well inside a wide terminal row.
const PROGRESS_LINE_LIMIT: usize = 200;

/// Read the execute prompt according to the `-x`/`--execute` argument.
///
/// Returns `None` when execute mode was not requested. `-x "<prompt>"`
/// keeps that text as the prompt and appends piped stdin when stdin is not a
/// terminal; `-x` without a value reads the whole prompt from stdin.
pub async fn read_prompt(argument: Option<&str>) -> Result<Option<String>> {
    let Some(argument) = argument else {
        return Ok(None);
    };
    let stdin = if argument.trim().is_empty() || !std::io::stdin().is_terminal() {
        read_stdin().await?
    } else {
        String::new()
    };
    Ok(Some(assemble_prompt(Some(argument), &stdin)?))
}

async fn read_stdin() -> Result<String> {
    let mut text = String::new();
    tokio::io::stdin().read_to_string(&mut text).await?;
    Ok(text)
}

/// Combine the `-x`/`--execute` argument with piped stdin text into the one
/// prompt submitted for the run.
///
/// A non-empty argument is the prompt and piped stdin follows it after a
/// blank line, so `cat file | uri-agent -x "explain this"` asks about the
/// piped content. An argument-less run uses stdin alone. Whitespace-only
/// input is rejected.
pub fn assemble_prompt(argument: Option<&str>, stdin: &str) -> Result<String> {
    let argument = argument.unwrap_or_default().trim();
    let stdin = stdin.trim();
    let prompt = match (argument.is_empty(), stdin.is_empty()) {
        (true, true) => String::new(),
        (true, false) => stdin.to_string(),
        (false, true) => argument.to_string(),
        (false, false) => format!("{argument}\n\n{stdin}"),
    };
    if prompt.is_empty() {
        bail!("the execute prompt is empty: pass -x \"<prompt>\" or pipe text through stdin");
    }
    Ok(prompt)
}

/// Run one non-interactive prompt turn and print the final reply to stdout.
///
/// The session is chosen like a TUI launch: a new session by default, or the
/// one named by `--session`/`--continue-session`. New sessions freeze the
/// execute-mode prompt fragment; resumed sessions keep the prompt they
/// froze.
pub async fn run(host: &AgentHost, config: &Config, prompt: &str) -> Result<()> {
    let requested = match &config.session {
        SessionChoice::New => None,
        SessionChoice::Latest => Some("latest"),
        SessionChoice::Existing(id) => Some(id.as_str()),
    };
    let mut initial = config.manager.current().await;
    if requested.is_none() {
        if !initial.model_configured() {
            bail!(
                "no model is configured: pass --model <provider>/<id>; `uri-agent models` lists \
runnable IDs"
            );
        }
        // A model discovered after the cached catalog was written is found by
        // one refresh; anything still missing cannot run.
        if initial.catalog_model(&config.catalog).await.is_none() {
            if let Some(warning) = config.refresh_catalog_for_cli().await {
                eprintln!("warning: {warning}");
            }
            initial = config.manager.current().await;
            if initial.catalog_model(&config.catalog).await.is_none() {
                bail!(
                    "model {}/{} is not in the model catalog; `uri-agent models` lists runnable \
IDs",
                    initial.provider,
                    initial.model
                );
            }
        }
    }
    let spec = AgentSpec::root(
        &initial.provider,
        &initial.model,
        initial.thinking,
        &config.cwd,
    )
    .append_system_prompt(prompts::EXECUTE_MODE_PROMPT);
    let agent = host.open_root(requested, spec).await?;
    let result = run_agent(&agent, prompt).await;
    agent.close().await;
    result
}

async fn run_agent(agent: &AgentHandle, prompt: &str) -> Result<()> {
    let runtime = agent.services().runtime.clone();
    eprintln!("session {}", agent.session_id());
    let mut updates = runtime.session().subscribe();
    runtime.prepare_context().await?;
    runtime.refresh_context_estimate().await;
    // A resumed session may replay recovered pending input as a turn first;
    // let it settle so this run's prompt owns one exclusive turn.
    if runtime.turn_running().await {
        eprintln!("waiting for recovered input to settle");
    }
    loop {
        drain_updates(&mut updates);
        if !runtime.turn_running().await {
            break;
        }
        tokio::time::sleep(SETTLE_POLL_INTERVAL).await;
    }
    let cancellation = CancellationToken::new();
    let interrupt = cancellation.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            interrupt.cancel();
        }
    });
    let answer = run_turn(&runtime, prompt, updates, stderr_line, cancellation).await?;
    println!("{answer}");
    Ok(())
}

fn stderr_line(line: String) {
    eprintln!("{line}");
}

/// Print one progress line per still-queued session update.
fn drain_updates(updates: &mut broadcast::Receiver<SessionUpdate>) {
    while let Ok(update) = updates.try_recv() {
        report_update(update, &mut stderr_line);
    }
}

/// Hand one session update to the progress sink as a single line, if it
/// carries execute-mode progress.
fn report_update(update: SessionUpdate, progress: &mut impl FnMut(String)) {
    if let Some(line) = progress_line(&update) {
        progress(line);
    }
}

/// Submit `prompt` as the run's single turn and return the final assistant
/// reply.
///
/// The turn is submitted exclusively, like ACP prompts, so the completion
/// watched here belongs to this submission. `updates` must be subscribed
/// before the turn starts; `progress` receives one line per tool call, task
/// change, model retry, notice, or error. `cancellation` interrupts the turn;
/// the runner then keeps waiting until the interrupted turn settles, so
/// cancelled process trees are reaped before it returns a failure.
pub(crate) async fn run_turn(
    runtime: &Arc<AgentRuntime>,
    prompt: &str,
    mut updates: broadcast::Receiver<SessionUpdate>,
    mut progress: impl FnMut(String),
    cancellation: CancellationToken,
) -> Result<String> {
    let session = runtime.session().clone();
    let mut completions = runtime.subscribe_turn_completions();
    let submission = runtime
        .submit_exclusive_with_images(prompt.to_string(), Vec::new())
        .await?;
    let submission_id = submission.id;
    let mut cancelling = false;
    loop {
        tokio::select! {
            update = updates.recv() => match update {
                Ok(update) => report_update(update, &mut progress),
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    progress("progress lines lagged; continuing".to_string());
                }
                Err(broadcast::error::RecvError::Closed) => {}
            },
            completion = completions.recv() => {
                let completion = match completion {
                    Ok(completion) => completion,
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        bail!("cannot watch the turn: completion stream lagged");
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        bail!("cannot watch the turn: completion stream closed");
                    }
                };
                if completion.submission_id != submission_id {
                    continue;
                }
                // The transcript commits before the completion publishes, but
                // this loop may observe the completion first; drain anything
                // still queued so the last progress lines are not lost.
                while let Ok(update) = updates.try_recv() {
                    report_update(update, &mut progress);
                }
                if cancelling {
                    bail!("the turn was cancelled");
                }
                return match completion.outcome {
                    TurnOutcome::Completed => session.last_assistant_text().await.ok_or_else(|| {
                        anyhow!("the turn finished without an assistant answer")
                    }),
                    TurnOutcome::Cancelled => bail!("the turn was cancelled"),
                    TurnOutcome::Failed(error) => bail!("{error}"),
                };
            }
            _ = cancellation.cancelled(), if !cancelling => {
                cancelling = true;
                let _ = runtime.interrupt_submission(submission_id).await;
            }
        }
    }
}

/// One-line summary of a session update for stderr progress, or `None` for
/// events that carry no execute-mode progress.
fn progress_line(update: &SessionUpdate) -> Option<String> {
    let kind = match update {
        SessionUpdate::Persisted(event) => &event.kind,
        SessionUpdate::Transient(kind) => kind,
    };
    match kind {
        EventKind::ToolCall {
            name, arguments, ..
        } => Some(format!(
            "tool {name} started: {}",
            summarize(&serde_json::to_string(arguments).unwrap_or_default())
        )),
        EventKind::ToolResult {
            name,
            failed,
            output,
            ..
        } => {
            if *failed {
                Some(format!("tool {name} failed: {}", summarize(output)))
            } else {
                Some(format!("tool {name} finished"))
            }
        }
        EventKind::ModelRetry {
            attempt,
            max_retries,
            delay_ms,
            reason,
        } => Some(format!(
            "model retry {attempt} of {max_retries} in {delay_ms} ms: {}",
            summarize(reason)
        )),
        EventKind::Task {
            id, status, label, ..
        } => Some(format!(
            "task {id} {}: {}",
            status.as_str(),
            summarize(label)
        )),
        EventKind::Notice { text } => Some(summarize(text)),
        EventKind::Error { text } => Some(format!("error: {}", summarize(text))),
        _ => None,
    }
}

/// Collapse an event payload onto one bounded line.
fn summarize(text: &str) -> String {
    let mut line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.len() > PROGRESS_LINE_LIMIT {
        let mut end = PROGRESS_LINE_LIMIT;
        while !line.is_char_boundary(end) {
            end -= 1;
        }
        line.truncate(end);
        line.push_str("...");
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        ModelBackend, ModelDelta, ModelFailure, ModelFailureKind, ModelRequest, ModelResponse,
    };
    use crate::output::OutputStore;
    use crate::protocol::{
        Protocol, ProtocolContext, ProtocolDescriptor, ProtocolOutput, ProtocolRegistry,
        ProtocolRequest,
    };
    use crate::session::{Session, SessionContext};
    use crate::task::{TaskManager, TaskStatus};
    use async_trait::async_trait;
    use rig::completion::FinishReason;
    use rig::message::{AssistantContent, ToolCallId, ToolFunction};
    use serde_json::json;
    use std::collections::VecDeque;
    use std::path::{Path, PathBuf};
    use tokio::sync::{Mutex, Notify};

    #[test]
    fn assembled_prompts_combine_argument_and_stdin() {
        assert_eq!(
            assemble_prompt(Some("explain this"), "").unwrap(),
            "explain this"
        );
        assert_eq!(assemble_prompt(Some("  spaced  "), "\n").unwrap(), "spaced");
        assert_eq!(
            assemble_prompt(None, "piped prompt\n").unwrap(),
            "piped prompt"
        );
        assert_eq!(
            assemble_prompt(Some(""), "piped prompt\n").unwrap(),
            "piped prompt"
        );
        assert_eq!(
            assemble_prompt(Some("explain this"), "one\ntwo\n").unwrap(),
            "explain this\n\none\ntwo"
        );
        for (argument, stdin) in [(None, ""), (Some(""), " \n\t"), (Some("   "), "")] {
            let error = assemble_prompt(argument, stdin).unwrap_err();
            assert!(
                error.to_string().contains("the execute prompt is empty"),
                "{argument:?} + {stdin:?}"
            );
        }
    }

    #[test]
    fn progress_lines_summarize_each_kind_on_one_line() {
        let persisted = |kind| {
            SessionUpdate::Persisted(crate::session::SessionEvent {
                sequence: 0,
                at: chrono::Utc::now(),
                kind,
            })
        };
        assert_eq!(
            progress_line(&persisted(EventKind::TurnFinished)),
            None,
            "turn bookkeeping is not progress"
        );
        let started = progress_line(&persisted(EventKind::ToolCall {
            call_id: "1".to_string(),
            name: "protocol".to_string(),
            arguments: json!({"steps": [{"read": "file://main.rs"}]}),
        }))
        .unwrap();
        assert_eq!(
            started,
            "tool protocol started: {\"steps\":[{\"read\":\"file://main.rs\"}]}"
        );
        assert_eq!(
            progress_line(&persisted(EventKind::ToolResult {
                call_id: "1".to_string(),
                name: "protocol".to_string(),
                output: "done".to_string(),
                failed: false,
                protocol_help_required: false,
            }))
            .unwrap(),
            "tool protocol finished"
        );
        let failed = progress_line(&persisted(EventKind::ToolResult {
            call_id: "2".to_string(),
            name: "protocol".to_string(),
            output: "boom\nsecond line".to_string(),
            failed: true,
            protocol_help_required: false,
        }))
        .unwrap();
        assert_eq!(failed, "tool protocol failed: boom second line");
        assert_eq!(
            progress_line(&persisted(EventKind::ModelRetry {
                attempt: 2,
                max_retries: 5,
                delay_ms: 500,
                reason: "network error".to_string(),
            }))
            .unwrap(),
            "model retry 2 of 5 in 500 ms: network error"
        );
        assert_eq!(
            progress_line(&persisted(EventKind::Task {
                id: "9".to_string(),
                protocol: "bash".to_string(),
                label: "run tests".to_string(),
                status: TaskStatus::Completed,
                output: None,
            }))
            .unwrap(),
            "task 9 completed: run tests"
        );
        assert_eq!(
            progress_line(&persisted(EventKind::Notice {
                text: "one\n\ttwo".to_string()
            }))
            .unwrap(),
            "one two"
        );
        assert_eq!(
            progress_line(&persisted(EventKind::Error {
                text: "model broke".to_string()
            }))
            .unwrap(),
            "error: model broke"
        );
        let long = progress_line(&persisted(EventKind::Notice {
            text: "x".repeat(PROGRESS_LINE_LIMIT * 2),
        }))
        .unwrap();
        assert!(long.len() <= PROGRESS_LINE_LIMIT + 3, "{}", long.len());
        assert!(long.ends_with("..."));
    }

    struct DemoProtocol;

    #[async_trait]
    impl Protocol for DemoProtocol {
        fn descriptor(&self) -> ProtocolDescriptor {
            ProtocolDescriptor {
                name: "demo".to_string(),
                description: "serve one fixed answer".to_string(),
                can_read: true,
                can_exec: false,
            }
        }

        async fn read(
            &self,
            request: ProtocolRequest<'_>,
            _context: ProtocolContext,
        ) -> anyhow::Result<ProtocolOutput> {
            if request.target == "help" {
                request.reject_input()?;
                return Ok(b"# demo\nread demo://answer for the answer".to_vec().into());
            }
            Ok(b"42".to_vec().into())
        }

        async fn exec(
            &self,
            request: ProtocolRequest<'_>,
            _context: ProtocolContext,
        ) -> anyhow::Result<ProtocolOutput> {
            if request.target == "help" {
                bail!("demo help is read-only");
            }
            bail!("demo is read-only: {}", request.uri)
        }
    }

    #[derive(Default)]
    struct FakeBackend {
        responses: Mutex<VecDeque<Result<ModelResponse>>>,
        requests: Mutex<Vec<ModelRequest>>,
    }

    impl FakeBackend {
        fn scripted(responses: Vec<Result<ModelResponse>>) -> Arc<Self> {
            Arc::new(Self {
                responses: Mutex::new(responses.into()),
                requests: Mutex::new(Vec::new()),
            })
        }
    }

    #[async_trait]
    impl ModelBackend for FakeBackend {
        async fn complete(
            &self,
            request: ModelRequest,
            _deltas: tokio::sync::mpsc::UnboundedSender<ModelDelta>,
        ) -> Result<ModelResponse> {
            self.requests.lock().await.push(request);
            self.responses
                .lock()
                .await
                .pop_front()
                .expect("a scripted response for every model request")
        }
    }

    struct BlockingBackend {
        started: Notify,
        release: Notify,
    }

    #[async_trait]
    impl ModelBackend for BlockingBackend {
        async fn complete(
            &self,
            _request: ModelRequest,
            _deltas: tokio::sync::mpsc::UnboundedSender<ModelDelta>,
        ) -> Result<ModelResponse> {
            self.started.notify_one();
            self.release.notified().await;
            text_response("released but cancelled")
        }
    }

    fn text_response(text: &str) -> Result<ModelResponse> {
        content_response(vec![AssistantContent::text(text)])
    }

    fn content_response(content: Vec<AssistantContent>) -> Result<ModelResponse> {
        Ok(ModelResponse {
            content,
            usage: None,
            context_tokens: None,
            finish_reason: Some(FinishReason::Stop),
        })
    }

    fn help_call(id: &str, protocols: &[&str]) -> AssistantContent {
        AssistantContent::ToolCall(rig::message::ToolCall::new(
            ToolCallId::new(id).unwrap(),
            ToolFunction::new("help".to_string(), json!({ "protocols": protocols })),
        ))
    }

    fn read_call(id: &str, uri: &str) -> AssistantContent {
        AssistantContent::ToolCall(rig::message::ToolCall::new(
            ToolCallId::new(id).unwrap(),
            ToolFunction::new("protocol".to_string(), json!({"steps": [{"read": uri}]})),
        ))
    }

    async fn runtime_with(
        workspace: &Path,
        backend: Arc<dyn ModelBackend>,
    ) -> (Arc<AgentRuntime>, Session, PathBuf) {
        let session_id = format!("test{}", uuid::Uuid::now_v7().simple());
        let session = Session::open_at(
            workspace.join("sessions.db"),
            Some(&session_id),
            workspace,
            "fake",
            "fake-model",
            SessionContext {
                system_prompt: "system".to_string(),
                skills: Vec::new(),
            },
        )
        .await
        .unwrap();
        let output = Arc::new(OutputStore::new(&session_id, 32 * 1024).await.unwrap());
        let output_directory = output.directory().to_path_buf();
        let mut protocols = ProtocolRegistry::new(output, TaskManager::new());
        protocols.register(DemoProtocol).unwrap();
        let mut model_tools = crate::plugin::ModelToolRegistry::new();
        crate::builtins::model_tools::register_protocol_tools(&mut model_tools).unwrap();
        let runtime = Arc::new(AgentRuntime::new(
            Some(backend),
            Arc::new(protocols),
            Arc::new(model_tools),
            session.clone(),
            "system".to_string(),
            crate::catalog::ModelLimits::default(),
        ));
        (runtime, session, output_directory)
    }

    #[tokio::test]
    async fn run_turn_returns_the_final_answer_with_one_line_tool_progress() {
        let workspace = tempfile::tempdir().unwrap();
        let backend = FakeBackend::scripted(vec![
            content_response(vec![help_call("load", &["demo"])]),
            content_response(vec![read_call("first", "demo://answer")]),
            text_response("The answer is 42"),
        ]);
        let (runtime, session, output_directory) = runtime_with(workspace.path(), backend).await;
        let mut lines = Vec::new();
        let answer = run_turn(
            &runtime,
            "read demo://answer and report",
            runtime.session().subscribe(),
            |line| lines.push(line),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(answer, "The answer is 42");
        assert_eq!(
            lines,
            vec![
                "tool help started: {\"protocols\":[\"demo\"]}".to_string(),
                "tool help finished".to_string(),
                "tool protocol started: {\"steps\":[{\"read\":\"demo://answer\"}]}".to_string(),
                "tool protocol finished".to_string(),
            ]
        );
        assert!(
            session.is_persisted().await,
            "the first user turn persists the session for later resume"
        );
        runtime.shutdown().await;
        let _ = tokio::fs::remove_dir_all(output_directory).await;
    }

    #[tokio::test]
    async fn run_turn_fails_when_the_model_fails() {
        let workspace = tempfile::tempdir().unwrap();
        let failure = ModelFailure::for_test(ModelFailureKind::Client, None, "billing declined");
        let backend = FakeBackend::scripted(vec![Err(failure.into())]);
        let (runtime, _session, output_directory) = runtime_with(workspace.path(), backend).await;
        let mut lines = Vec::new();
        let error = run_turn(
            &runtime,
            "any prompt",
            runtime.session().subscribe(),
            |line| lines.push(line),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("billing declined"), "{error:#}");
        assert!(
            lines.iter().any(|line| line.starts_with("error:")),
            "{lines:?}"
        );
        runtime.shutdown().await;
        let _ = tokio::fs::remove_dir_all(output_directory).await;
    }

    #[tokio::test]
    async fn run_turn_fails_when_the_turn_ends_without_an_answer() {
        let workspace = tempfile::tempdir().unwrap();
        let reasoning = AssistantContent::Reasoning(rig::message::Reasoning {
            id: None,
            content: vec![rig::message::ReasoningContent::Text {
                text: "thinking only".to_string(),
                signature: None,
            }],
        });
        let backend = FakeBackend::scripted(vec![content_response(vec![reasoning])]);
        let (runtime, _session, output_directory) = runtime_with(workspace.path(), backend).await;
        let error = run_turn(
            &runtime,
            "any prompt",
            runtime.session().subscribe(),
            |_| (),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("the turn finished without an assistant answer"),
            "{error:#}"
        );
        runtime.shutdown().await;
        let _ = tokio::fs::remove_dir_all(output_directory).await;
    }

    #[tokio::test]
    async fn cancelling_interrupts_the_turn_and_reports_failure() {
        let workspace = tempfile::tempdir().unwrap();
        let backend = Arc::new(BlockingBackend {
            started: Notify::new(),
            release: Notify::new(),
        });
        let (runtime, _session, output_directory) =
            runtime_with(workspace.path(), backend.clone()).await;
        let cancellation = CancellationToken::new();
        let running = {
            let runtime = runtime.clone();
            let updates = runtime.session().subscribe();
            let cancellation = cancellation.clone();
            tokio::spawn(async move {
                run_turn(&runtime, "slow prompt", updates, |_| (), cancellation).await
            })
        };
        wait_started(&backend.started).await;
        cancellation.cancel();
        let error = tokio::time::timeout(Duration::from_secs(10), running)
            .await
            .expect("the cancelled turn settles")
            .unwrap()
            .unwrap_err();
        assert_eq!(error.to_string(), "the turn was cancelled");
        assert!(!runtime.turn_running().await);
        runtime.shutdown().await;
        let _ = tokio::fs::remove_dir_all(output_directory).await;
    }

    async fn wait_started(started: &Notify) {
        tokio::time::timeout(Duration::from_secs(10), started.notified())
            .await
            .expect("the model request starts");
    }

    #[tokio::test]
    async fn new_sessions_freeze_the_execute_prompt_and_resumed_sessions_keep_it() {
        let workspace = tempfile::tempdir().unwrap();
        let config_directory = workspace.path().join("config");
        tokio::fs::create_dir_all(&config_directory).await.unwrap();
        let manager =
            crate::config::ConfigManager::load_for_test(&config_directory, workspace.path())
                .await
                .unwrap();
        let environment = Arc::new(
            crate::config::AgentEnvironment::load(&config_directory)
                .await
                .unwrap(),
        );
        let catalog = Arc::new(
            crate::catalog::ModelCatalog::load(&config_directory, true)
                .await
                .unwrap(),
        );
        let host = AgentHost::new(
            manager.clone(),
            environment,
            catalog,
            workspace.path().to_path_buf(),
        )
        .await
        .unwrap();
        let initial = manager.current().await;
        let spec = AgentSpec::root(
            &initial.provider,
            &initial.model,
            initial.thinking,
            workspace.path(),
        )
        .append_system_prompt(prompts::EXECUTE_MODE_PROMPT);
        let created = host.open_root(None, spec.clone()).await.unwrap();
        created.services().runtime.prepare_context().await.unwrap();
        created
            .services()
            .runtime
            .session()
            .persist()
            .await
            .unwrap();
        let frozen = created
            .services()
            .runtime
            .session()
            .context()
            .await
            .system_prompt;
        assert!(frozen.contains("non-interactive execute mode"));
        assert!(frozen.ends_with(prompts::EXECUTE_MODE_PROMPT));
        created.close().await;

        let session_id = find_only_session(workspace.path()).await;
        let resumed = host.open_root(Some(&session_id), spec).await.unwrap();
        resumed.services().runtime.prepare_context().await.unwrap();
        assert_eq!(
            resumed
                .services()
                .runtime
                .session()
                .context()
                .await
                .system_prompt,
            frozen
        );
        resumed.close().await;
    }

    async fn find_only_session(cwd: &Path) -> String {
        let sessions = Session::list_for(cwd).await.unwrap();
        assert_eq!(sessions.len(), 1, "exactly the execute session exists");
        sessions[0].id.clone()
    }
}
