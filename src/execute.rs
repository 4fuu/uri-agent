//! Non-interactive execute mode.
//!
//! `uat -x "<prompt>"` runs exactly one prompt turn without the
//! line REPL, prints the final assistant reply to stdout, and
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
/// blank line, so `cat file | uat -x "explain this"` asks about the
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
/// The run uses the simple tool set (`read`, `write`, `edit`, `bash`,
/// `search`, `notes`, `steer`, `codemode`, `mcp`, `apply_patch`).
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
                "no model is configured: pass --model <provider>/<id>; `uat models` lists \
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
                    "model {}/{} is not in the model catalog; `uat models` lists runnable \
IDs",
                    initial.provider,
                    initial.model
                );
            }
        }
    }
    let spec = simple_spec(
        &initial.provider,
        &initial.model,
        initial.thinking,
        &config.cwd,
        true,
    );
    let agent = host.open_root(requested, spec).await?;
    let result = run_agent(&agent, prompt).await;
    agent.close().await;
    result
}

/// Build the tool-only session used by `uat -x`, `uat serve`, and the line
/// REPL.
pub(crate) fn simple_spec(
    provider: &str,
    model: &str,
    thinking: crate::catalog::ThinkingLevel,
    cwd: &std::path::Path,
    non_interactive: bool,
) -> AgentSpec {
    let spec = AgentSpec::root(provider, model, thinking, cwd).with_tools([
        "read",
        "write",
        "edit",
        "bash",
        "search",
        "notes",
        "steer",
        "codemode",
        "mcp",
        "apply_patch",
    ]);
    if non_interactive {
        spec.append_system_prompt(prompts::EXECUTE_MODE_PROMPT)
    } else {
        spec
    }
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
    let submission = runtime.submit_exclusive(prompt.to_string()).await?;
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
    fn simple_spec_selects_the_text_tools() {
        let spec = simple_spec(
            "provider",
            "model",
            crate::catalog::ThinkingLevel::Medium,
            std::path::Path::new("/work"),
            true,
        );
        let crate::agent::CapabilitySelection::Only(tools) = &spec.tools else {
            panic!("simple mode must select tools explicitly");
        };
        assert_eq!(
            tools,
            &[
                "read",
                "write",
                "edit",
                "bash",
                "search",
                "notes",
                "steer",
                "codemode",
                "mcp",
                "apply_patch"
            ]
        );
        assert!(matches!(
            spec.system_prompt,
            crate::agent::SystemPromptSelection::Append(_)
        ));
    }
}
