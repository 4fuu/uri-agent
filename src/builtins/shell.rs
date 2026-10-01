use crate::plugin::{Plugin, PluginEnvironment, PluginHost, PluginPermission, PluginRegistry};
use crate::process::{PWSH_STDIN_BOOTSTRAP, ProcessTree};
use crate::prompts;
use crate::protocol::{
    Protocol, ProtocolContext, ProtocolDescriptor, ProtocolOutput, ProtocolRequest,
};
use crate::task::{AutoTask, TaskControls, TaskInput, TaskManager};
use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
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
final result in the same `protocol` tool call.

Read steps support no shell operations. Run a command with one exec step:

{"exec": "bash://run", "input": {"script": "cargo test"}}

`bash://run` accepts these `input` fields; unknown fields are rejected:

- `script` (string, required): the Bash script to run. It must contain at
  least one non-whitespace character.
- `background` (boolean, default false): return a task handle immediately
  instead of waiting for the command.
- `interactive` (boolean, default false): keep stdin open for runtime input
  such as confirmation prompts, passwords, and REPLs. Interactive commands are
  always background tasks; omit `background` or set it to true.
- `timeout` (integer seconds, default 1800): execution timeout shared by
  foreground and background runs; 0 disables the timeout.
- `env` (object of string values): extra environment variables for this
  command, overriding the Agent-managed environment. Names must be nonempty
  and contain no `=`; numbers and booleans, such as substituted references,
  are converted to text.

If a foreground command is still running after about 60 seconds, URI Agent
automatically converts the same process into a background task without
restarting it.

Examples:

{"exec": "bash://run", "input": {"script": "cargo test", "timeout": 120}}

{"exec": "bash://run", "input": {"script": "cargo test", "background": true}}

{"exec": "bash://run", "input": {"script": "mysql -u root -p", "interactive": true, "timeout": 0}}

Data never reaches a script through string interpolation. Pass values through
`env` and read them as variables:
`{"exec": "bash://run", "input": {"script": "deploy \"$TARGET\"", "env": {"TARGET": "production"}}}`.

Read current output with a `{"read": "tasks://<id>"}` step, send input with a
`{"exec": "tasks://<id>/send", "input": {"text": "yes\n"}}` step; `text` is
delivered to the process byte-for-byte. Close stdin with a
`{"exec": "tasks://<id>/eof"}` step and interrupt with a
`{"exec": "tasks://<id>/interrupt"}` step. The shared timeout keeps running
while the command waits for input; use `timeout: 0` for an open-ended
interactive command.

You MUST NOT add another background layer inside the command. Child processes
remain owned by this execution and are terminated when the root shell exits or
the task times out or is cancelled. Background task status, output, and
cancellation use the unified `tasks://` protocol; load its contract with
`help(["tasks"])` before the first such call. Completion is delivered
automatically; read a task only when you need its current output.

User-managed Agent environment variables are injected into every command. Use
secret values by name and do not print them unless the user explicitly asks.

On success, stdout-only output is returned directly, stderr-only output is
identified by `stderr:`, and both streams are labeled when both exist. A
successful command with no output returns `(no output)`. Failures retain the
exit code or timeout and any output observed before termination.
"#;
const PWSH_HELP: &str = r#"# pwsh

Run PowerShell 7 commands. Commands start in the foreground and normally return
their final result in the same `protocol` tool call.

Write PowerShell 7 syntax rather than Unix shell syntax. Use multiline commands
with normal indentation when they improve readability; do not collapse them
into one line. Single quotes are literal, double quotes expand variables, and
the backtick is the escape character. Set environment variables with
`$env:NAME = 'value'` and quote paths containing spaces.

Prefer modern cross-platform tools such as `rg` and `fd` when available.
PowerShell recursive searches do not honor `.gitignore`, so bound search paths,
depth, and output tightly.

Native commands such as `gh`, `git`, and `cargo` do not accept PowerShell
common parameters (`-OutVariable`, `-ErrorAction`, `-ErrorVariable`). Pass
their own flags only, and capture output with variables, `$LASTEXITCODE`,
or redirection.

Read steps support no shell operations. Run a command with one exec step:

{"exec": "pwsh://run", "input": {"script": "Get-ChildItem -Path . -Force"}}

`pwsh://run` accepts these `input` fields; unknown fields are rejected:

- `script` (string, required): the PowerShell script to run. It must contain
  at least one non-whitespace character.
- `background` (boolean, default false): return a task handle immediately
  instead of waiting for the command.
- `interactive` (boolean, default false): keep stdin open for runtime input
  such as confirmation prompts, passwords, and REPLs. Interactive commands are
  always background tasks; omit `background` or set it to true.
- `timeout` (integer seconds, default 1800): execution timeout shared by
  foreground and background runs; 0 disables the timeout.
- `env` (object of string values): extra environment variables for this
  command, overriding the Agent-managed environment. Names must be nonempty
  and contain no `=`; numbers and booleans, such as substituted references,
  are converted to text.

If a foreground command is still running after about 60 seconds, URI Agent
automatically converts the same process into a background task without
restarting it.

Examples:

{"exec": "pwsh://run", "input": {"script": "cargo test", "timeout": 120}}

{"exec": "pwsh://run", "input": {"script": "cargo test", "background": true}}

{"exec": "pwsh://run", "input": {"script": "$token = Read-Host 'Token'", "interactive": true, "timeout": 0}}

Data never reaches a script through string interpolation. Pass values through
`env` and read them as variables:
`{"exec": "pwsh://run", "input": {"script": "Deploy -Target $env:TARGET", "env": {"TARGET": "production"}}}`.

Read current output with a `{"read": "tasks://<id>"}` step, send input with a
`{"exec": "tasks://<id>/send", "input": {"text": "yes\n"}}` step; `text` is
delivered to the process byte-for-byte. Close stdin with a
`{"exec": "tasks://<id>/eof"}` step and interrupt with a
`{"exec": "tasks://<id>/interrupt"}` step. The shared timeout keeps running
while the command waits for input; use `timeout: 0` for an open-ended
interactive command.

You MUST NOT add another background layer inside the command. Child processes
remain owned by this execution and are terminated when the root shell exits or
the task times out or is cancelled. Background task status, output, and
cancellation use the unified `tasks://` protocol; load its contract with
`help(["tasks"])` before the first such call. Completion is delivered
automatically; read a task only when you need its current output.

PowerShell source and plain-text output use UTF-8. Command success follows the
final PowerShell or native command, and native exit codes are preserved.

User-managed Agent environment variables are injected into every command. Use
secret values by name and do not print them unless the user explicitly asks.

On success, stdout-only output is returned directly, stderr-only output is
identified by `stderr:`, and both streams are labeled when both exist. A
successful command with no output returns `(no output)`. Failures retain the
exit code or timeout and any output observed before termination.
"#;

#[derive(Clone, Debug, Eq, PartialEq)]
struct ShellOptions {
    background: bool,
    timeout: Option<Duration>,
    interactive: bool,
    env: BTreeMap<String, String>,
    script: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExecInput {
    script: String,
    background: Option<bool>,
    interactive: Option<bool>,
    timeout: Option<u64>,
    env: Option<BTreeMap<String, Value>>,
}

struct ExecutionControl<'a> {
    timeout: Option<Duration>,
    progress: Option<(&'a TaskManager, &'a str)>,
    cancellation: CancellationToken,
    input: Option<mpsc::Receiver<TaskInput>>,
    interrupt: Option<CancellationToken>,
}

/// Ends the interactive stdin writer on every exit path of an execution,
/// including error returns, so no writer outlives its process.
struct AbortWriter(Option<JoinHandle<()>>);

impl AbortWriter {
    fn none() -> Self {
        Self(None)
    }
}

impl Drop for AbortWriter {
    fn drop(&mut self) {
        if let Some(writer) = self.0.take() {
            writer.abort();
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

    fn literal_input_fields(&self) -> &[&str] {
        &["script"]
    }

    async fn read(
        &self,
        request: ProtocolRequest<'_>,
        _context: ProtocolContext,
    ) -> Result<ProtocolOutput> {
        if request.target != "help" {
            bail!(
                "{0} read supports no shell operations; use an {{\"exec\": \"{0}://run\", \
                 \"input\": {{\"script\": \"<script>\"}}}} step",
                self.name
            );
        }
        request.reject_input()?;
        Ok(if self.name == "bash" {
            BASH_HELP
        } else {
            PWSH_HELP
        }
        .into())
    }

    async fn exec(
        &self,
        request: ProtocolRequest<'_>,
        context: ProtocolContext,
    ) -> Result<ProtocolOutput> {
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
    ) -> Result<ProtocolOutput> {
        let options = if request.target == "run" {
            parse_options(self.name, &request)
        } else {
            Err(anyhow!("expected shell target run"))
        }
        .with_context(|| {
            format!(
                r#"invalid {0} exec; use an {{"exec": "{0}://run", "input": {{"script": "<script>"}}}} step"#,
                self.name
            )
        })?;
        let command = options.script;
        let executable = self.executable.clone();
        let cwd = self.cwd.clone();
        let protocol = self.name.to_string();
        let environment = self
            .environment
            .clone()
            .ok_or_else(|| anyhow!("shell environment is not attached"))?;
        let (controls, input, interrupt) = if options.interactive {
            let (controls, input, interrupt) = TaskControls::interactive();
            (Some(controls), Some(input), Some(interrupt))
        } else {
            (None, None, None)
        };
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
        // Controls attach before the worker starts so input and interrupt
        // calls cannot race process startup.
        if let Some(controls) = controls {
            tasks.set_controls(&id, controls).await;
        }
        let progress_tasks = tasks.clone();
        let progress_id = id.clone();
        let step_env = options.env;
        let work = move |cancellation| async move {
            let mut environment = environment.snapshot().await;
            environment.extend(step_env);
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
                    input,
                    interrupt,
                },
            )
            .await
        };
        if options.background {
            tasks.spawn_with_cancellation(record, work).await;
            return Ok(if options.interactive {
                prompts::interactive_task_accepted(&id).into()
            } else {
                prompts::task_accepted(&id).into()
            });
        }
        let auto_background_after = context.foreground_grace(auto_background_after);
        match tasks
            .run_with_auto_background(record, auto_background_after, work)
            .await?
        {
            AutoTask::Background(id) => Ok(prompts::task_accepted(&id).into()),
            AutoTask::Terminal(record) => Ok(record.terminal_result("shell command")?.into()),
        }
    }
}

fn parse_options(protocol: &str, request: &ProtocolRequest<'_>) -> Result<ShellOptions> {
    let input: ExecInput = request.input_struct()?;
    let background = input.background.unwrap_or(false);
    let interactive = input.interactive.unwrap_or(false);
    if input.background == Some(false) && input.interactive == Some(true) {
        bail!(
            "interactive input requires background execution; omit the background field or use `background: true`"
        );
    }
    if input.script.trim().is_empty() {
        bail!(
            "{protocol} script must contain a non-whitespace character; use an \
             {{\"exec\": \"{protocol}://run\", \"input\": {{\"script\": \"<script>\"}}}} step"
        );
    }
    let timeout = input.timeout.unwrap_or(DEFAULT_TIMEOUT.as_secs());
    Ok(ShellOptions {
        background: background || interactive,
        timeout: (timeout != 0).then(|| Duration::from_secs(timeout)),
        interactive,
        env: step_environment(input.env.unwrap_or_default())?,
        script: input.script,
    })
}

/// Validates `env` names and converts values to text. Numbers and booleans
/// are accepted because a whole-value `{{ reference }}` substitution keeps
/// the referenced JSON type.
fn step_environment(env: BTreeMap<String, Value>) -> Result<BTreeMap<String, String>> {
    env.into_iter()
        .map(|(name, value)| {
            if name.is_empty() || name.contains(['=', '\0']) {
                bail!("env names must be nonempty and contain neither `=` nor NUL; got {name:?}");
            }
            let value = match value {
                Value::String(text) => text,
                Value::Number(number) => number.to_string(),
                Value::Bool(flag) => flag.to_string(),
                _ => bail!("env value for {name:?} must be a string, number, or boolean"),
            };
            if value.contains('\0') {
                bail!("env value for {name:?} must not contain NUL");
            }
            Ok((name, value))
        })
        .collect()
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

fn pwsh_source(script: &str) -> String {
    format!(
        "{PWSH_UTF8_PREFIX}$global:LASTEXITCODE = $null; $global:__uri_agent_exit_code = 0; . {{\n{script}{PWSH_EXIT_EPILOGUE}\n}} | Out-Default\nexit $global:__uri_agent_exit_code"
    )
}

fn encode_pwsh_script(script: &str) -> String {
    BASE64.encode(pwsh_source(script))
}

/// Writes an interactive command's script to a private temporary file and
/// returns its path with delete-on-drop semantics. The file handle is closed
/// before returning: a held-open write handle blocks the Windows shell from
/// reading the script.
fn write_script_file(source: &str, extension: &str) -> Result<tempfile::TempPath> {
    use std::io::Write as _;
    let mut file = tempfile::Builder::new()
        .prefix("uri-agent-script-")
        .suffix(&format!(".{extension}"))
        .tempfile()
        .context("failed to create the script file for an interactive command")?;
    file.write_all(source.as_bytes())
        .and_then(|()| file.flush())
        .context("failed to write the script file for an interactive command")?;
    Ok(file.into_temp_path())
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
            input: None,
            interrupt: None,
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
        input,
        interrupt,
    } = control;
    let interactive = input.is_some();
    let deadline = timeout
        .map(|timeout| {
            Instant::now()
                .checked_add(timeout)
                .ok_or_else(|| anyhow!("shell timeout is too large"))
        })
        .transpose()?;
    let mut command = Command::new(executable);
    // An interactive command runs its script from a private temporary file
    // because stdin must stay reserved for the running program: a
    // stdin-delivered bash script would race program input for one pipe, and
    // the PowerShell bootstrap consumes stdin to EOF before running anything.
    // `_script_file` must outlive the spawned process; dropping it removes the
    // file.
    let (script_input, _script_file) = match (protocol, interactive) {
        ("bash", true) => {
            command.args(["--noprofile", "--norc"]);
            let file = write_script_file(&bash_script_input(script), "sh")?;
            command.arg(file.as_os_str());
            (None, Some(file))
        }
        ("bash", false) => {
            command.args(["--noprofile", "--norc"]);
            (Some(bash_script_input(script)), None)
        }
        (_, true) => {
            // PowerShell script files are subject to execution policies that
            // can block `pwsh -File`, so the script text is loaded from the
            // private file and run through a script block instead — the same
            // execution shape as the stdin bootstrap, with stdin free for the
            // program.
            command.args(["-NoLogo", "-NoProfile"]);
            let file = write_script_file(&pwsh_source(script), "ps1")?;
            let bootstrap = format!(
                "& ([ScriptBlock]::Create([System.IO.File]::ReadAllText('{}')))",
                file.to_string_lossy().replace('\'', "''")
            );
            command.arg("-Command").arg(&bootstrap);
            (None, Some(file))
        }
        _ => {
            command.args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                PWSH_STDIN_BOOTSTRAP,
            ]);
            (Some(encode_pwsh_script(script)), None)
        }
    };
    command
        .envs(environment)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let (mut child, process_tree) = if interactive {
        ProcessTree::spawn_interruptible(&mut command)
    } else {
        ProcessTree::spawn(&mut command)
    }?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("failed to open shell stdin"))?;
    // Interactive commands hand stdin to a dedicated writer so accepted input
    // reaches the running program while the read loop below observes output.
    // `_writer_guard` aborts the writer on every exit path of this function.
    let _writer_guard = if let Some(mut input) = input {
        let writer_cancellation = cancellation.clone();
        AbortWriter(Some(tokio::spawn(async move {
            loop {
                let message = tokio::select! {
                    biased;
                    _ = writer_cancellation.cancelled() => break,
                    message = input.recv() => message,
                };
                match message {
                    Some(TaskInput::Bytes(bytes)) => {
                        let written = tokio::select! {
                            biased;
                            _ = writer_cancellation.cancelled() => break,
                            result = stdin.write_all(&bytes) => result,
                        };
                        if written.is_err() {
                            break;
                        }
                    }
                    // An explicit close and a dropped channel both end stdin,
                    // which the program observes as end-of-file.
                    Some(TaskInput::Close) | None => break,
                }
            }
        })))
    } else {
        enum InputWrite {
            Complete(std::io::Result<()>),
            TimedOut,
            Cancelled,
        }
        let script = script_input.expect("non-interactive commands carry their script on stdin");
        let write_result = {
            let write_deadline = time::sleep_until(
                deadline
                    .unwrap_or_else(|| Instant::now() + Duration::from_secs(365 * 24 * 60 * 60)),
            );
            let write = stdin.write_all(script.as_bytes());
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
        AbortWriter::none()
    };

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
    let mut interrupted = false;
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
            _ = async {
                interrupt
                    .as_ref()
                    .expect("guarded by the branch condition")
                    .cancelled()
                    .await
            }, if interrupt.is_some() && !interrupted => {
                interrupted = true;
                process_tree.interrupt();
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
            result.reserve(b"(no output)".len());
            result.extend_from_slice(b"(no output)");
        }
        return;
    }
    // Reserve the final shape once instead of repeatedly growing the result
    // while a large stdout/stderr pair is being assembled.
    let separator_len = if result.is_empty() { 0 } else { b"\n\n".len() };
    let additional = match (stdout.is_empty(), stderr.is_empty()) {
        (false, true) => separator_len + stdout.len(),
        (true, false) => separator_len + b"stderr:\n".len() + stderr.len(),
        (false, false) => {
            separator_len
                + b"stdout:\n".len()
                + stdout.len()
                + if stdout.ends_with(b"\n") { 0 } else { 1 }
                + b"\n\nstderr:\n".len()
                + stderr.len()
        }
        (true, true) => unreachable!("empty streams returned above"),
    };
    result.reserve(additional);
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
    use serde_json::{Map, Value, json};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    fn input_map(value: Value) -> Map<String, Value> {
        serde_json::from_value(value).unwrap()
    }

    fn request<'a>(
        uri: &'a str,
        target: &'a str,
        input: &'a Map<String, Value>,
    ) -> ProtocolRequest<'a> {
        ProtocolRequest { uri, target, input }
    }

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
        for page in [BASH_HELP, PWSH_HELP] {
            assert!(page.contains("Read steps support no shell operations"));
            assert!(page.contains("least one non-whitespace character"));
            assert!(page.contains("MUST NOT add another background layer"));
            assert!(page.contains("\"background\": true"));
            assert!(page.contains("`timeout` (integer seconds, default 1800)"));
            assert!(page.contains("0 disables the timeout"));
            assert!(page.contains("\"interactive\": true"));
            assert!(page.contains("tasks://<id>/send"));
            assert!(page.contains("delivered to the process byte-for-byte"));
            assert!(page.contains("tasks://<id>/eof"));
            assert!(page.contains("tasks://<id>/interrupt"));
            assert!(page.contains("Child processes\nremain owned by this execution"));
            assert!(page.contains("unified `tasks://` protocol"));
            assert!(page.contains("Agent environment variables are injected"));
            assert!(page.contains("`env` (object of string values)"));
            assert!(
                !page.contains("header"),
                "help pages must not mention headers"
            );
            assert!(
                !page.contains("*** "),
                "help pages must not show request envelopes"
            );
        }
        assert!(PWSH_HELP.contains("PowerShell 7 syntax rather than Unix shell syntax"));
        assert!(PWSH_HELP.contains("`$env:NAME = 'value'`"));
        assert!(PWSH_HELP.contains("do not honor `.gitignore`"));
        assert!(PWSH_HELP.contains("do not accept PowerShell\ncommon parameters"));
    }

    #[test]
    fn shell_input_parses_background_timeout_and_env_fields() {
        let plain = input_map(json!({"script": "cargo test"}));
        assert_eq!(
            parse_options("bash", &request("bash://run", "run", &plain)).unwrap(),
            ShellOptions {
                background: false,
                timeout: Some(DEFAULT_TIMEOUT),
                interactive: false,
                env: BTreeMap::new(),
                script: "cargo test".to_string(),
            }
        );

        let unbounded = input_map(json!({
            "script": "cargo test",
            "background": true,
            "timeout": 0,
        }));
        assert_eq!(
            parse_options("bash", &request("bash://run", "run", &unbounded)).unwrap(),
            ShellOptions {
                background: true,
                timeout: None,
                interactive: false,
                env: BTreeMap::new(),
                script: "cargo test".to_string(),
            }
        );

        let bounded = input_map(json!({
            "script": "cargo test",
            "timeout": 30,
            "background": false,
            "env": {"EXTRA": "value"},
        }));
        assert_eq!(
            parse_options("bash", &request("bash://run", "run", &bounded)).unwrap(),
            ShellOptions {
                background: false,
                timeout: Some(Duration::from_secs(30)),
                interactive: false,
                env: BTreeMap::from([("EXTRA".to_string(), "value".to_string())]),
                script: "cargo test".to_string(),
            }
        );

        let typed = input_map(json!({"script": "cargo test", "timeout": "30"}));
        assert!(
            parse_options("bash", &request("bash://run", "run", &typed))
                .unwrap_err()
                .to_string()
                .contains("invalid input for bash://run")
        );
        let unknown = input_map(json!({"script": "cargo test", "other": 30}));
        let error = parse_options("bash", &request("bash://run", "run", &unknown)).unwrap_err();
        assert!(format!("{error:#}").contains("unknown field `other`"));
        assert!(format!("{error:#}").contains("expected one of"));
        let scalars = input_map(json!({"script": "x", "env": {"COUNT": 3, "FLAG": true}}));
        assert_eq!(
            parse_options("bash", &request("bash://run", "run", &scalars))
                .unwrap()
                .env,
            BTreeMap::from([
                ("COUNT".to_string(), "3".to_string()),
                ("FLAG".to_string(), "true".to_string()),
            ]),
            "substituted numbers and booleans become text"
        );
        for env in [
            json!({"EXTRA": {"nested": 1}}),
            json!({"EXTRA": null}),
            json!({"A=B": "x"}),
            json!({"": "x"}),
        ] {
            let values = input_map(json!({"script": "x", "env": env}));
            assert!(
                parse_options("bash", &request("bash://run", "run", &values)).is_err(),
                "{env} must be rejected"
            );
        }
    }

    #[test]
    fn shell_interactive_input_implies_background_execution() {
        let implied = input_map(json!({"script": "mysql -u root -p", "interactive": true}));
        assert_eq!(
            parse_options("bash", &request("bash://run", "run", &implied)).unwrap(),
            ShellOptions {
                background: true,
                timeout: Some(DEFAULT_TIMEOUT),
                interactive: true,
                env: BTreeMap::new(),
                script: "mysql -u root -p".to_string(),
            }
        );

        let explicit = input_map(json!({
            "script": "mysql -u root -p",
            "interactive": true,
            "background": true,
            "timeout": 0,
        }));
        assert_eq!(
            parse_options("bash", &request("bash://run", "run", &explicit)).unwrap(),
            ShellOptions {
                background: true,
                timeout: None,
                interactive: true,
                env: BTreeMap::new(),
                script: "mysql -u root -p".to_string(),
            }
        );

        let off = input_map(json!({"script": "x", "interactive": false}));
        assert_eq!(
            parse_options("bash", &request("bash://run", "run", &off)).unwrap(),
            ShellOptions {
                background: false,
                timeout: Some(DEFAULT_TIMEOUT),
                interactive: false,
                env: BTreeMap::new(),
                script: "x".to_string(),
            }
        );

        let conflicting = input_map(json!({
            "script": "x",
            "interactive": true,
            "background": false,
        }));
        assert!(
            parse_options("bash", &request("bash://run", "run", &conflicting))
                .unwrap_err()
                .to_string()
                .contains("interactive input requires background execution")
        );
        let typed = input_map(json!({"script": "x", "interactive": "1"}));
        assert!(parse_options("bash", &request("bash://run", "run", &typed)).is_err());
    }

    #[test]
    fn shell_script_requires_a_non_whitespace_character() {
        for script in ["", " \n\t"] {
            let input = input_map(json!({"script": script}));
            let error = parse_options("bash", &request("bash://run", "run", &input))
                .unwrap_err()
                .to_string();
            assert!(error.contains("non-whitespace character"), "{error}");
            assert!(error.contains("{\"exec\": \"bash://run\""), "{error}");
        }
    }

    #[tokio::test]
    async fn shell_route_errors_provide_copyable_exec_steps() {
        let shell = ShellProtocol::new("bash", PathBuf::from("bash"), Path::new("."));
        let context = ProtocolContext::new(TaskManager::new());

        let input = input_map(json!({"script": "cargo test"}));
        let read_error = shell
            .read(request("bash://run", "run", &input), context.clone())
            .await
            .unwrap_err();
        assert!(read_error.to_string().contains("{\"exec\": \"bash://run\""));

        let empty = input_map(json!({}));
        let exec_error = shell
            .exec(request("bash://help", "help", &empty), context.clone())
            .await
            .unwrap_err();
        assert!(format!("{exec_error:#}").contains("{\"exec\": \"bash://run\""));

        let help_input = input_map(json!({"script": "x"}));
        let help_error = shell
            .read(request("bash://help", "help", &help_input), context.clone())
            .await
            .unwrap_err();
        assert!(help_error.to_string().contains("takes no input fields"));

        let blank = input_map(json!({"script": " \n\t"}));
        let script_error = shell
            .exec(request("bash://run", "run", &blank), context)
            .await
            .unwrap_err();
        assert!(format!("{script_error:#}").contains("non-whitespace character"));
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
        let context = ProtocolContext::new(TaskManager::new());
        let line_count = 10_000;
        let script =
            format!("1..{line_count} | ForEach-Object {{ Write-Output \"line-$($_):中文-✓\" }}");
        let run_input = input_map(json!({"script": script, "background": true}));

        shell
            .exec(request("pwsh://run", "run", &run_input), context.clone())
            .await
            .unwrap();
        let record = context
            .tasks
            .wait("001", Duration::from_secs(30))
            .await
            .unwrap();
        assert_eq!(record.status, TaskStatus::Completed);
        let empty = input_map(json!({}));
        let detail = TasksProtocol
            .read(request("tasks://001", "001", &empty), context.clone())
            .await
            .unwrap();
        let detail = String::from_utf8(detail.text_bytes().to_vec()).unwrap();
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
        let context = ProtocolContext::new(crate::task::TaskManager::new());
        let run_uri = format!("{protocol}://run");
        let short_input = input_map(json!({"script": short}));
        let completed = shell
            .exec_with_auto_background(
                request(&run_uri, "run", &short_input),
                context.clone(),
                Duration::from_secs(10),
            )
            .await
            .unwrap();
        let completed = String::from_utf8(completed.text_bytes().to_vec()).unwrap();
        assert!(completed.contains("foreground-ok"));
        assert!(!completed.contains("Exit:"));
        assert!(context.tasks.list().await.is_empty());

        let started = Instant::now();
        let delayed_input = input_map(json!({"script": delayed}));
        let accepted = shell
            .exec_with_auto_background(
                request(&run_uri, "run", &delayed_input),
                context.clone(),
                Duration::from_millis(20),
            )
            .await
            .unwrap();
        assert!(started.elapsed() < Duration::from_millis(150));
        let accepted = String::from_utf8(accepted.text_bytes().to_vec()).unwrap();
        assert!(accepted.contains("Background task started: tasks://002"));
        assert!(accepted.contains("then use one bounded wait. Do not poll or rerun"));

        let started = Instant::now();
        let explicit_input = input_map(json!({"script": explicit, "background": true}));
        let accepted = shell
            .exec(request(&run_uri, "run", &explicit_input), context.clone())
            .await
            .unwrap();
        assert!(started.elapsed() < Duration::from_millis(150));
        assert!(
            String::from_utf8(accepted.text_bytes().to_vec())
                .unwrap()
                .contains("tasks://003")
        );

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
    async fn interactive_commands_receive_input_and_read_end_of_file() {
        let directory = tempfile::tempdir().unwrap();
        let (protocol, executable, line_script, bulk_script) = if cfg!(windows) {
            let Some(executable) = find_executable("pwsh") else {
                return;
            };
            (
                "pwsh",
                executable,
                "$line = [Console]::In.ReadLine(); Write-Output \"got:$line\"",
                "$text = [Console]::In.ReadToEnd(); Write-Output \"[$text]\"",
            )
        } else {
            let Some(executable) = find_executable("bash") else {
                return;
            };
            (
                "bash",
                executable,
                "IFS= read -r line; printf 'got:%s' \"$line\"",
                "text=$(cat); printf '[%s]' \"$text\"",
            )
        };
        let mut shell = ShellProtocol::new(protocol, executable, directory.path());
        shell.environment = Some(PluginEnvironment::new(Arc::new(
            AgentEnvironment::load(directory.path()).await.unwrap(),
        )));
        let context = ProtocolContext::new(TaskManager::new());
        let interactive_uri = format!("{protocol}://run");

        let line_input = input_map(json!({"script": line_script, "interactive": true}));
        let accepted = shell
            .exec(
                request(&interactive_uri, "run", &line_input),
                context.clone(),
            )
            .await
            .unwrap();
        assert!(
            String::from_utf8(accepted.text_bytes().to_vec())
                .unwrap()
                .contains("Interactive task started: tasks://001")
        );
        let send = input_map(json!({"text": "hello\n"}));
        TasksProtocol
            .exec(
                request("tasks://001/send", "001/send", &send),
                context.clone(),
            )
            .await
            .unwrap();
        let record = context
            .tasks
            .wait("001", Duration::from_secs(30))
            .await
            .unwrap();
        assert_eq!(
            record.status,
            TaskStatus::Completed,
            "content: {}",
            String::from_utf8_lossy(&record.content)
        );
        let output = String::from_utf8(record.content).unwrap();
        assert!(output.contains("got:hello"), "{output}");

        let bulk_input = input_map(json!({
            "script": bulk_script,
            "interactive": true,
            "timeout": 60,
        }));
        shell
            .exec(
                request(&interactive_uri, "run", &bulk_input),
                context.clone(),
            )
            .await
            .unwrap();
        let send = input_map(json!({"text": "abc\n"}));
        TasksProtocol
            .exec(
                request("tasks://002/send", "002/send", &send),
                context.clone(),
            )
            .await
            .unwrap();
        let empty = input_map(json!({}));
        TasksProtocol
            .exec(
                request("tasks://002/eof", "002/eof", &empty),
                context.clone(),
            )
            .await
            .unwrap();
        let record = context
            .tasks
            .wait("002", Duration::from_secs(30))
            .await
            .unwrap();
        assert_eq!(
            record.status,
            TaskStatus::Completed,
            "content: {}",
            String::from_utf8_lossy(&record.content)
        );
        let output = String::from_utf8(record.content).unwrap();
        assert!(output.contains("[abc"), "{output}");
        context.tasks.shutdown().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn interactive_timeouts_preserve_output_while_waiting_for_input() {
        let directory = tempfile::tempdir().unwrap();
        let Some(executable) = find_executable("bash") else {
            return;
        };
        let mut shell = ShellProtocol::new("bash", executable, directory.path());
        shell.environment = Some(PluginEnvironment::new(Arc::new(
            AgentEnvironment::load(directory.path()).await.unwrap(),
        )));
        let context = ProtocolContext::new(TaskManager::new());
        let interactive_input = input_map(json!({
            "script": "printf waiting; IFS= read -r line",
            "interactive": true,
            "timeout": 1,
        }));

        shell
            .exec(
                request("bash://run", "run", &interactive_input),
                context.clone(),
            )
            .await
            .unwrap();
        let record = context
            .tasks
            .wait("001", Duration::from_secs(30))
            .await
            .unwrap();
        assert_eq!(record.status, TaskStatus::Failed);
        let output = String::from_utf8(record.content).unwrap();
        assert!(output.contains("Command timed out after 1s."), "{output}");
        assert!(output.contains("waiting"), "{output}");
        context.tasks.shutdown().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn interactive_interrupt_signals_a_trapping_process_group() {
        let directory = tempfile::tempdir().unwrap();
        let Some(executable) = find_executable("bash") else {
            return;
        };
        let mut shell = ShellProtocol::new("bash", executable, directory.path());
        shell.environment = Some(PluginEnvironment::new(Arc::new(
            AgentEnvironment::load(directory.path()).await.unwrap(),
        )));
        let context = ProtocolContext::new(TaskManager::new());
        let script =
            "printf ready; trap 'printf trapped; exit 42' INT; while :; do sleep 0.1; done";
        let interactive_input = input_map(json!({
            "script": script,
            "interactive": true,
            "timeout": 60,
        }));

        shell
            .exec(
                request("bash://run", "run", &interactive_input),
                context.clone(),
            )
            .await
            .unwrap();
        // Wait for the trap to be installed before interrupting.
        let mut ready = false;
        for _ in 0..500 {
            if context
                .tasks
                .get("001")
                .await
                .is_some_and(|record| record.content.windows(5).any(|window| window == b"ready"))
            {
                ready = true;
                break;
            }
            time::sleep(Duration::from_millis(10)).await;
        }
        assert!(ready, "the interactive command never reported readiness");

        let empty = input_map(json!({}));
        TasksProtocol
            .exec(
                request("tasks://001/interrupt", "001/interrupt", &empty),
                context.clone(),
            )
            .await
            .unwrap();
        let record = context
            .tasks
            .wait("001", Duration::from_secs(30))
            .await
            .unwrap();
        assert_eq!(record.status, TaskStatus::Failed);
        let output = String::from_utf8(record.content).unwrap();
        assert!(output.contains("trapped"), "{output}");
        assert!(output.contains("Command exited with code 42."), "{output}");
        context.tasks.shutdown().await;
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

    #[tokio::test]
    async fn step_env_values_reach_the_command_and_override_managed_values() {
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
        let mut shell = ShellProtocol::new(protocol, executable, directory.path());
        let environment = Arc::new(AgentEnvironment::load(directory.path()).await.unwrap());
        environment.set(&name, "managed".to_string()).await.unwrap();
        shell.environment = Some(PluginEnvironment::new(environment));
        let run_uri = format!("{protocol}://run");
        let run_input = input_map(json!({
            "script": script,
            "env": {name: "from-step"},
        }));

        let output = shell
            .exec(
                request(&run_uri, "run", &run_input),
                ProtocolContext::new(TaskManager::new()),
            )
            .await
            .unwrap();
        let output = String::from_utf8(output.text_bytes().to_vec()).unwrap();
        assert_eq!(output.trim_end(), "from-step");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pinned_foreground_commands_never_promote_to_background() {
        let directory = tempfile::tempdir().unwrap();
        let Some(executable) = find_executable("bash") else {
            return;
        };
        let mut shell = ShellProtocol::new("bash", executable, directory.path());
        shell.environment = Some(PluginEnvironment::new(Arc::new(
            AgentEnvironment::load(directory.path()).await.unwrap(),
        )));
        let mut context = ProtocolContext::new(TaskManager::new());
        context.pinned_foreground = true;
        let run_input = input_map(json!({"script": "sleep 0.2; printf pinned-ok"}));

        let output = shell
            .exec_with_auto_background(
                request("bash://run", "run", &run_input),
                context.clone(),
                Duration::from_millis(20),
            )
            .await
            .unwrap();

        assert!(
            String::from_utf8(output.text_bytes().to_vec())
                .unwrap()
                .contains("pinned-ok")
        );
        assert!(context.tasks.list().await.is_empty());
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
        // Fork the subshell before the readiness marker: the test drops the
        // execution as soon as the marker appears, and the process-group kill
        // must already cover that descendant.
        let command = format!(
            "(while [ ! -e '{}' ]; do :; done; printf leaked > '{}') & printf started > '{}'; wait",
            release_path.display(),
            leaked_path.display(),
            started_path.display()
        );
        let executable = find_executable("bash").unwrap();
        let mut shell = ShellProtocol::new("bash", executable, directory.path());
        shell.environment = Some(PluginEnvironment::new(Arc::new(
            AgentEnvironment::load(directory.path()).await.unwrap(),
        )));
        let context = ProtocolContext::new(TaskManager::new());
        let run_input = input_map(json!({"script": command}));

        {
            let execution = shell.exec_with_auto_background(
                request("bash://run", "run", &run_input),
                context.clone(),
                Duration::from_secs(60),
            );
            tokio::pin!(execution);
            for _ in 0..1000 {
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
        // Releasing the subshell before the cancelled call settles would let
        // it finish on its own, so this wait must outlive a slow runner.
        for _ in 0..1000 {
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
        // Fork the subshell before the readiness marker: the test drops the
        // execution as soon as the marker appears, and the process-group kill
        // must already cover that descendant.
        let command = format!(
            "(while [ ! -e '{}' ]; do :; done; printf leaked > '{}') & printf started > '{}'; wait",
            release_path.display(),
            leaked_path.display(),
            started_path.display()
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

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_children_get_a_private_console() {
        let Some(executable) = find_executable("pwsh") else {
            return;
        };
        let directory = tempfile::tempdir().unwrap();
        // Spawned children must not share the terminal's console: native
        // programs that read console input would consume or clobber the TUI's
        // input. CREATE_NO_WINDOW gives the child its own hidden console, so
        // only the child itself is attached to it. (DETACHED_PROCESS would
        // remove the console entirely, but pwsh crashes during startup
        // without one.)
        let script = r#"
Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class UriAgentConsoleProbe {
    [DllImport("kernel32.dll", SetLastError = true)]
    public static extern uint GetConsoleProcessList(uint[] processList, uint processCount);
}
'@
$processes = New-Object 'uint[]' 16
$count = [UriAgentConsoleProbe]::GetConsoleProcessList($processes, 16)
Write-Output "attached=$count"
"#;
        let output = execute(
            "pwsh",
            &executable,
            directory.path(),
            script,
            &BTreeMap::new(),
            Some(Duration::from_secs(60)),
            None,
        )
        .await
        .expect("the console probe must succeed");
        let output = String::from_utf8(output).unwrap();
        assert!(
            output.contains("attached=1"),
            "children must get a private console with only themselves attached: {output}"
        );
    }
}
