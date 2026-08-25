use crate::plugin::{Plugin, PluginEnvironment, PluginHost, PluginPermission, PluginRegistry};
use crate::prompts;
use crate::protocol::{Protocol, ProtocolContext, ProtocolDescriptor, ProtocolRequest};
use crate::task::{PromoteBackground, TaskManager, TaskRecord, TaskStatus};
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

const PWSH_SOURCE_BOOTSTRAP: &str = "$__uri_agent_source = [System.Text.Encoding]::UTF8.GetString([System.Convert]::FromBase64String([Console]::In.ReadToEnd())); & ([ScriptBlock]::Create($__uri_agent_source))";
const PWSH_UTF8_PREFIX: &str = "$OutputEncoding = [Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false); if ($null -ne $PSStyle) { $PSStyle.OutputRendering = 'PlainText' }; ";
const PWSH_EXIT_EPILOGUE: &str = "\n; $__uri_agent_ok = $?; $__uri_agent_native = $global:LASTEXITCODE; if ($__uri_agent_ok) { $global:__uri_agent_exit_code = 0 } elseif ($null -ne $__uri_agent_native -and $__uri_agent_native -ne 0) { $global:__uri_agent_exit_code = $__uri_agent_native } else { $global:__uri_agent_exit_code = 1 }";
const PWSH_WINDOWS_WARNING: &str =
    "PowerShell 7 or newer was not found on Windows; pwsh:// is disabled.";
const EXIT_OUTPUT_IDLE_GRACE: Duration = Duration::from_millis(100);
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

You MUST NOT add another background layer inside the command. Background task
status, output, and cancellation use the unified `tasks://` protocol.
Completion is delivered automatically, so you MUST NOT poll.

User-managed Agent environment variables are injected into every command. Use
secret values by name and do not print them unless the user explicitly asks.
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

You MUST NOT add another background layer inside the command. Background task
status, output, and cancellation use the unified `tasks://` protocol.
Completion is delivered automatically, so you MUST NOT poll.

PowerShell source and plain-text output use UTF-8. Command success follows the
final PowerShell or native command, and native exit codes are preserved.

User-managed Agent environment variables are injected into every command. Use
secret values by name and do not print them unless the user explicitly asks.
"#;

struct ProcessTreeGuard {
    pid: u32,
    armed: bool,
}

struct ForegroundTaskGuard {
    tasks: TaskManager,
    id: String,
    cancellation: CancellationToken,
    armed: bool,
}

impl ForegroundTaskGuard {
    fn new(tasks: TaskManager, id: String, cancellation: CancellationToken) -> Self {
        Self {
            tasks,
            id,
            cancellation,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ShellOptions {
    background: bool,
    timeout: Option<Duration>,
}

impl ProcessTreeGuard {
    fn new(pid: u32) -> Self {
        Self { pid, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ProcessTreeGuard {
    fn drop(&mut self) {
        if self.armed {
            terminate_process_tree(self.pid);
        }
    }
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
        let mut foreground = (!options.background).then(|| {
            ForegroundTaskGuard::new(tasks.clone(), id.clone(), record.cancellation.clone())
        });
        let progress_tasks = tasks.clone();
        let progress_id = id.clone();
        tasks
            .spawn(record, async move {
                let environment = environment.snapshot().await;
                execute(
                    &protocol,
                    &executable,
                    &cwd,
                    &command,
                    &environment,
                    options.timeout,
                    Some((&progress_tasks, &progress_id)),
                )
                .await
            })
            .await;
        if options.background {
            return Ok(prompts::task_accepted(&id).into_bytes());
        }
        let record = context
            .tasks
            .wait(&id, auto_background_after)
            .await
            .ok_or_else(|| anyhow!("task disappeared: {id}"))?;
        if record.status.terminal() {
            let result = finish_foreground(&context.tasks, record).await;
            foreground
                .as_mut()
                .expect("foreground commands have a cancellation guard")
                .disarm();
            return result;
        }
        match context.tasks.promote_background(&id).await {
            PromoteBackground::Promoted => {
                foreground
                    .as_mut()
                    .expect("foreground commands have a cancellation guard")
                    .disarm();
                Ok(prompts::task_accepted(&id).into_bytes())
            }
            PromoteBackground::Terminal(record) => {
                let result = finish_foreground(&context.tasks, record).await;
                foreground
                    .as_mut()
                    .expect("foreground commands have a cancellation guard")
                    .disarm();
                result
            }
            PromoteBackground::AtCapacity => {
                let record = context
                    .tasks
                    .wait_until_terminal(&id)
                    .await
                    .ok_or_else(|| anyhow!("task disappeared: {id}"))?;
                let result = finish_foreground(&context.tasks, record).await;
                foreground
                    .as_mut()
                    .expect("foreground commands have a cancellation guard")
                    .disarm();
                result
            }
        }
    }
}

async fn finish_foreground(tasks: &TaskManager, record: TaskRecord) -> Result<Vec<u8>> {
    tasks.remove(&record.id).await;
    match record.status {
        TaskStatus::Completed => Ok(record.content),
        TaskStatus::Failed => Err(anyhow!(
            String::from_utf8_lossy(&record.content).into_owned()
        )),
        TaskStatus::Cancelled => bail!("shell command was cancelled"),
        TaskStatus::Pending | TaskStatus::Running => {
            bail!("shell command did not reach a terminal state")
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

fn encode_pwsh_script(script: &str) -> String {
    let source = format!(
        "{PWSH_UTF8_PREFIX}$global:LASTEXITCODE = $null; $global:__uri_agent_exit_code = 0; . {{\n{script}{PWSH_EXIT_EPILOGUE}\n}} | Out-Default\nexit $global:__uri_agent_exit_code"
    );
    BASE64.encode(source)
}

async fn execute(
    protocol: &str,
    executable: &Path,
    cwd: &Path,
    script: &str,
    environment: &BTreeMap<String, String>,
    timeout: Option<Duration>,
    progress: Option<(&TaskManager, &str)>,
) -> Result<Vec<u8>> {
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
        script.to_string()
    } else {
        command.args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            PWSH_SOURCE_BOOTSTRAP,
        ]);
        encode_pwsh_script(script)
    };
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.as_std_mut().process_group(0);
    }
    let mut child = command
        .envs(environment)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let mut process_tree = ProcessTreeGuard::new(
        child
            .id()
            .ok_or_else(|| anyhow!("failed to get shell process ID"))?,
    );
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("failed to open shell stdin"))?;
    if let Some(deadline) = deadline {
        match time::timeout_at(deadline, stdin.write_all(input.as_bytes())).await {
            Ok(result) => result?,
            Err(_) => {
                bail!(
                    "Command timed out after {} seconds.",
                    timeout
                        .expect("a deadline is only present when a timeout is configured")
                        .as_secs()
                )
            }
        }
    } else {
        stdin.write_all(input.as_bytes()).await?;
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
    let output_idle = time::sleep(Duration::from_secs(365 * 24 * 60 * 60));
    let deadline = time::sleep_until(
        deadline.unwrap_or_else(|| Instant::now() + Duration::from_secs(365 * 24 * 60 * 60)),
    );
    tokio::pin!(output_idle);
    tokio::pin!(deadline);

    loop {
        if status.is_some() && stdout.is_none() && stderr.is_none() {
            break;
        }
        tokio::select! {
            _ = &mut deadline, if timeout.is_some() => {
                timed_out = true;
                break;
            }
            result = child.wait(), if status.is_none() => {
                status = Some(result?);
                output_idle.as_mut().reset(Instant::now() + EXIT_OUTPUT_IDLE_GRACE);
            }
            result = async {
                stdout
                    .as_mut()
                    .expect("stdout read is guarded")
                    .read(&mut stdout_buffer)
                    .await
            }, if stdout.is_some() => {
                let count = result?;
                if count == 0 {
                    stdout = None;
                } else {
                    stdout_content.extend_from_slice(&stdout_buffer[..count]);
                    if let Some((tasks, id)) = progress {
                        tasks.append_latest_output(id, &stdout_buffer[..count]).await;
                    }
                    if status.is_some() {
                        output_idle.as_mut().reset(Instant::now() + EXIT_OUTPUT_IDLE_GRACE);
                    }
                }
            }
            result = async {
                stderr
                    .as_mut()
                    .expect("stderr read is guarded")
                    .read(&mut stderr_buffer)
                    .await
            }, if stderr.is_some() => {
                let count = result?;
                if count == 0 {
                    stderr = None;
                } else {
                    stderr_content.extend_from_slice(&stderr_buffer[..count]);
                    if let Some((tasks, id)) = progress {
                        tasks.append_latest_output(id, &stderr_buffer[..count]).await;
                    }
                    if status.is_some() {
                        output_idle.as_mut().reset(Instant::now() + EXIT_OUTPUT_IDLE_GRACE);
                    }
                }
            }
            _ = &mut output_idle, if status.is_some() => break,
        }
    }

    if timed_out {
        let mut result = format!(
            "Command timed out after {} seconds.\n",
            timeout
                .expect("the timeout branch is only enabled when configured")
                .as_secs()
        )
        .into_bytes();
        append_stream_output(&mut result, &stdout_content, &stderr_content);
        return Err(anyhow!(String::from_utf8_lossy(&result).into_owned()));
    }
    let status = status.ok_or_else(|| anyhow!("shell process exited without a status"))?;
    process_tree.disarm();
    let mut result = format!("Exit: {status}\n").into_bytes();
    append_stream_output(&mut result, &stdout_content, &stderr_content);
    if status.success() {
        Ok(result)
    } else {
        Err(anyhow!(String::from_utf8_lossy(&result).into_owned()))
    }
}

fn append_stream_output(result: &mut Vec<u8>, stdout: &[u8], stderr: &[u8]) {
    if !stdout.is_empty() {
        result.extend_from_slice(b"\nstdout:\n");
        result.extend_from_slice(stdout);
    }
    if !stderr.is_empty() {
        result.extend_from_slice(b"\nstderr:\n");
        result.extend_from_slice(stderr);
    }
}

#[cfg(unix)]
fn terminate_process_tree(pid: u32) {
    let Ok(pid) = i32::try_from(pid) else {
        return;
    };
    // SAFETY: kill only reads the process-group ID, and a negative ID targets
    // the group created for this shell immediately before it was spawned.
    let _ = unsafe { libc::kill(-pid, libc::SIGKILL) };
}

#[cfg(windows)]
fn terminate_process_tree(pid: u32) {
    let _ = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(not(any(unix, windows)))]
fn terminate_process_tree(_pid: u32) {}

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
    use crate::config::AgentEnvironment;
    use base64::engine::general_purpose::STANDARD as BASE64;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

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
        assert!(PWSH_HELP.contains("unified `tasks://` protocol"));
        assert!(PWSH_HELP.contains("MUST NOT poll"));
        assert!(PWSH_HELP.contains("Agent environment variables are injected"));
        assert!(BASH_HELP.contains("`bash://help` and MUST use an empty string body"));
        assert!(BASH_HELP.contains("command body MUST contain at least\none non-whitespace"));
        assert!(BASH_HELP.contains("MUST NOT add another background layer"));
        assert!(BASH_HELP.contains("`background=true`"));
        assert!(BASH_HELP.contains("`timeout=0` disables the timeout"));
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
    fn pwsh_source_transport_is_utf8_and_preserves_final_status() {
        let encoded = encode_pwsh_script("Write-Output '中文 ✓' # trailing comment");
        let decoded = String::from_utf8(BASE64.decode(encoded).unwrap()).unwrap();

        assert!(decoded.starts_with(PWSH_UTF8_PREFIX));
        assert!(decoded.contains("Write-Output '中文 ✓' # trailing comment\n; "));
        assert!(decoded.contains("$__uri_agent_native = $global:LASTEXITCODE"));
        assert!(decoded.contains("} | Out-Default\nexit $global:__uri_agent_exit_code"));
        assert!(PWSH_SOURCE_BOOTSTRAP.is_ascii());
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
        assert!(error.contains("Exit:"));
        #[cfg(unix)]
        assert!(error.contains("Exit: exit status: 7"));
        #[cfg(windows)]
        assert!(error.contains("Exit: exit code: 7"));
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
        assert!(completed.contains("Exit:"));
        assert!(completed.contains("foreground-ok"));
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
        assert!(accepted.contains("Background task accepted: tasks://002"));
        assert!(accepted.contains("current status or output is explicitly needed"));

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
        assert!(output.contains("stdout:\nmanaged"));
        assert!(!output.contains("inherited"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shell_completion_does_not_wait_for_quiet_inherited_output_handles() {
        let directory = tempfile::tempdir().unwrap();
        let executable = find_executable("bash").unwrap();
        let started = Instant::now();
        let output = execute(
            "bash",
            &executable,
            directory.path(),
            "printf finished; sleep 2 &",
            &BTreeMap::new(),
            None,
            None,
        )
        .await
        .unwrap();

        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(String::from_utf8(output).unwrap().contains("finished"));
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
            "(for value in 1 2 3 4; do sleep 0.05; printf 'tail%s\\n' \"$value\"; done) &",
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
    async fn shell_timeout_preserves_observed_output_and_terminates_the_process_tree() {
        let directory = tempfile::tempdir().unwrap();
        let leaked_path = directory.path().join("leaked");
        let command = format!(
            "printf partial; (sleep 0.3; printf leaked > '{}') & wait",
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
        time::sleep(Duration::from_millis(500)).await;

        assert!(error.contains("Command timed out"));
        assert!(error.contains("stdout:\npartial"));
        assert!(!leaked_path.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_a_foreground_shell_call_cancels_its_managed_process() {
        let directory = tempfile::tempdir().unwrap();
        let started_path = directory.path().join("started");
        let leaked_path = directory.path().join("leaked");
        let command = format!(
            "printf started > '{}'; (sleep 0.3; printf leaked > '{}') & wait",
            started_path.display(),
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
        time::sleep(Duration::from_millis(500)).await;

        assert!(context.tasks.get("001").await.is_none());
        assert!(!leaked_path.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_shell_execution_terminates_its_process_tree() {
        let directory = tempfile::tempdir().unwrap();
        let started_path = directory.path().join("started");
        let leaked_path = directory.path().join("leaked");
        let command = format!(
            "printf started > '{}'; (sleep 0.3; printf leaked > '{}') & wait",
            started_path.display(),
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
        time::sleep(Duration::from_millis(500)).await;

        assert!(!leaked_path.exists());
    }
}
