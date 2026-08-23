use super::{render_record, render_task, render_task_list};
use crate::plugin::{Plugin, PluginHost, PluginRegistry};
use crate::prompts;
use crate::protocol::{Protocol, ProtocolContext, ProtocolDescriptor, ProtocolRequest};
use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::time::{self, Instant};

const PWSH_SOURCE_BOOTSTRAP: &str = "$__uri_agent_source = [System.Text.Encoding]::UTF8.GetString([System.Convert]::FromBase64String([Console]::In.ReadToEnd())); & ([ScriptBlock]::Create($__uri_agent_source))";
const PWSH_UTF8_PREFIX: &str = "$OutputEncoding = [Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false); if ($null -ne $PSStyle) { $PSStyle.OutputRendering = 'PlainText' }; ";
const PWSH_EXIT_EPILOGUE: &str = "\n; $__uri_agent_ok = $?; $__uri_agent_native = $global:LASTEXITCODE; if ($__uri_agent_ok) { $global:__uri_agent_exit_code = 0 } elseif ($null -ne $__uri_agent_native -and $__uri_agent_native -ne 0) { $global:__uri_agent_exit_code = $__uri_agent_native } else { $global:__uri_agent_exit_code = 1 }";
const PWSH_WINDOWS_WARNING: &str =
    "PowerShell 7 or newer was not found on Windows; pwsh:// is disabled.";
const EXIT_OUTPUT_IDLE_GRACE: Duration = Duration::from_millis(100);
const BASH_HELP: &str = r#"# bash

Run Bash commands as managed asynchronous tasks.

Call `exec` with `bash://run` and pass the command string directly as the body:

```text
exec("bash://run", "cargo test")
```

Add `?wait=N` to wait up to N seconds (maximum 300), for example
`bash://?wait=30`. If the wait window expires, the command keeps running and
the result contains its task URI.

Read `bash://tasks/<id>` for status and bounded output. If that output exceeds
the system limit, the result includes a `file://` address containing the full
output.
"#;
const PWSH_HELP: &str = r#"# pwsh

Run PowerShell 7 commands as managed asynchronous tasks.

Write PowerShell 7 syntax rather than Unix shell syntax. Use multiline commands
with normal indentation when they improve readability; do not collapse them
into one line. Single quotes are literal, double quotes expand variables, and
the backtick is the escape character. Set environment variables with
`$env:NAME = 'value'` and quote paths containing spaces.

Prefer modern cross-platform tools such as `rg` and `fd` when available.
PowerShell recursive searches do not honor `.gitignore`, so bound search paths,
depth, and output tightly.

Call `exec` with `pwsh://run` and pass the command string directly as the body:

```text
exec("pwsh://run", "Get-ChildItem -Path . -Force")
```

Commands already run as managed tasks. Do not create another background layer
inside the command. Use the returned task URI to inspect the task later.

Add `?wait=N` to wait up to N seconds (maximum 300), for example
`pwsh://?wait=30`. If the wait window expires, the command keeps running and
the result contains its task URI.

PowerShell source and plain-text output use UTF-8. Task success follows the
final PowerShell or native command, and native exit codes are preserved.

Read `pwsh://tasks/<id>` for status and bounded output. If that output exceeds
the system limit, the result includes a `file://` address containing the full
output.
"#;

struct ProcessTreeGuard {
    pid: u32,
    armed: bool,
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
}

impl ShellProtocol {
    fn new(name: &'static str, executable: PathBuf, cwd: &Path) -> Self {
        Self {
            name,
            executable,
            cwd: cwd.to_path_buf(),
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

    fn register(&self, host: &mut PluginHost<'_>) -> Result<()> {
        if let Some(protocol) = &self.protocol {
            host.protocols.register(protocol.clone())?;
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

    fn register(&self, host: &mut PluginHost<'_>) -> Result<()> {
        host.protocols.register(self.clone())
    }
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
                BASH_HELP
            } else {
                PWSH_HELP
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
        let (target, wait) = parse_target(request.target)?;
        if !matches!(target, "" | "run") {
            bail!(
                "expected {}://run or {}://?wait=<seconds>",
                self.name,
                self.name
            );
        }
        let command = command_from_body(request.body)?.to_string();
        let executable = self.executable.clone();
        let cwd = self.cwd.clone();
        let protocol = self.name.to_string();
        let record = context
            .tasks
            .allocate(self.name, command_label(&command))
            .await;
        let id = record.id.clone();
        let tasks = context.tasks.clone();
        tasks
            .spawn(record, async move {
                execute(&protocol, &executable, &cwd, &command).await
            })
            .await;
        let Some(wait) = wait else {
            return Ok(prompts::task_accepted(self.name, &id).into_bytes());
        };
        let record = context
            .tasks
            .wait(&id, wait)
            .await
            .ok_or_else(|| anyhow!("task disappeared: {id}"))?;
        if record.status.terminal() {
            return Ok(render_record(&record).into_bytes());
        }
        Ok(format!(
            "{}\nWait window elapsed; the task is still {}.",
            prompts::task_accepted(self.name, &id),
            record.status.as_str()
        )
        .into_bytes())
    }
}

fn parse_target(target: &str) -> Result<(&str, Option<Duration>)> {
    let Some((target, query)) = target.rsplit_once('?') else {
        return Ok((target, None));
    };
    let seconds = query
        .strip_prefix("wait=")
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or_else(|| anyhow!("shell wait must be an integer number of seconds"))?;
    if seconds > 300 {
        bail!("shell wait cannot exceed 300 seconds");
    }
    Ok((target, Some(Duration::from_secs(seconds))))
}

fn command_from_body(body: Option<&Value>) -> Result<&str> {
    match body {
        Some(Value::String(command)) => Ok(command),
        _ => bail!("shell body must be a command string"),
    }
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

async fn execute(protocol: &str, executable: &Path, cwd: &Path, script: &str) -> Result<Vec<u8>> {
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
    stdin.write_all(input.as_bytes()).await?;
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
    let output_idle = time::sleep(Duration::from_secs(365 * 24 * 60 * 60));
    tokio::pin!(output_idle);

    loop {
        if status.is_some() && stdout.is_none() && stderr.is_none() {
            break;
        }
        tokio::select! {
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
                    if status.is_some() {
                        output_idle.as_mut().reset(Instant::now() + EXIT_OUTPUT_IDLE_GRACE);
                    }
                }
            }
            _ = &mut output_idle, if status.is_some() => break,
        }
    }

    let status = status.ok_or_else(|| anyhow!("shell process exited without a status"))?;
    process_tree.disarm();
    let mut result = format!("Exit: {status}\n").into_bytes();
    if !stdout_content.is_empty() {
        result.extend_from_slice(b"\nstdout:\n");
        result.extend_from_slice(&stdout_content);
    }
    if !stderr_content.is_empty() {
        result.extend_from_slice(b"\nstderr:\n");
        result.extend_from_slice(&stderr_content);
    }
    if status.success() {
        Ok(result)
    } else {
        Err(anyhow!(String::from_utf8_lossy(&result).into_owned()))
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
    use base64::engine::general_purpose::STANDARD as BASE64;
    use std::time::{Duration, Instant};

    #[test]
    fn pwsh_help_uses_powershell_syntax_and_bounds_shell_work() {
        assert!(PWSH_HELP.contains("PowerShell 7 syntax rather than Unix shell syntax"));
        assert!(PWSH_HELP.contains("`$env:NAME = 'value'`"));
        assert!(PWSH_HELP.contains("do not honor `.gitignore`"));
        assert!(PWSH_HELP.contains("Do not create another background layer"));
        assert!(PWSH_HELP.contains("`pwsh://?wait=30`"));
    }

    #[test]
    fn shell_plugin_parses_its_own_wait_option() {
        assert_eq!(parse_target("run").unwrap(), ("run", None));
        assert_eq!(
            parse_target("?wait=30").unwrap(),
            ("", Some(Duration::from_secs(30)))
        );
        assert!(parse_target("run?wait=not-a-number").is_err());
        assert!(parse_target("run?wait=301").is_err());
        assert!(parse_target("run?other=30").is_err());
    }

    #[test]
    fn shell_body_only_accepts_a_command_string() {
        let string = Value::String("cargo test".to_string());
        let object = serde_json::json!({"command": "cargo test"});
        assert_eq!(command_from_body(Some(&string)).unwrap(), "cargo test");
        assert!(command_from_body(Some(&object)).is_err());
        assert!(command_from_body(None).is_err());
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
        let output = execute("pwsh", &executable, directory.path(), &script)
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
        let error = execute("pwsh", &executable, directory.path(), native_failure)
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
    async fn shell_is_async_by_default_and_can_opt_into_a_bounded_wait() {
        let directory = tempfile::tempdir().unwrap();
        let Some(executable) = find_executable("bash") else {
            return;
        };
        let shell = ShellProtocol::new("bash", executable, directory.path());
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
        )
        .await
        .unwrap();

        assert!(String::from_utf8(output).unwrap().contains("tail4"));
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
        let execution =
            tokio::spawn(async move { execute("bash", &executable, &cwd, &command).await });

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
