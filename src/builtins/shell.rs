use super::{render_task, render_task_list, split_wait, task_response};
use crate::prompts;
use crate::protocol::{Protocol, ProtocolContext, ProtocolDescriptor, ProtocolRequest};
use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

pub struct ShellProtocol {
    name: &'static str,
    executable: PathBuf,
    cwd: PathBuf,
}

impl ShellProtocol {
    fn new(name: &'static str, executable: PathBuf, cwd: &Path) -> Self {
        Self {
            name,
            executable,
            cwd: cwd.to_path_buf(),
        }
    }

    pub fn name(&self) -> &str {
        self.name
    }
}

pub fn discover_shells(cwd: &Path) -> Vec<ShellProtocol> {
    let mut shells = Vec::new();
    if let Some(executable) = find_executable("bash") {
        shells.push(ShellProtocol::new("bash", executable, cwd));
    }
    if let Some(executable) = find_executable("pwsh") {
        shells.push(ShellProtocol::new("pwsh", executable, cwd));
    }
    shells
}

#[async_trait]
impl Protocol for ShellProtocol {
    fn descriptor(&self) -> ProtocolDescriptor {
        ProtocolDescriptor {
            name: self.name.to_string(),
            description: if self.name == "bash" {
                "Run Bash commands as managed asynchronous tasks."
            } else {
                "Run PowerShell commands as managed asynchronous tasks."
            }
            .to_string(),
            can_read: true,
            can_exec: true,
        }
    }

    async fn read(
        &self,
        request: ProtocolRequest<'_>,
        context: ProtocolContext,
    ) -> Result<Vec<u8>> {
        match request.target {
            "help" => Ok(if self.name == "bash" {
                prompts::BASH_HELP
            } else {
                prompts::PWSH_HELP
            }
            .as_bytes()
            .to_vec()),
            "tasks" => Ok(render_task_list(&context.tasks, self.name).await),
            target => {
                let id = target.strip_prefix("tasks/").ok_or_else(|| {
                    anyhow!(
                        "expected {}://help or {}://tasks/<id>",
                        self.name,
                        self.name
                    )
                })?;
                render_task(&context.tasks, self.name, id).await
            }
        }
    }

    async fn exec(
        &self,
        request: ProtocolRequest<'_>,
        context: ProtocolContext,
    ) -> Result<Vec<u8>> {
        let (target, wait) = split_wait(request.target)?;
        if !matches!(target, "" | "run") {
            bail!(
                "expected {}://run or {}://?wait=<seconds>",
                self.name,
                self.name
            );
        }
        let body = request.body.cloned();
        let executable = self.executable.clone();
        let cwd = self.cwd.clone();
        let protocol = self.name.to_string();
        let record = context
            .tasks
            .allocate(self.name, command_label(body.as_ref()))
            .await;
        let id = record.id.clone();
        let tasks = context.tasks.clone();
        tasks
            .spawn(record, async move {
                let command = command_from_body(body.as_ref())?;
                execute(&protocol, &executable, &cwd, command).await
            })
            .await;
        task_response(&context.tasks, self.name, &id, wait).await
    }
}

fn command_from_body(body: Option<&Value>) -> Result<&str> {
    match body {
        Some(Value::String(command)) => Ok(command),
        Some(Value::Object(object)) => object
            .get("command")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("shell body object requires a command string")),
        _ => bail!("shell body must be a command string or an object with command"),
    }
}

fn command_label(body: Option<&Value>) -> String {
    let command = command_from_body(body).unwrap_or("invalid command");
    let mut label = command
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_string();
    if label.chars().count() > 72 {
        label = label.chars().take(71).collect::<String>() + "…";
    }
    label
}

async fn execute(protocol: &str, executable: &Path, cwd: &Path, script: &str) -> Result<Vec<u8>> {
    let mut command = Command::new(executable);
    if protocol == "bash" {
        command.args(["--noprofile", "--norc"]);
    } else {
        command.args(["-NoLogo", "-NoProfile", "-NonInteractive", "-Command", "-"]);
    }
    let mut child = command
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("failed to open shell stdin"))?;
    stdin.write_all(script.as_bytes()).await?;
    drop(stdin);
    let output = child.wait_with_output().await?;
    let mut result = format!("Exit: {}\n", output.status).into_bytes();
    if !output.stdout.is_empty() {
        result.extend_from_slice(b"\nstdout:\n");
        result.extend_from_slice(&output.stdout);
    }
    if !output.stderr.is_empty() {
        result.extend_from_slice(b"\nstderr:\n");
        result.extend_from_slice(&output.stderr);
    }
    if output.status.success() {
        Ok(result)
    } else {
        Err(anyhow!(String::from_utf8_lossy(&result).into_owned()))
    }
}

fn find_executable(name: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    for directory in std::env::split_paths(&paths) {
        let candidate = directory.join(name);
        if is_executable(&candidate) {
            return Some(candidate);
        }
        #[cfg(windows)]
        for extension in std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string())
            .split(';')
        {
            let candidate = directory.join(format!("{name}{extension}"));
            if is_executable(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[tokio::test]
    async fn shell_is_async_by_default_and_can_opt_into_a_bounded_wait() {
        let directory = tempfile::tempdir().unwrap();
        let Some(shell) = discover_shells(directory.path())
            .into_iter()
            .find(|shell| shell.name == "bash")
        else {
            return;
        };
        let context = ProtocolContext {
            tasks: crate::task::TaskManager::new(),
        };
        let command = Value::String("sleep 0.2; printf async-ok".to_string());
        let started = Instant::now();
        let accepted = shell
            .exec(
                ProtocolRequest {
                    uri: "bash://run",
                    target: "run",
                    body: Some(&command),
                },
                context.clone(),
            )
            .await
            .unwrap();
        assert!(started.elapsed() < Duration::from_millis(150));
        assert!(
            String::from_utf8(accepted)
                .unwrap()
                .contains("Task accepted")
        );

        let command = Value::String("printf wait-ok".to_string());
        let completed = shell
            .exec(
                ProtocolRequest {
                    uri: "bash://?wait=2",
                    target: "?wait=2",
                    body: Some(&command),
                },
                context,
            )
            .await
            .unwrap();
        let completed = String::from_utf8(completed).unwrap();
        assert!(completed.contains("Status: completed"));
        assert!(completed.contains("wait-ok"));
    }
}
