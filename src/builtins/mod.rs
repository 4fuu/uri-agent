mod edit;
mod file;
mod shell;

use crate::protocol::ProtocolRegistry;
use crate::task::{TaskManager, TaskRecord};
use anyhow::{Result, anyhow, bail};
use std::fmt::Write as _;
use std::path::Path;
use std::time::Duration;

pub use edit::EditProtocol;
pub use file::FileProtocol;
pub use shell::{ShellProtocol, discover_shells};

pub fn register(registry: &mut ProtocolRegistry, cwd: &Path) -> Result<()> {
    registry.register(FileProtocol::new(cwd))?;
    registry.register(EditProtocol::new(cwd))?;
    for shell in discover_shells(cwd) {
        registry.register(shell)?;
    }
    Ok(())
}

async fn render_task(tasks: &TaskManager, protocol: &str, id: &str) -> Result<Vec<u8>> {
    let record = tasks
        .get(id)
        .await
        .ok_or_else(|| anyhow!("task not found: {id}"))?;
    if record.protocol != protocol {
        bail!("task {id} belongs to {}://", record.protocol);
    }
    Ok(render_record(&record).into_bytes())
}

async fn task_response(
    tasks: &TaskManager,
    protocol: &str,
    id: &str,
    wait: Option<Duration>,
) -> Result<Vec<u8>> {
    let Some(wait) = wait else {
        return Ok(crate::prompts::task_accepted(protocol, id).into_bytes());
    };
    let record = tasks
        .wait(id, wait)
        .await
        .ok_or_else(|| anyhow!("task disappeared: {id}"))?;
    if record.status.terminal() {
        return Ok(render_record(&record).into_bytes());
    }
    Ok(format!(
        "{}\nWait window elapsed; the task is still {}.",
        crate::prompts::task_accepted(protocol, id),
        record.status.as_str()
    )
    .into_bytes())
}

fn split_wait(target: &str) -> Result<(&str, Option<Duration>)> {
    let complete_target = target;
    let Some((target, query)) = complete_target.rsplit_once('?') else {
        return Ok((target, None));
    };
    if !query.starts_with("wait=") {
        return Ok((complete_target, None));
    }
    let seconds = query
        .strip_prefix("wait=")
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or_else(|| anyhow!("wait must be an integer number of seconds"))?;
    if seconds > 300 {
        bail!("wait cannot exceed 300 seconds");
    }
    Ok((target, Some(Duration::from_secs(seconds))))
}

async fn render_task_list(tasks: &TaskManager, protocol: &str) -> Vec<u8> {
    let records = tasks.list().await;
    let mut output = String::new();
    for record in records
        .into_iter()
        .filter(|record| record.protocol == protocol)
    {
        let _ = writeln!(
            output,
            "{}  {:9}  {}",
            record.id,
            record.status.as_str(),
            record.label
        );
    }
    if output.is_empty() {
        output.push_str("No tasks.\n");
    }
    output.into_bytes()
}

fn render_record(record: &TaskRecord) -> String {
    let finished = record
        .finished_at
        .map(|value| value.to_rfc3339())
        .unwrap_or_else(|| "—".to_string());
    let mut output = format!(
        "Task: {}\nStatus: {}\nLabel: {}\nStarted: {}\nFinished: {}\n",
        record.id,
        record.status.as_str(),
        record.label,
        record.started_at.to_rfc3339(),
        finished
    );
    if !record.content.is_empty() {
        output.push('\n');
        output.push_str(&String::from_utf8_lossy(&record.content));
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wait_is_an_opt_in_protocol_query() {
        assert_eq!(split_wait("run").unwrap(), ("run", None));
        assert_eq!(
            split_wait("?wait=30").unwrap(),
            ("", Some(Duration::from_secs(30)))
        );
        assert_eq!(
            split_wait("a?literal=yes").unwrap(),
            ("a?literal=yes", None)
        );
        assert!(split_wait("run?wait=301").is_err());
    }
}
