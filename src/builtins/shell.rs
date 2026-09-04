use crate::plugin::{Plugin, PluginEnvironment, PluginHost, PluginPermission, PluginRegistry};
use crate::process::{PWSH_STDIN_BOOTSTRAP, ProcessTree};
use crate::prompts;
use crate::protocol::{Protocol, ProtocolContext, ProtocolDescriptor, ProtocolRequest};
use crate::task::{AutoTask, TaskManager};
use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::time::{self, Instant};
use tokio_util::sync::CancellationToken;

// The Out-String default keeps formatted tables from truncating columns at the
// inherited console width; streaming still flows through Out-Default.
const PWSH_UTF8_PREFIX: &str = "$OutputEncoding = [Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false); if ($null -ne $PSStyle) { $PSStyle.OutputRendering = 'PlainText' }; $PSDefaultParameterValues['Out-String:Width'] = 4096; ";
// Width-aware tools (ps, git, docker, kubectl) size output to COLUMNS and fall
// back to 80 columns when it is unset; export a wide default so piped output is
// not truncated. Bash leaves COLUMNS untouched in non-interactive shells.
const BASH_PREFIX: &str = "export COLUMNS=4096\n";
const PWSH_EXIT_EPILOGUE: &str = "\n; $__uri_agent_ok = $?; $__uri_agent_native = $global:LASTEXITCODE; if ($__uri_agent_ok) { $global:__uri_agent_exit_code = 0 } elseif ($null -ne $__uri_agent_native -and $__uri_agent_native -ne 0) { $global:__uri_agent_exit_code = $__uri_agent_native } else { $global:__uri_agent_exit_code = 1 }";
const PWSH_WINDOWS_WARNING: &str =
    "PowerShell 7 or newer was not found on Windows; pwsh:// is disabled.";
const EXIT_OUTPUT_DRAIN_GRACE: Duration = Duration::from_millis(100);
const AUTO_BACKGROUND_AFTER: Duration = Duration::from_secs(60);
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const BASH_HELP: &str = r#"# bash

Run Bash commands. Commands start in the foreground and normally return their
final result in the same `exec` call.

`read` supports only `bash://help` and MUST use an empty string body. To run a
command, call `exec` with `bash://run`; the command body MUST contain at least
one non-whitespace character:

```text
exec("bash://run", "cargo test")
```

If a foreground command is still running after about 60 seconds, URI Agent
automatically converts the same process into a background task without
restarting it. Use `background=true` to return a task immediately:

```text
exec("bash://run?background=true", "cargo test")
```

Foreground and background commands share one execution timeout. `timeout` is
an integer number of seconds; omission defaults to 1800 seconds (30 minutes),
and `timeout=0` disables the timeout:

```text
exec("bash://run?timeout=120", "cargo test")
```

You MUST NOT add another background layer inside the command. Child processes
remain owned by this execution and are terminated when the root shell exits or
the task times out or is cancelled. Background task status, output, and
cancellation use the unified `tasks://` protocol. Completion is delivered
automatically, so you MUST NOT poll.

User-managed Agent environment variables are injected into every command. Use
secret values by name and do not print them unless the user explicitly asks.

On success, stdout-only output is returned directly, stderr-only output is
identified by `stderr:`, and both streams are labeled when both exist. A
successful command with no output returns `(no output)`. Failures retain the
exit code or timeout and any output observed before termination.
"#;
const PWSH_HELP: &str = r#"# pwsh

Run PowerShell 7 commands. Commands start in the foreground and normally return
their final result in the same `exec` call.

Write PowerShell 7 syntax rather than Unix shell syntax. Use multiline commands
with normal indentation when they improve readability; do not collapse them
into one line. Single quotes are literal, double quotes expand variables, and
the backtick is the escape character. Set environment variables with
`$env:NAME = 'value'` and quote paths containing spaces.

Prefer modern cross-platform tools such as `rg` and `fd` when available.
PowerShell recursive searches do not honor `.gitignore`, so bound search paths,
depth, and output tightly.

`read` supports only `pwsh://help` and MUST use an empty string body. To run a
command, call `exec` with `pwsh://run`; the command body MUST contain at least
one non-whitespace character:

```text
exec("pwsh://run", "Get-ChildItem -Path . -Force")
```

If a foreground command is still running after about 60 seconds, URI Agent
automatically converts the same process into a background task without
restarting it. Use `background=true` to return a task immediately:

```text
exec("pwsh://run?background=true", "cargo test")
```

Foreground and background commands share one execution timeout. `timeout` is
an integer number of seconds; omission defaults to 1800 seconds (30 minutes),
and `timeout=0` disables the timeout.

You MUST NOT add another background layer inside the command. Child processes
remain owned by this execution and are terminated when the root shell exits or
the task times out or is cancelled. Background task status, output, and
cancellation use the unified `tasks://` protocol. Completion is delivered
automatically, so you MUST NOT poll.

PowerShell source and plain-text output use UTF-8. Command success follows the
final PowerShell or native command, and native exit codes are preserved.

User-managed Agent environment variables are injected into every command. Use
secret values by name and do not print them unless the user explicitly asks.

On success, stdout-only output is returned directly, stderr-only output is
identified by `stderr:`, and both streams are labeled when both exist. A
successful command with no output returns `(no output)`. Failures retain the
exit code or timeout and any output observed before termination.
"#;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ShellOptions {
    background: bool,
    timeout: Option<Duration>,
}

struct ExecutionControl<'a> {
    timeout: Option<Duration>,
    progress: Option<(&'a TaskManager, &'a str)>,
    cancellation: CancellationToken,
}

#[derive(Clone)]
pub(super) struct ShellProtocol {
    name: &'static str,
    executable: PathBuf,
    cwd: PathBuf,
    environment: Option<PluginEnvironment>,
}

impl ShellProtocol {
    fn new(name: &'static str, executable: PathBuf, cwd: &Path) -> Self {
        Self {
            name,
            executable,
            cwd: cwd.to_path_buf(),
            environment: None,
        }
    }
}

#[derive(Clone)]
struct PwshPlugin {
    protocol: Option<ShellProtocol>,
    suppresses_bash: bool,
    warning: Option<String>,
}

impl PwshPlugin {
    fn detect(
        cwd: &Path,
        windows: bool,
        find: &mut impl FnMut(&str) -> Option<PathBuf>,
        supports_pwsh_7: impl FnOnce(&Path) -> bool,
    ) -> Option<Self> {
        if !windows {
            return None;
        }
        let protocol = find("pwsh").and_then(|executable| {
            supports_pwsh_7(&executable).then(|| ShellProtocol::new("pwsh", executable, cwd))
        });
        Some(Self {
            suppresses_bash: protocol.is_some(),
            warning: protocol.is_none().then(|| PWSH_WINDOWS_WARNING.to_string()),
            protocol,
        })
    }
}

impl Plugin for PwshPlugin {
    fn protocol_descriptors(&self) -> Vec<ProtocolDescriptor> {
        self.protocol.iter().map(Protocol::descriptor).collect()
    }

    fn startup_notices(&self) -> Vec<String> {
        self.warning.iter().cloned().collect()
    }

    fn permissions(&self) -> Vec<PluginPermission> {
        self.protocol
            .as_ref()
            .map(|_| PluginPermission::Environment)
            .into_iter()
            .collect()
    }

    fn register(&self, host: &mut PluginHost<'_>) -> Result<()> {
        if let Some(protocol) = &self.protocol {
            let mut protocol = protocol.clone();
            protocol.environment = Some(host.environment()?);
            host.protocols.register(protocol)?;
        }
        Ok(())
    }
}

pub(super) fn add_plugins(plugins: &mut PluginRegistry, cwd: &Path) {
    add_plugins_with(
        plugins,
        cwd,
        cfg!(windows),
        find_executable,
        supports_pwsh_7,
    );
}

fn add_plugins_with(
    plugins: &mut PluginRegistry,
    cwd: &Path,
    windows: bool,
    mut find: impl FnMut(&str) -> Option<PathBuf>,
    supports_pwsh_7: impl FnOnce(&Path) -> bool,
) {
    let pwsh = PwshPlugin::detect(cwd, windows, &mut find, supports_pwsh_7);
    if !pwsh.as_ref().is_some_and(|plugin| plugin.suppresses_bash)
        && let Some(executable) = find("bash")
    {
        plugins.add(ShellProtocol::new("bash", executable, cwd));
    }
    if let Some(pwsh) = pwsh {
        plugins.add(pwsh);
    }
}

impl Plugin for ShellProtocol {
    fn protocol_descriptors(&self) -> Vec<ProtocolDescriptor> {
        vec![self.descriptor()]
    }

    fn permissions(&self) -> Vec<PluginPermission> {
        vec![PluginPermission::Environment]
    }

    fn register(&self, host: &mut PluginHost<'_>) -> Result<()> {
        let mut protocol = self.clone();
        protocol.environment = Some(host.environment()?);
        host.protocols.register(protocol)
    }
}

#[async_trait]
impl Protocol for ShellProtocol {
    fn descriptor(&self) -> ProtocolDescriptor {
        ProtocolDescriptor {
            name: self.name.to_string(),
            description: if self.name == "bash" {
                "Run Bash commands in the foreground or as managed background tasks."
            } else {
                "Run PowerShell commands in the foreground or as managed background tasks."
            }
            .to_string(),
            can_read: true,
            can_exec: true,
        }
    }

    async fn read(
        &self,
        request: ProtocolRequest<'_>,
        _context: ProtocolContext,
    ) -> Result<Vec<u8>> {
        if request.target != "help" {
            bail!(
                r#"{0} read supports only help; use read("{0}://help", "") or run a command with exec("{0}://run", "<command>")"#,
                self.name
            );
        }
        if !request.body.is_empty() {
            bail!(
                r#"{0}://help requires an empty body; retry read("{0}://help", "")"#,
                self.name
            );
        }
        Ok(if self.name == "bash" {
            BASH_HELP
        } else {
            PWSH_HELP
        }
        .as_bytes()
        .to_vec())
    }

    async fn exec(
        &self,
        request: ProtocolRequest<'_>,
        context: ProtocolContext,
    ) -> Result<Vec<u8>> {
        self.exec_with_auto_background(request, context, AUTO_BACKGROUND_AFTER)
            .await
    }
}

impl ShellProtocol {
    async fn exec_with_auto_background(
        &self,
        request: ProtocolRequest<'_>,
        context: ProtocolContext,
        auto_background_after: Duration,
    ) -> Result<Vec<u8>> {
        let options = parse_target(request.target).with_context(|| {
            format!(
                r#"invalid {0} exec; use exec("{0}://run", "<command>")"#,
                self.name
            )
        })?;
        let command = command_from_body(request.body, self.name)?.to_string();
        let executable = self.executable.clone();
        let cwd = self.cwd.clone();
        let protocol = self.name.to_string();
        let environment = self
            .environment
            .clone()
            .ok_or_else(|| anyhow!("shell environment is not attached"))?;
        let record = if options.background {
            context
                .tasks
                .allocate_background(self.name, command_label(&command))
                .await?
        } else {
            context
                .tasks
                .allocate(self.name, command_label(&command))
                .await
        };
        let id = record.id.clone();
        let tasks = context.tasks.clone();
        let progress_tasks = tasks.clone();
        let progress_id = id.clone();
        let work = move |cancellation| async move {
            let environment = environment.snapshot().await;
            execute_with_cancellation(
                &protocol,
                &executable,
                &cwd,
                &command,
                &environment,
                ExecutionControl {
                    timeout: options.timeout,
                    progress: Some((&progress_tasks, &progress_id)),
                    cancellation,
                },
            )
            .await
        };
        if options.background {
            tasks.spawn_with_cancellation(record, work).await;
            return Ok(prompts::task_accepted(&id).into_bytes());
        }
        match tasks
            .run_with_auto_background(record, auto_background_after, work)
            .await?
        {
            AutoTask::Background(id) => Ok(prompts::task_accepted(&id).into_bytes()),
            AutoTask::Terminal(record) => record.terminal_result("shell command"),
        }
    }
}

fn parse_target(target: &str) -> Result<ShellOptions> {
    let (route, query) = target
        .split_once('?')
        .map_or((target, None), |(route, query)| (route, Some(query)));
    if route != "run" {
        bail!("expected shell target run");
    }
    let mut background = false;
    let mut timeout = DEFAULT_TIMEOUT;
    let mut saw_background = false;
    let mut saw_timeout = false;
    if let Some(query) = query {
        if query.is_empty() {
            bail!("shell query cannot be empty");
        }
        for option in query.split('&') {
            let (name, value) = option
                .split_once('=')
                .ok_or_else(|| anyhow!("shell options must use name=value"))?;
            match name {
                "background" if !saw_background => {
                    background = match value {
                        "true" => true,
                        "false" => false,
                        _ => bail!("shell background must be true or false"),
                    };
                    saw_background = true;
                }
                "timeout" if !saw_timeout => {
                    timeout = Duration::from_secs(value.parse::<u64>().map_err(|_| {
                        anyhow!("shell timeout must be an integer number of seconds")
                    })?);
                    saw_timeout = true;
                }
                "background" | "timeout" => bail!("duplicate shell option: {name}"),
                _ => bail!("unknown shell option: {name}"),
            }
        }
    }
    Ok(ShellOptions {
        background,
        timeout: (!timeout.is_zero()).then_some(timeout),
    })
}

fn command_from_body<'a>(body: &'a str, protocol: &str) -> Result<&'a str> {
    if body.trim().is_empty() {
        bail!(
            r#"{protocol} command body must contain a non-whitespace character; use exec("{protocol}://run", "<command>")"#
        );
    }
    Ok(body)
}

fn command_label(command: &str) -> String {
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

fn bash_script_input(script: &str) -> String {
    format!("{BASH_PREFIX}{script}")
}

fn encode_pwsh_script(script: &str) -> String {
    let source = format!(
        "{PWSH_UTF8_PREFIX}$global:LASTEXITCODE = $null; $global:__uri_agent_exit_code = 0; . {{\n{script}{PWSH_EXIT_EPILOGUE}\n}} | Out-Default\nexit $global:__uri_agent_exit_code"
    );
    BASE64.encode(source)
}

#[cfg(test)]
async fn execute(
    protocol: &str,
    executable: &Path,
    cwd: &Path,
    script: &str,
    environment: &BTreeMap<String, String>,
    timeout: Option<Duration>,
    progress: Option<(&TaskManager, &str)>,
) -> Result<Vec<u8>> {
    execute_with_cancellation(
        protocol,
        executable,
        cwd,
        script,
        environment,
        ExecutionControl {
            timeout,
            progress,
            cancellation: CancellationToken::new(),
        },
    )
    .await
}

async fn execute_with_cancellation(
    protocol: &str,
    executable: &Path,
    cwd: &Path,
    script: &str,
    environment: &BTreeMap<String, String>,
    control: ExecutionControl<'_>,
) -> Result<Vec<u8>> {
    let ExecutionControl {
        timeout,
        progress,
        cancellation,
    } = control;
    let deadline = timeout
        .map(|timeout| {
            Instant::now()
                .checked_add(timeout)
                .ok_or_else(|| anyhow!("shell timeout is too large"))
        })
        .transpose()?;
    let mut command = Command::new(executable);
    let input = if protocol == "bash" {
        command.args(["--noprofile", "--norc"]);
        bash_script_input(script)
    } else {
        command.args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            PWSH_STDIN_BOOTSTRAP,
        ]);
        encode_pwsh_script(script)
    };
    command
        .envs(environment)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let (mut child, process_tree) = ProcessTree::spawn(&mut command)?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("failed to open shell stdin"))?;
    enum InputWrite {
        Complete(std::io::Result<()>),
        TimedOut,
        Cancelled,
    }
    let write_result = {
        let write_deadline = time::sleep_until(
            deadline.unwrap_or_else(|| Instant::now() + Duration::from_secs(365 * 24 * 60 * 60)),
        );
        let write = stdin.write_all(input.as_bytes());
        tokio::pin!(write_deadline);
        tokio::pin!(write);
        tokio::select! {
            biased;
            _ = &mut write_deadline, if timeout.is_some() => InputWrite::TimedOut,
            _ = cancellation.cancelled() => InputWrite::Cancelled,
            result = &mut write => InputWrite::Complete(result),
        }
    };
    if !matches!(&write_result, InputWrite::Complete(Ok(()))) {
        drop(stdin);
        process_tree.terminate_and_wait(&mut child).await?;
        match write_result {
            InputWrite::TimedOut => bail!(
                "Command timed out after {}s.",
                timeout
                    .expect("a write timeout requires a configured timeout")
                    .as_secs()
            ),
            InputWrite::Cancelled => bail!("shell command was cancelled"),
            InputWrite::Complete(Err(error)) => return Err(error.into()),
            InputWrite::Complete(Ok(())) => unreachable!("successful writes returned above"),
        }
    }
    drop(stdin);

    let mut stdout = Some(
        child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("failed to open shell stdout"))?,
    );
    let mut stderr = Some(
        child
            .stderr
            .take()
            .ok_or_else(|| anyhow!("failed to open shell stderr"))?,
    );
    let mut stdout_content = Vec::new();
    let mut stderr_content = Vec::new();
    let mut stdout_buffer = [0_u8; 8192];
    let mut stderr_buffer = [0_u8; 8192];
    let mut status = None;
    let mut timed_out = false;
    let mut cancelled = false;
    let exit_output_drain = time::sleep(Duration::from_secs(365 * 24 * 60 * 60));
    let deadline = time::sleep_until(
        deadline.unwrap_or_else(|| Instant::now() + Duration::from_secs(365 * 24 * 60 * 60)),
    );
    tokio::pin!(exit_output_drain);
    tokio::pin!(deadline);

    loop {
        if status.is_some() && stdout.is_none() && stderr.is_none() {
            break;
        }
        // After the parent exits, drain ready pipe data for a bounded period
        // before terminating descendants that retained inherited handles.
        tokio::select! {
            biased;
            _ = &mut deadline, if timeout.is_some() => {
                timed_out = true;
                break;
            }
            _ = cancellation.cancelled() => {
                cancelled = true;
                break;
            }
            _ = &mut exit_output_drain, if status.is_some() => break,
            result = child.wait(), if status.is_none() => {
                status = Some(result?);
                exit_output_drain.as_mut().reset(Instant::now() + EXIT_OUTPUT_DRAIN_GRACE);
            }
            (is_stdout, result) = async {
                tokio::select! {
                    result = async {
                        stdout
                            .as_mut()
                            .expect("stdout read is guarded")
                            .read(&mut stdout_buffer)
                            .await
                    }, if stdout.is_some() => (true, result),
                    result = async {
                        stderr
                            .as_mut()
                            .expect("stderr read is guarded")
                            .read(&mut stderr_buffer)
                            .await
                    }, if stderr.is_some() => (false, result),
                }
            }, if stdout.is_some() || stderr.is_some() => {
                let count = result?;
                if count == 0 {
                    if is_stdout {
                        stdout = None;
                    } else {
                        stderr = None;
                    }
                } else {
                    let content = if is_stdout {
                        &stdout_buffer[..count]
                    } else {
                        &stderr_buffer[..count]
                    };
                    if is_stdout {
                        stdout_content.extend_from_slice(content);
                    } else {
                        stderr_content.extend_from_slice(content);
                    }
                    if let Some((tasks, id)) = progress {
                        tasks.append_latest_output(id, content).await;
                    }
                }
            }
        }
    }

    if timed_out || cancelled {
        process_tree.terminate_and_wait(&mut child).await?;
        if cancelled {
            bail!("shell command was cancelled");
        }
        let mut result = format!(
            "Command timed out after {}s.",
            timeout
                .expect("the timeout branch is only enabled when configured")
                .as_secs()
        )
        .into_bytes();
        append_process_output(&mut result, &stdout_content, &stderr_content, false);
        return Err(anyhow!(String::from_utf8_lossy(&result).into_owned()));
    }
    let status = status.ok_or_else(|| anyhow!("shell process exited without a status"))?;
    process_tree.terminate();
    if status.success() {
        let mut result = Vec::new();
        append_process_output(&mut result, &stdout_content, &stderr_content, true);
        Ok(result)
    } else {
        let mut result = status.code().map_or_else(
            || format!("Command terminated: {status}.").into_bytes(),
            |code| format!("Command exited with code {code}.").into_bytes(),
        );
        append_process_output(&mut result, &stdout_content, &stderr_content, false);
        Err(anyhow!(String::from_utf8_lossy(&result).into_owned()))
    }
}

fn append_process_output(result: &mut Vec<u8>, stdout: &[u8], stderr: &[u8], empty_marker: bool) {
    if stdout.is_empty() && stderr.is_empty() {
        if empty_marker {
            result.extend_from_slice(b"(no output)");
        }
        return;
    }
    if !result.is_empty() {
        result.extend_from_slice(b"\n\n");
    }
    match (stdout.is_empty(), stderr.is_empty()) {
        (false, true) => result.extend_from_slice(stdout),
        (true, false) => {
            result.extend_from_slice(b"stderr:\n");
            result.extend_from_slice(stderr);
        }
        (false, false) => {
            result.extend_from_slice(b"stdout:\n");
            result.extend_from_slice(stdout);
            if !stdout.ends_with(b"\n") {
                result.push(b'\n');
            }
            result.extend_from_slice(b"\nstderr:\n");
            result.extend_from_slice(stderr);
        }
        (true, true) => unreachable!("empty streams returned above"),
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

fn supports_pwsh_7(executable: &Path) -> bool {
    std::process::Command::new(executable)
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "if ($PSVersionTable.PSVersion.Major -ge 7) { exit 0 } else { exit 1 }",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtins::tasks::TasksProtocol;
    use crate::config::AgentEnvironment;
    use crate::task::TaskStatus;
    use base64::engine::general_purpose::STANDARD as BASE64;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    #[test]
    fn process_output_uses_only_the_stream_labels_the_model_needs() {
        let render = |stdout: &[u8], stderr: &[u8], empty_marker| {
            let mut output = Vec::new();
            append_process_output(&mut output, stdout, stderr, empty_marker);
            String::from_utf8(output).unwrap()
        };

        assert_eq!(render(b"out", b"", true), "out");
        assert_eq!(render(b"", b"err", true), "stderr:\nerr");
        assert_eq!(render(b"out", b"err", true), "stdout:\nout\n\nstderr:\nerr");
        assert_eq!(render(b"", b"", true), "(no output)");
        assert_eq!(render(b"", b"", false), "");
    }

    #[test]
    fn pwsh_help_uses_powershell_syntax_and_bounds_shell_work() {
        assert!(PWSH_HELP.contains("PowerShell 7 syntax rather than Unix shell syntax"));
        assert!(PWSH_HELP.contains("`$env:NAME = 'value'`"));
        assert!(PWSH_HELP.contains("do not honor `.gitignore`"));
        assert!(PWSH_HELP.contains("`pwsh://help` and MUST use an empty string body"));
        assert!(PWSH_HELP.contains("command body MUST contain at least\none non-whitespace"));
        assert!(PWSH_HELP.contains("MUST NOT add another background layer"));
        assert!(PWSH_HELP.contains("`background=true`"));
        assert!(PWSH_HELP.contains("`timeout` is\nan integer number of seconds"));
        assert!(PWSH_HELP.contains("Child processes\nremain owned by this execution"));
        assert!(PWSH_HELP.contains("unified `tasks://` protocol"));
        assert!(PWSH_HELP.contains("MUST NOT poll"));
        assert!(PWSH_HELP.contains("Agent environment variables are injected"));
        assert!(BASH_HELP.contains("`bash://help` and MUST use an empty string body"));
        assert!(BASH_HELP.contains("command body MUST contain at least\none non-whitespace"));
        assert!(BASH_HELP.contains("MUST NOT add another background layer"));
        assert!(BASH_HELP.contains("`background=true`"));
        assert!(BASH_HELP.contains("`timeout=0` disables the timeout"));
        assert!(BASH_HELP.contains("Child processes\nremain owned by this execution"));
        assert!(BASH_HELP.contains("unified `tasks://` protocol"));
        assert!(BASH_HELP.contains("MUST NOT poll"));
        assert!(!BASH_HELP.contains("?wait="));
        assert!(BASH_HELP.contains("Agent environment variables are injected"));
    }

    #[test]
    fn shell_plugin_parses_background_and_timeout_options() {
        assert_eq!(
            parse_target("run").unwrap(),
            ShellOptions {
                background: false,
                timeout: Some(DEFAULT_TIMEOUT),
            }
        );
        assert_eq!(
            parse_target("run?background=true&timeout=0").unwrap(),
            ShellOptions {
                background: true,
                timeout: None,
            }
        );
        assert_eq!(
            parse_target("run?timeout=30&background=false").unwrap(),
            ShellOptions {
                background: false,
                timeout: Some(Duration::from_secs(30)),
            }
        );
        assert!(parse_target("run?timeout=not-a-number").is_err());
        assert!(parse_target("run?background=yes").is_err());
        assert!(parse_target("run?timeout=1&timeout=2").is_err());
        assert!(parse_target("run?other=30").is_err());
        assert!(parse_target("?timeout=30").is_err());
    }

    #[test]
    fn shell_body_only_accepts_a_command_string() {
        assert_eq!(
            command_from_body("cargo test", "bash").unwrap(),
            "cargo test"
        );
        assert!(command_from_body("", "bash").is_err());
        assert!(command_from_body(" \n\t", "bash").is_err());
    }

    #[tokio::test]
    async fn shell_route_errors_provide_copyable_read_and_exec_calls() {
        let shell = ShellProtocol::new("bash", PathBuf::from("bash"), Path::new("."));
        let context = ProtocolContext {
            tasks: TaskManager::new(),
        };

        let read_error = shell
            .read(
                ProtocolRequest {
                    uri: "bash://run",
                    target: "run",
                    body: "cargo test",
                },
                context.clone(),
            )
            .await
            .unwrap_err();
        assert!(
            read_error
                .to_string()
                .contains(r#"exec("bash://run", "<command>")"#)
        );

        let exec_error = shell
            .exec(
                ProtocolRequest {
                    uri: "bash://help",
                    target: "help",
                    body: "",
                },
                context.clone(),
            )
            .await
            .unwrap_err();
        assert!(format!("{exec_error:#}").contains(r#"exec("bash://run", "<command>")"#));

        let body_error = shell
            .exec(
                ProtocolRequest {
                    uri: "bash://run",
                    target: "run",
                    body: " \n\t",
                },
                context,
            )
            .await
            .unwrap_err();
        assert!(body_error.to_string().contains("non-whitespace character"));
    }

    #[test]
    fn bash_input_exports_a_wide_columns_default() {
        let input = bash_script_input("printf 'ok'");
        assert!(input.starts_with("export COLUMNS=4096\n"));
        assert!(input.ends_with("printf 'ok'"));
    }

    #[test]
    fn pwsh_source_transport_is_utf8_and_preserves_final_status() {
        let encoded = encode_pwsh_script("Write-Output '中文 ✓' # trailing comment");
        let decoded = String::from_utf8(BASE64.decode(encoded).unwrap()).unwrap();

        assert!(decoded.starts_with(PWSH_UTF8_PREFIX));
        assert!(PWSH_UTF8_PREFIX.contains("$PSDefaultParameterValues['Out-String:Width']"));
        assert!(decoded.contains("Write-Output '中文 ✓' # trailing comment\n; "));
        assert!(decoded.contains("$__uri_agent_native = $global:LASTEXITCODE"));
        assert!(decoded.contains("} | Out-Default\nexit $global:__uri_agent_exit_code"));
        assert!(PWSH_STDIN_BOOTSTRAP.is_ascii());
    }

    #[test]
    fn valid_windows_pwsh_suppresses_bash() {
        let directory = tempfile::tempdir().unwrap();
        let mut plugins = PluginRegistry::new();
        add_plugins_with(
            &mut plugins,
            directory.path(),
            true,
            |name| Some(PathBuf::from(format!("C:\\shells\\{name}.exe"))),
            |_| true,
        );
        let names = plugins
            .protocol_descriptors()
            .unwrap()
            .into_iter()
            .map(|descriptor| descriptor.name)
            .collect::<Vec<_>>();

        assert_eq!(names, vec!["pwsh"]);
        assert!(plugins.startup_notices().is_empty());
        assert!(plugins.system_prompt_fragments().unwrap().is_empty());
    }

    #[test]
    fn unsupported_windows_pwsh_warns_and_leaves_bash_enabled() {
        let directory = tempfile::tempdir().unwrap();
        let mut plugins = PluginRegistry::new();
        add_plugins_with(
            &mut plugins,
            directory.path(),
            true,
            |name| Some(PathBuf::from(name)),
            |_| false,
        );
        let names = plugins
            .protocol_descriptors()
            .unwrap()
            .into_iter()
            .map(|descriptor| descriptor.name)
            .collect::<Vec<_>>();

        assert_eq!(names, vec!["bash"]);
        assert_eq!(plugins.startup_notices(), vec![PWSH_WINDOWS_WARNING]);
    }

    #[test]
    fn missing_windows_pwsh_warns_without_checking_a_version() {
        let directory = tempfile::tempdir().unwrap();
        let mut plugins = PluginRegistry::new();
        add_plugins_with(
            &mut plugins,
            directory.path(),
            true,
            |name| (name == "bash").then(|| PathBuf::from(name)),
            |_| panic!("a missing pwsh executable has no version to check"),
        );

        assert_eq!(plugins.startup_notices(), vec![PWSH_WINDOWS_WARNING]);
        assert_eq!(plugins.protocol_descriptors().unwrap()[0].name, "bash");
    }

    #[test]
    fn non_windows_only_adds_bash_without_checking_pwsh() {
        let directory = tempfile::tempdir().unwrap();
        let mut plugins = PluginRegistry::new();
        add_plugins_with(
            &mut plugins,
            directory.path(),
            false,
            |name| {
                assert_eq!(name, "bash");
                Some(PathBuf::from(name))
            },
            |_| panic!("non-Windows discovery does not require a PowerShell version check"),
        );
        let names = plugins
            .protocol_descriptors()
            .unwrap()
            .into_iter()
            .map(|descriptor| descriptor.name)
            .collect::<Vec<_>>();

        assert_eq!(names, vec!["bash"]);
        assert!(plugins.startup_notices().is_empty());
        assert!(plugins.system_prompt_fragments().unwrap().is_empty());
    }

    #[tokio::test]
    async fn pwsh_round_trips_long_utf8_source_and_preserves_native_exit_code() {
        let Some(executable) = find_executable("pwsh") else {
            return;
        };
        let directory = tempfile::tempdir().unwrap();
        let long_value = "x".repeat(45_000);
        let script = format!(
            "$value = '{long_value}'; Write-Host '中文主机'; Write-Error '中文错误'; [pscustomobject]@{{Name='对象';State='正常'}}; Write-Output \"length=$($value.Length)\""
        );
        let output = execute(
            "pwsh",
            &executable,
            directory.path(),
            &script,
            &BTreeMap::new(),
            None,
            None,
        )
        .await
        .unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("中文主机"));
        assert!(output.contains("Write-Error: 中文错误"));
        assert!(output.contains("对象"));
        assert!(output.contains("length=45000"));
        assert!(!output.contains("CLIXML"));

        let native_failure = if cfg!(windows) {
            "cmd /c exit 7"
        } else {
            "sh -c 'exit 7'"
        };
        let error = execute(
            "pwsh",
            &executable,
            directory.path(),
            native_failure,
            &BTreeMap::new(),
            None,
            None,
        )
        .await
        .unwrap_err()
        .to_string();
        assert_eq!(error, "Command exited with code 7.");
    }

    #[tokio::test]
    async fn pwsh_background_task_preserves_complete_large_utf8_output() {
        let Some(executable) = find_executable("pwsh") else {
            return;
        };
        let directory = tempfile::tempdir().unwrap();
        let mut shell = ShellProtocol::new("pwsh", executable, directory.path());
        shell.environment = Some(PluginEnvironment::new(Arc::new(
            AgentEnvironment::load(directory.path()).await.unwrap(),
        )));
        let context = ProtocolContext {
            tasks: TaskManager::new(),
        };
        let line_count = 10_000;
        let script =
            format!("1..{line_count} | ForEach-Object {{ Write-Output \"line-$($_):中文-✓\" }}");

        shell
            .exec(
                ProtocolRequest {
                    uri: "pwsh://run?background=true",
                    target: "run?background=true",
                    body: &script,
                },
                context.clone(),
            )
            .await
            .unwrap();
        let record = context
            .tasks
            .wait("001", Duration::from_secs(30))
            .await
            .unwrap();
        assert_eq!(record.status, TaskStatus::Completed);
        let detail = TasksProtocol
            .read(
                ProtocolRequest {
                    uri: "tasks://001",
                    target: "001",
                    body: "",
                },
                context.clone(),
            )
            .await
            .unwrap();
        let detail = String::from_utf8(detail).unwrap();
        let output = detail
            .lines()
            .filter(|line| line.starts_with("line-"))
            .collect::<Vec<_>>();

        assert!(detail.len() > 64 * 1024);
        assert_eq!(output.len(), line_count);
        assert_eq!(output.first(), Some(&"line-1:中文-✓"));
        assert_eq!(output.last(), Some(&"line-10000:中文-✓"));
        context.tasks.shutdown().await;
    }

    #[tokio::test]
    async fn shell_returns_short_commands_and_backgrounds_long_or_explicit_commands() {
        let directory = tempfile::tempdir().unwrap();
        let (protocol, executable, short, delayed, explicit) = if cfg!(windows) {
            let Some(executable) = find_executable("pwsh") else {
                return;
            };
            (
                "pwsh",
                executable,
                "Write-Output foreground-ok",
                "Start-Sleep -Milliseconds 200; Write-Output automatic-ok",
                "Start-Sleep -Milliseconds 200; Write-Output explicit-ok",
            )
        } else {
            let Some(executable) = find_executable("bash") else {
                return;
            };
            (
                "bash",
                executable,
                "printf foreground-ok",
                "sleep 0.2; printf automatic-ok",
                "sleep 0.2; printf explicit-ok",
            )
        };
        let mut shell = ShellProtocol::new(protocol, executable, directory.path());
        shell.environment = Some(PluginEnvironment::new(Arc::new(
            AgentEnvironment::load(directory.path()).await.unwrap(),
        )));
        let context = ProtocolContext {
            tasks: crate::task::TaskManager::new(),
        };
        let run_uri = format!("{protocol}://run");
        let completed = shell
            .exec_with_auto_background(
                ProtocolRequest {
                    uri: &run_uri,
                    target: "run",
                    body: short,
                },
                context.clone(),
                Duration::from_secs(10),
            )
            .await
            .unwrap();
        let completed = String::from_utf8(completed).unwrap();
        assert!(completed.contains("foreground-ok"));
        assert!(!completed.contains("Exit:"));
        assert!(context.tasks.list().await.is_empty());

        let started = Instant::now();
        let accepted = shell
            .exec_with_auto_background(
                ProtocolRequest {
                    uri: &run_uri,
                    target: "run",
                    body: delayed,
                },
                context.clone(),
                Duration::from_millis(20),
            )
            .await
            .unwrap();
        assert!(started.elapsed() < Duration::from_millis(150));
        let accepted = String::from_utf8(accepted).unwrap();
        assert!(accepted.contains("Background task started: tasks://002"));
        assert!(accepted.contains("then use one bounded wait. Do not poll or rerun"));

        let background_uri = format!("{protocol}://run?background=true");
        let started = Instant::now();
        let accepted = shell
            .exec(
                ProtocolRequest {
                    uri: &background_uri,
                    target: "run?background=true",
                    body: explicit,
                },
                context.clone(),
            )
            .await
            .unwrap();
        assert!(started.elapsed() < Duration::from_millis(150));
        assert!(String::from_utf8(accepted).unwrap().contains("tasks://003"));

        let automatic = context
            .tasks
            .wait("002", Duration::from_secs(10))
            .await
            .unwrap();
        assert_eq!(automatic.status, TaskStatus::Completed);
        assert!(
            String::from_utf8(automatic.content)
                .unwrap()
                .contains("automatic-ok")
        );
        context.tasks.cancel("003").await;
    }

    #[tokio::test]
    async fn shell_execution_injects_managed_values_over_inherited_values() {
        let directory = tempfile::tempdir().unwrap();
        let name = format!("URI_AGENT_SHELL_ENV_TEST_{}", uuid::Uuid::now_v7().simple());
        let (protocol, executable, script) = if cfg!(windows) {
            let Some(executable) = find_executable("pwsh") else {
                return;
            };
            ("pwsh", executable, format!("Write-Output $env:{name}"))
        } else {
            let Some(executable) = find_executable("bash") else {
                return;
            };
            ("bash", executable, format!("printf '%s' \"${name}\""))
        };
        // SAFETY: the process-unique variable is removed before this test returns.
        unsafe { std::env::set_var(&name, "inherited") };
        let environment = Arc::new(AgentEnvironment::load(directory.path()).await.unwrap());
        environment.set(&name, "managed".to_string()).await.unwrap();
        let values = PluginEnvironment::new(environment).snapshot().await;

        let output = execute(
            protocol,
            &executable,
            directory.path(),
            &script,
            &values,
            None,
            None,
        )
        .await
        .unwrap();
        // SAFETY: the process-unique variable is no longer used.
        unsafe { std::env::remove_var(&name) };
        let output = String::from_utf8(output).unwrap();
        assert_eq!(output.trim_end(), "managed");
        assert!(!output.contains("inherited"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shell_completion_terminates_quiet_descendants_after_parent_exit() {
        let directory = tempfile::tempdir().unwrap();
        let leaked_path = directory.path().join("leaked");
        let executable = find_executable("bash").unwrap();
        let started = Instant::now();
        let command = format!(
            "printf finished; (sleep 0.2; printf leaked > '{}') &",
            leaked_path.display()
        );
        let output = execute(
            "bash",
            &executable,
            directory.path(),
            &command,
            &BTreeMap::new(),
            None,
            None,
        )
        .await
        .unwrap();
        time::sleep(Duration::from_millis(300)).await;

        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(String::from_utf8(output).unwrap().contains("finished"));
        assert!(!leaked_path.exists());
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn shell_completion_terminates_quiet_windows_descendants_after_parent_exit() {
        let directory = tempfile::tempdir().unwrap();
        let leaked_path = directory.path().join("leaked");
        let Some(executable) = find_executable("pwsh") else {
            return;
        };
        let started = Instant::now();
        let leak_script = BASE64.encode(format!(
            "Start-Sleep -Seconds 3; Set-Content -LiteralPath '{}' -Value leaked",
            leaked_path.display()
        ));
        let command = format!(
            "Write-Output finished; Start-Process -WindowStyle Hidden pwsh -ArgumentList '-NoProfile', '-EncodedCommand', '{leak_script}' | Out-Null"
        );
        let output = execute(
            "pwsh",
            &executable,
            directory.path(),
            &command,
            &BTreeMap::new(),
            None,
            None,
        )
        .await
        .unwrap();
        time::sleep(Duration::from_secs(4)).await;

        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(String::from_utf8(output).unwrap().contains("finished"));
        assert!(!leaked_path.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shell_completion_keeps_tail_output_active_after_parent_exit() {
        let directory = tempfile::tempdir().unwrap();
        let executable = find_executable("bash").unwrap();
        let output = execute(
            "bash",
            &executable,
            directory.path(),
            "parent=$$; (while kill -0 \"$parent\" 2>/dev/null; do :; done; printf 'tail1\\ntail2\\ntail3\\ntail4\\n') &",
            &BTreeMap::new(),
            None,
            None,
        )
        .await
        .unwrap();

        assert!(String::from_utf8(output).unwrap().contains("tail4"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shell_completion_bounds_continuous_descendant_output_after_parent_exit() {
        let directory = tempfile::tempdir().unwrap();
        let executable = find_executable("bash").unwrap();
        let output = time::timeout(
            Duration::from_secs(1),
            execute(
                "bash",
                &executable,
                directory.path(),
                "(while :; do printf x; sleep 0.05; done) &",
                &BTreeMap::new(),
                None,
                None,
            ),
        )
        .await
        .expect("shell cleanup must not wait indefinitely for descendant output")
        .unwrap();

        assert!(!output.is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shell_timeout_preserves_observed_output_and_terminates_the_process_tree() {
        let directory = tempfile::tempdir().unwrap();
        let release_path = directory.path().join("release");
        let leaked_path = directory.path().join("leaked");
        let command = format!(
            "printf partial; (while [ ! -e '{}' ]; do :; done; printf leaked > '{}') & wait",
            release_path.display(),
            leaked_path.display()
        );
        let executable = find_executable("bash").unwrap();

        let error = execute(
            "bash",
            &executable,
            directory.path(),
            &command,
            &BTreeMap::new(),
            Some(Duration::from_millis(50)),
            None,
        )
        .await
        .unwrap_err()
        .to_string();
        tokio::fs::write(&release_path, b"release").await.unwrap();
        time::sleep(Duration::from_millis(500)).await;

        assert!(error.contains("Command timed out"));
        assert!(error.ends_with("\n\npartial"));
        assert!(!leaked_path.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_a_foreground_shell_call_cancels_its_managed_process() {
        let directory = tempfile::tempdir().unwrap();
        let started_path = directory.path().join("started");
        let release_path = directory.path().join("release");
        let leaked_path = directory.path().join("leaked");
        let command = format!(
            "printf started > '{}'; (while [ ! -e '{}' ]; do :; done; printf leaked > '{}') & wait",
            started_path.display(),
            release_path.display(),
            leaked_path.display()
        );
        let executable = find_executable("bash").unwrap();
        let mut shell = ShellProtocol::new("bash", executable, directory.path());
        shell.environment = Some(PluginEnvironment::new(Arc::new(
            AgentEnvironment::load(directory.path()).await.unwrap(),
        )));
        let context = ProtocolContext {
            tasks: TaskManager::new(),
        };

        {
            let execution = shell.exec_with_auto_background(
                ProtocolRequest {
                    uri: "bash://run",
                    target: "run",
                    body: &command,
                },
                context.clone(),
                Duration::from_secs(60),
            );
            tokio::pin!(execution);
            for _ in 0..100 {
                if started_path.exists() {
                    break;
                }
                tokio::select! {
                    result = &mut execution => panic!("command unexpectedly settled: {result:?}"),
                    _ = time::sleep(Duration::from_millis(10)) => {}
                }
            }
            assert!(started_path.exists());
        }
        for _ in 0..100 {
            if context.tasks.get("001").await.is_none() {
                break;
            }
            time::sleep(Duration::from_millis(10)).await;
        }
        tokio::fs::write(&release_path, b"release").await.unwrap();
        time::sleep(Duration::from_millis(500)).await;

        assert!(context.tasks.get("001").await.is_none());
        assert!(!leaked_path.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_shell_execution_terminates_its_process_tree() {
        let directory = tempfile::tempdir().unwrap();
        let started_path = directory.path().join("started");
        let release_path = directory.path().join("release");
        let leaked_path = directory.path().join("leaked");
        let command = format!(
            "printf started > '{}'; (while [ ! -e '{}' ]; do :; done; printf leaked > '{}') & wait",
            started_path.display(),
            release_path.display(),
            leaked_path.display()
        );
        let executable = find_executable("bash").unwrap();
        let cwd = directory.path().to_path_buf();
        let execution = tokio::spawn(async move {
            execute(
                "bash",
                &executable,
                &cwd,
                &command,
                &BTreeMap::new(),
                None,
                None,
            )
            .await
        });

        for _ in 0..50 {
            if started_path.exists() {
                break;
            }
            time::sleep(Duration::from_millis(10)).await;
        }
        assert!(started_path.exists());
        execution.abort();
        let _ = execution.await;
        tokio::fs::write(&release_path, b"release").await.unwrap();
        time::sleep(Duration::from_millis(500)).await;

        assert!(!leaked_path.exists());
    }
}
