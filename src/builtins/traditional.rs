use super::atomic_write;
use super::collaboration::{self, CollaborationState};
use super::context::{self, ContextState};
use super::file::resolve_path;
use super::grep;
use super::replace::replace_exact;
use crate::config::display_path;
use crate::output::OutputStore;
use crate::plugin::{
    ModelTool, ModelToolDescriptor, ModelToolOutput, ModelToolRegistry, Plugin, PluginDownloads,
    PluginHost,
};
use crate::process::ProcessTree;
use crate::protocol::ProtocolRegistry;
use crate::retrieval::SearchMode;
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use rhai::{Dynamic, Engine, EvalAltResult, ImmutableString, Position};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

const DEFAULT_READ_LIMIT: usize = 400;
const MAX_READ_LIMIT: usize = 2_000;
const DEFAULT_BASH_TIMEOUT_MS: u64 = 120_000;
const MAX_BASH_TIMEOUT_MS: u64 = 600_000;
const MAX_OUTPUT_BYTES: usize = 64 * 1024;

/// Traditional coding tools for the simple mode. These deliberately do not
/// use URI addresses or the protocol registry; they operate on the same
/// working directory and process boundary as the rest of the session.
pub(super) struct TraditionalToolsPlugin {
    cwd: PathBuf,
}

impl TraditionalToolsPlugin {
    pub(super) fn new(cwd: &Path) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
        }
    }
}

impl Plugin for TraditionalToolsPlugin {
    fn model_tool_descriptors(&self) -> Vec<ModelToolDescriptor> {
        vec![
            ReadTool::new(&self.cwd).descriptor(),
            WriteTool::new(&self.cwd).descriptor(),
            EditTool::new(&self.cwd).descriptor(),
            BashTool::new(&self.cwd).descriptor(),
            search_descriptor(),
        ]
    }

    fn register(&self, host: &mut PluginHost<'_>) -> Result<()> {
        host.model_tools.register(ReadTool::new(&self.cwd))?;
        host.model_tools.register(WriteTool::new(&self.cwd))?;
        host.model_tools.register(EditTool::new(&self.cwd))?;
        host.model_tools.register(BashTool::new(&self.cwd))?;
        let downloads = host.downloads()?;
        host.model_tools
            .register(SearchTool::new(&self.cwd, downloads))?;
        Ok(())
    }
}

#[derive(Clone)]
struct ReadTool {
    cwd: PathBuf,
}

impl ReadTool {
    fn new(cwd: &Path) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadArguments {
    path: String,
    offset: Option<usize>,
    limit: Option<usize>,
}

#[async_trait]
impl ModelTool for ReadTool {
    fn descriptor(&self) -> ModelToolDescriptor {
        ModelToolDescriptor {
            name: "read".to_string(),
            description: "Read a UTF-8 text file with a bounded line range. Relative paths resolve from the startup working directory. Use bash to list directories, search, or inspect binary files.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path."},
                    "offset": {"type": "integer", "minimum": 1, "description": "One-based first line to return. Default 1."},
                    "limit": {"type": "integer", "minimum": 1, "maximum": MAX_READ_LIMIT, "description": "Maximum lines to return. Default 400."},
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(
        &self,
        arguments: &Value,
        _protocols: &ProtocolRegistry,
    ) -> Result<ModelToolOutput> {
        let arguments: ReadArguments =
            serde_json::from_value(arguments.clone()).context("invalid read tool arguments")?;
        if arguments.path.trim().is_empty() {
            bail!("read path cannot be empty");
        }
        let path = resolve_path(&self.cwd, &arguments.path)?;
        let metadata = fs::metadata(&path)
            .await
            .with_context(|| format!("cannot read {}", display_path(&path)))?;
        if !metadata.is_file() {
            bail!(
                "read path is not a regular file: {}; use bash to list directories",
                display_path(&path)
            );
        }
        let content = fs::read_to_string(&path)
            .await
            .with_context(|| format!("cannot read {} as UTF-8 text", display_path(&path)))?;
        let lines = content.lines().collect::<Vec<_>>();
        let total = lines.len();
        let offset = arguments.offset.unwrap_or(1).max(1);
        let limit = arguments
            .limit
            .unwrap_or(DEFAULT_READ_LIMIT)
            .clamp(1, MAX_READ_LIMIT);
        let start = offset.saturating_sub(1).min(total);
        let end = start.saturating_add(limit).min(total);
        let mut output = format!(
            "{} (lines {}-{} of {})\n",
            display_path(&path),
            if total == 0 { 0 } else { start + 1 },
            end,
            total
        );
        for line in &lines[start..end] {
            output.push_str(line);
            output.push('\n');
        }
        if end < total {
            output.push_str(&format!(
                "\n[truncated; continue with offset {}]\n",
                end + 1
            ));
        }
        Ok(output.into())
    }
}

#[derive(Clone)]
struct WriteTool {
    cwd: PathBuf,
}

impl WriteTool {
    fn new(cwd: &Path) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteArguments {
    path: String,
    content: String,
}

#[async_trait]
impl ModelTool for WriteTool {
    fn descriptor(&self) -> ModelToolDescriptor {
        ModelToolDescriptor {
            name: "write".to_string(),
            description: "Write a UTF-8 file atomically, creating parent directories. Relative paths resolve from the startup working directory. Symbolic-link paths are rejected.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path."},
                    "content": {"type": "string", "description": "Complete file content."}
                },
                "required": ["path", "content"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(
        &self,
        arguments: &Value,
        _protocols: &ProtocolRegistry,
    ) -> Result<ModelToolOutput> {
        let arguments: WriteArguments =
            serde_json::from_value(arguments.clone()).context("invalid write tool arguments")?;
        if arguments.path.trim().is_empty() {
            bail!("write path cannot be empty");
        }
        let path = resolve_path(&self.cwd, &arguments.path)?;
        if fs::symlink_metadata(&path)
            .await
            .map(|metadata| metadata.file_type().is_symlink())
            .unwrap_or(false)
        {
            bail!(
                "write paths cannot be symbolic links: {}",
                display_path(&path)
            );
        }
        if fs::metadata(&path)
            .await
            .is_ok_and(|metadata| metadata.is_dir())
        {
            bail!("write path is a directory: {}", display_path(&path));
        }
        atomic_write(&path, arguments.content.as_bytes()).await?;
        Ok(format!(
            "Wrote {} bytes to {}",
            arguments.content.len(),
            display_path(&path)
        )
        .into())
    }
}

#[derive(Clone)]
struct EditTool {
    cwd: PathBuf,
}

impl EditTool {
    fn new(cwd: &Path) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EditArguments {
    path: String,
    old_text: String,
    new_text: String,
}

#[async_trait]
impl ModelTool for EditTool {
    fn descriptor(&self) -> ModelToolDescriptor {
        ModelToolDescriptor {
            name: "edit".to_string(),
            description: "Replace one exact text match in a UTF-8 file atomically. Relative paths resolve from the startup working directory. The old text must be nonempty and occur exactly once; missing or ambiguous matches leave the file unchanged.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path."},
                    "old_text": {"type": "string", "description": "Exact nonempty text to replace. Must occur once."},
                    "new_text": {"type": "string", "description": "Replacement text."}
                },
                "required": ["path", "old_text", "new_text"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(
        &self,
        arguments: &Value,
        _protocols: &ProtocolRegistry,
    ) -> Result<ModelToolOutput> {
        let arguments: EditArguments =
            serde_json::from_value(arguments.clone()).context("invalid edit tool arguments")?;
        if arguments.path.trim().is_empty() {
            bail!("edit path cannot be empty");
        }
        let path = resolve_path(&self.cwd, &arguments.path)?;
        replace_exact(&path, &arguments.old_text, &arguments.new_text).await?;
        Ok(format!("Updated {}", display_path(&path)).into())
    }
}

#[derive(Clone)]
struct BashTool {
    cwd: PathBuf,
}

impl BashTool {
    fn new(cwd: &Path) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BashArguments {
    command: String,
    timeout_ms: Option<u64>,
    cwd: Option<String>,
}

#[async_trait]
impl ModelTool for BashTool {
    fn descriptor(&self) -> ModelToolDescriptor {
        ModelToolDescriptor {
            name: "bash".to_string(),
            description: "Run a shell command and return its output and exit code. Use this for other model CLIs, ripgrep, builds, tests, and file operations. Relative cwd resolves from the startup working directory; the command's output is bounded.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "Shell command to run."},
                    "timeout_ms": {"type": "integer", "minimum": 1, "maximum": MAX_BASH_TIMEOUT_MS, "description": "Timeout in milliseconds. Default 120000."},
                    "cwd": {"type": "string", "description": "Optional working directory, relative to the startup directory."}
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(
        &self,
        arguments: &Value,
        _protocols: &ProtocolRegistry,
    ) -> Result<ModelToolOutput> {
        let arguments: BashArguments =
            serde_json::from_value(arguments.clone()).context("invalid bash tool arguments")?;
        if arguments.command.trim().is_empty() {
            bail!("bash command cannot be empty");
        }
        let cwd = match arguments.cwd.as_deref() {
            Some(cwd) if !cwd.trim().is_empty() => resolve_path(&self.cwd, cwd)?,
            _ => self.cwd.clone(),
        };
        let metadata = fs::metadata(&cwd)
            .await
            .with_context(|| format!("cannot use cwd {}", display_path(&cwd)))?;
        if !metadata.is_dir() {
            bail!("bash cwd is not a directory: {}", display_path(&cwd));
        }
        let timeout = Duration::from_millis(
            arguments
                .timeout_ms
                .unwrap_or(DEFAULT_BASH_TIMEOUT_MS)
                .clamp(1, MAX_BASH_TIMEOUT_MS),
        );
        let (stdout, stderr, status) = run_command(&cwd, &arguments.command, timeout).await?;
        let mut output = format!(
            "exit_code: {}\n",
            status
                .code()
                .map_or_else(|| "signal".to_string(), |code| code.to_string())
        );
        output.push_str(&String::from_utf8_lossy(&stdout));
        if !stderr.is_empty() {
            if !output.ends_with('\n') {
                output.push('\n');
            }
            output.push_str("[stderr]\n");
            output.push_str(&String::from_utf8_lossy(&stderr));
        }
        Ok(truncate_output(output).into())
    }
}

fn search_descriptor() -> ModelToolDescriptor {
    ModelToolDescriptor {
        name: "search".to_string(),
        description: "Search file contents. `exact` uses ripgrep and returns `path:line:text` lines; `semantic` and `hybrid` use the local code index and return ranked fragments. Relative roots resolve from the startup working directory.".to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "Search pattern or natural-language query."},
                "mode": {"type": "string", "enum": ["exact", "semantic", "hybrid"], "description": "Search mode. Default exact."},
                "root": {"type": "string", "description": "Project-relative or absolute file/directory root. Default the startup directory."},
                "glob": {"type": "string", "description": "Optional ripgrep-style glob filter."},
                "limit": {"type": "integer", "minimum": 1, "maximum": 2000, "description": "Maximum matches. Exact default 200; ranked default 7 and at most 50."}
            },
            "required": ["query"],
            "additionalProperties": false
        }),
    }
}

#[derive(Clone)]
struct SearchTool {
    cwd: PathBuf,
    downloads: PluginDownloads,
}

impl SearchTool {
    fn new(cwd: &Path, downloads: PluginDownloads) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
            downloads,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchArguments {
    query: String,
    mode: Option<String>,
    root: Option<String>,
    glob: Option<String>,
    limit: Option<usize>,
}

#[async_trait]
impl ModelTool for SearchTool {
    fn descriptor(&self) -> ModelToolDescriptor {
        search_descriptor()
    }

    async fn execute(
        &self,
        arguments: &Value,
        _protocols: &ProtocolRegistry,
    ) -> Result<ModelToolOutput> {
        let arguments: SearchArguments =
            serde_json::from_value(arguments.clone()).context("invalid search tool arguments")?;
        if arguments.query.trim().is_empty() {
            bail!("search query cannot be empty");
        }
        let root = arguments.root.as_deref().unwrap_or_default();
        match arguments.mode.as_deref() {
            None | Some("exact") => grep::exact_search(
                &self.downloads,
                &self.cwd,
                root,
                &arguments.query,
                arguments.glob.as_deref(),
                arguments.limit,
            )
            .await
            .map(Into::into),
            Some(mode @ ("semantic" | "hybrid")) => {
                let mode = SearchMode::parse(mode, "search")?;
                grep::semantic_search(
                    &self.cwd,
                    root,
                    &arguments.query,
                    mode,
                    arguments.glob.as_deref(),
                    arguments.limit,
                )
                .await
                .map(Into::into)
            }
            Some(other) => bail!("search mode must be exact, semantic, or hybrid; got {other:?}"),
        }
    }
}

#[derive(Clone)]
pub(crate) struct NotesTool {
    state: ContextState,
}

impl NotesTool {
    pub(crate) fn new(state: ContextState) -> Self {
        Self { state }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NotesArguments {
    op: String,
    id: Option<String>,
    title: Option<String>,
    content: Option<String>,
    offset: Option<usize>,
    limit: Option<usize>,
}

#[async_trait]
impl ModelTool for NotesTool {
    fn descriptor(&self) -> ModelToolDescriptor {
        ModelToolDescriptor {
            name: "notes".to_string(),
            description: "Maintain this session's durable working notes. `list` shows the note index, `read` returns one note, and `add`, `replace`, `delete` mutate the session's note state. Titles are single-line and at most 120 characters.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "op": {"type": "string", "enum": ["list", "read", "add", "replace", "delete"], "description": "Operation."},
                    "id": {"type": "string", "description": "Note ID, required for read, replace, and delete."},
                    "title": {"type": "string", "description": "Single-line title, required for add and replace."},
                    "content": {"type": "string", "description": "Note body, required for add and replace."},
                    "offset": {"type": "integer", "minimum": 1, "description": "Character offset for read. Default start."},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 7000, "description": "Maximum characters for read. Default 7000."}
                },
                "required": ["op"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(
        &self,
        arguments: &Value,
        _protocols: &ProtocolRegistry,
    ) -> Result<ModelToolOutput> {
        let arguments: NotesArguments =
            serde_json::from_value(arguments.clone()).context("invalid notes tool arguments")?;
        let result = match arguments.op.as_str() {
            "list" => context::notes_list_tool(&self.state).await,
            "read" => {
                let id = required_field(arguments.id, "id", "read")?;
                context::notes_read_tool(&self.state, &id, arguments.offset, arguments.limit).await
            }
            "add" => {
                let title = required_field(arguments.title, "title", "add")?;
                let content = required_field(arguments.content, "content", "add")?;
                context::notes_add_tool(&self.state, &title, &content).await
            }
            "replace" => {
                let id = required_field(arguments.id, "id", "replace")?;
                let title = required_field(arguments.title, "title", "replace")?;
                let content = required_field(arguments.content, "content", "replace")?;
                context::notes_replace_tool(&self.state, &id, &title, &content).await
            }
            "delete" => {
                let id = required_field(arguments.id, "id", "delete")?;
                context::notes_delete_tool(&self.state, &id).await
            }
            other => bail!("notes op must be list, read, add, replace, or delete; got {other:?}"),
        }?;
        Ok(result.into())
    }
}

fn required_field(value: Option<String>, field: &str, op: &str) -> Result<String> {
    value.ok_or_else(|| anyhow::anyhow!("notes op {op} requires `{field}`"))
}

#[derive(Clone)]
pub(crate) struct SteerTool {
    state: CollaborationState,
}

impl SteerTool {
    pub(crate) fn new(state: CollaborationState) -> Self {
        Self { state }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SteerArguments {
    target: String,
    message: String,
    delivery: Option<String>,
    reply: Option<String>,
    scope: Option<String>,
    in_reply_to: Option<String>,
}

#[async_trait]
impl ModelTool for SteerTool {
    fn descriptor(&self) -> ModelToolDescriptor {
        ModelToolDescriptor {
            name: "steer".to_string(),
            description: "Send a message to another active UAT session. `delivery: \"steer\"` targets the next model boundary; `queue` durably schedules a later turn. The target may be a live session name or session ID.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "target": {"type": "string", "description": "Live session name or stable session ID."},
                    "message": {"type": "string", "description": "Message body."},
                    "delivery": {"type": "string", "enum": ["queue", "steer"], "description": "Delivery mode. Default queue."},
                    "reply": {"type": "string", "enum": ["none", "requested"], "description": "Whether to request a reply. Default none."},
                    "scope": {"type": "string", "enum": ["project", "all"], "description": "Target lookup scope. Default project."},
                    "in_reply_to": {"type": "string", "description": "Optional collaboration message ID being answered."}
                },
                "required": ["target", "message"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(
        &self,
        arguments: &Value,
        _protocols: &ProtocolRegistry,
    ) -> Result<ModelToolOutput> {
        let arguments: SteerArguments =
            serde_json::from_value(arguments.clone()).context("invalid steer tool arguments")?;
        if arguments.target.trim().is_empty() {
            bail!("steer target cannot be empty");
        }
        collaboration::send_tool_message(
            &self.state,
            &arguments.target,
            &arguments.message,
            arguments.delivery.as_deref(),
            arguments.reply.as_deref(),
            arguments.scope.as_deref(),
            arguments.in_reply_to.as_deref(),
        )
        .await
        .map(Into::into)
    }
}

/// Runs model-written Rhai scripts against the same tool registry. Only the
/// script's `text(...)` output and return value reach the model.
#[derive(Clone)]
pub(crate) struct CodemodeTool {
    tools: Arc<OnceLock<Weak<ModelToolRegistry>>>,
    protocols: Arc<ProtocolRegistry>,
    output: Arc<OutputStore>,
}

impl CodemodeTool {
    pub(crate) fn new(protocols: Arc<ProtocolRegistry>, output: Arc<OutputStore>) -> Self {
        Self {
            tools: Arc::new(OnceLock::new()),
            protocols,
            output,
        }
    }

    pub(crate) fn bind(&self, tools: Weak<ModelToolRegistry>) {
        let _ = self.tools.set(tools);
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CodemodeArguments {
    code: String,
}

#[async_trait]
impl ModelTool for CodemodeTool {
    fn descriptor(&self) -> ModelToolDescriptor {
        ModelToolDescriptor {
            name: "codemode".to_string(),
            description: "Run a Rhai script that calls the other tools. Use `bash(\"command\")`, `read(\"path\")`, `write(path, content)`, `edit(path, old, new)`, `search(query)`, `search_mode(query, mode)`, `notes_list()`, `notes_read(id)`, `notes_add(title, content)`, `notes_replace(id, title, content)`, `notes_delete(id)`, `steer(target, message)`, `mcp(server, tool, json_args)`, `apply_patch(patch)`, or `tool(name, json_args)`. Only values passed to `text(...)` or returned reach you; intermediate tool output stays inside the script. Scripts run synchronously with a 60 second limit.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "code": {"type": "string", "description": "Rhai script source. Example: let out = bash(\"rg -n TODO src\"); text(out);", "maxLength": 65536}
                },
                "required": ["code"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(
        &self,
        arguments: &Value,
        _protocols: &ProtocolRegistry,
    ) -> Result<ModelToolOutput> {
        let arguments: CodemodeArguments =
            serde_json::from_value(arguments.clone()).context("invalid codemode tool arguments")?;
        if arguments.code.trim().is_empty() {
            bail!("codemode script cannot be empty");
        }
        let sink = Arc::new(Mutex::new(String::new()));
        let mut engine = Engine::new();
        engine
            .set_max_operations(5_000_000)
            .set_max_string_size(2_000_000)
            .set_max_array_size(10_000)
            .set_max_map_size(10_000);
        let deadline = Instant::now() + Duration::from_secs(60);
        engine.on_progress(move |_| {
            (Instant::now() >= deadline).then(|| Dynamic::from("codemode timed out after 60s"))
        });
        let print_sink = sink.clone();
        engine.on_print(move |line| append(&print_sink, line));
        register_codemode_functions(
            &mut engine,
            self.tools.clone(),
            self.protocols.clone(),
            sink.clone(),
        );
        let result = tokio::task::block_in_place(|| engine.eval::<Dynamic>(&arguments.code));
        let mut text = sink.lock().expect("codemode output lock poisoned").clone();
        match result {
            Ok(value) => {
                if !value.is_unit() {
                    append(&sink, &value.to_string());
                    text = sink.lock().expect("codemode output lock poisoned").clone();
                }
            }
            Err(error) => {
                bail!(
                    "codemode script failed: {error}\n\nPartial output:\n{}",
                    if text.trim().is_empty() {
                        "(none)"
                    } else {
                        text.as_str()
                    }
                );
            }
        }
        if text.trim().is_empty() {
            return Ok("Script completed with no output.".into());
        }
        self.output
            .present(text.into_bytes(), "codemode")
            .await
            .map(Into::into)
    }
}

fn register_codemode_functions(
    engine: &mut Engine,
    tools: Arc<OnceLock<Weak<ModelToolRegistry>>>,
    protocols: Arc<ProtocolRegistry>,
    sink: Arc<Mutex<String>>,
) {
    let text_sink = sink.clone();
    engine.register_fn("text", move |value: Dynamic| {
        append(&text_sink, &value.to_string());
    });

    let tool_registry = tools.clone();
    let tool_protocols = protocols.clone();
    engine.register_fn(
        "tool",
        move |name: ImmutableString,
              args: ImmutableString|
              -> Result<ImmutableString, Box<EvalAltResult>> {
            let args: Value = serde_json::from_str(args.as_str())
                .map_err(|error| script_error(format!("tool arguments must be JSON: {error}")))?;
            call_tool(&tool_registry, &tool_protocols, name.as_str(), args)
                .map(ImmutableString::from)
        },
    );

    let bash_tools = tools.clone();
    let bash_protocols = protocols.clone();
    engine.register_fn(
        "bash",
        move |command: ImmutableString| -> Result<ImmutableString, Box<EvalAltResult>> {
            call_tool(
                &bash_tools,
                &bash_protocols,
                "bash",
                json!({"command": command.as_str()}),
            )
            .map(ImmutableString::from)
        },
    );

    let read_tools = tools.clone();
    let read_protocols = protocols.clone();
    engine.register_fn(
        "read",
        move |path: ImmutableString| -> Result<ImmutableString, Box<EvalAltResult>> {
            call_tool(
                &read_tools,
                &read_protocols,
                "read",
                json!({"path": path.as_str()}),
            )
            .map(ImmutableString::from)
        },
    );

    let write_tools = tools.clone();
    let write_protocols = protocols.clone();
    engine.register_fn(
        "write",
        move |path: ImmutableString,
              content: ImmutableString|
              -> Result<ImmutableString, Box<EvalAltResult>> {
            call_tool(
                &write_tools,
                &write_protocols,
                "write",
                json!({"path": path.as_str(), "content": content.as_str()}),
            )
            .map(ImmutableString::from)
        },
    );

    let edit_tools = tools.clone();
    let edit_protocols = protocols.clone();
    engine.register_fn(
        "edit",
        move |path: ImmutableString,
              old_text: ImmutableString,
              new_text: ImmutableString|
              -> Result<ImmutableString, Box<EvalAltResult>> {
            call_tool(
                &edit_tools,
                &edit_protocols,
                "edit",
                json!({
                    "path": path.as_str(),
                    "old_text": old_text.as_str(),
                    "new_text": new_text.as_str()
                }),
            )
            .map(ImmutableString::from)
        },
    );

    let search_tools = tools.clone();
    let search_protocols = protocols.clone();
    engine.register_fn(
        "search",
        move |query: ImmutableString| -> Result<ImmutableString, Box<EvalAltResult>> {
            call_tool(
                &search_tools,
                &search_protocols,
                "search",
                json!({"query": query.as_str()}),
            )
            .map(ImmutableString::from)
        },
    );

    let search_mode_tools = tools.clone();
    let search_mode_protocols = protocols.clone();
    engine.register_fn(
        "search_mode",
        move |query: ImmutableString,
              mode: ImmutableString|
              -> Result<ImmutableString, Box<EvalAltResult>> {
            call_tool(
                &search_mode_tools,
                &search_mode_protocols,
                "search",
                json!({"query": query.as_str(), "mode": mode.as_str()}),
            )
            .map(ImmutableString::from)
        },
    );

    let notes_tools = tools.clone();
    let notes_protocols = protocols.clone();
    engine.register_fn(
        "notes_list",
        move || -> Result<ImmutableString, Box<EvalAltResult>> {
            call_tool(
                &notes_tools,
                &notes_protocols,
                "notes",
                json!({"op": "list"}),
            )
            .map(ImmutableString::from)
        },
    );

    let notes_read_tools = tools.clone();
    let notes_read_protocols = protocols.clone();
    engine.register_fn(
        "notes_read",
        move |id: ImmutableString| -> Result<ImmutableString, Box<EvalAltResult>> {
            call_tool(
                &notes_read_tools,
                &notes_read_protocols,
                "notes",
                json!({"op": "read", "id": id.as_str()}),
            )
            .map(ImmutableString::from)
        },
    );

    let notes_add_tools = tools.clone();
    let notes_add_protocols = protocols.clone();
    engine.register_fn(
        "notes_add",
        move |title: ImmutableString,
              content: ImmutableString|
              -> Result<ImmutableString, Box<EvalAltResult>> {
            call_tool(
                &notes_add_tools,
                &notes_add_protocols,
                "notes",
                json!({"op": "add", "title": title.as_str(), "content": content.as_str()}),
            )
            .map(ImmutableString::from)
        },
    );

    let notes_replace_tools = tools.clone();
    let notes_replace_protocols = protocols.clone();
    engine.register_fn(
        "notes_replace",
        move |id: ImmutableString,
              title: ImmutableString,
              content: ImmutableString|
              -> Result<ImmutableString, Box<EvalAltResult>> {
            call_tool(
                &notes_replace_tools,
                &notes_replace_protocols,
                "notes",
                json!({
                    "op": "replace",
                    "id": id.as_str(),
                    "title": title.as_str(),
                    "content": content.as_str()
                }),
            )
            .map(ImmutableString::from)
        },
    );

    let notes_delete_tools = tools.clone();
    let notes_delete_protocols = protocols.clone();
    engine.register_fn(
        "notes_delete",
        move |id: ImmutableString| -> Result<ImmutableString, Box<EvalAltResult>> {
            call_tool(
                &notes_delete_tools,
                &notes_delete_protocols,
                "notes",
                json!({"op": "delete", "id": id.as_str()}),
            )
            .map(ImmutableString::from)
        },
    );

    let steer_tools = tools.clone();
    let steer_protocols = protocols.clone();
    engine.register_fn(
        "steer",
        move |target: ImmutableString,
              message: ImmutableString|
              -> Result<ImmutableString, Box<EvalAltResult>> {
            call_tool(
                &steer_tools,
                &steer_protocols,
                "steer",
                json!({"target": target.as_str(), "message": message.as_str()}),
            )
            .map(ImmutableString::from)
        },
    );

    let mcp_tools = tools.clone();
    let mcp_protocols = protocols.clone();
    engine.register_fn(
        "mcp",
        move |server: ImmutableString,
              tool_name: ImmutableString,
              args: ImmutableString|
              -> Result<ImmutableString, Box<EvalAltResult>> {
            let args: Value = serde_json::from_str(args.as_str()).map_err(|error| {
                script_error(format!("mcp arguments must be a JSON object: {error}"))
            })?;
            call_tool(
                &mcp_tools,
                &mcp_protocols,
                "mcp",
                json!({
                    "server": server.as_str(),
                    "tool": tool_name.as_str(),
                    "arguments": args
                }),
            )
            .map(ImmutableString::from)
        },
    );

    let apply_patch_tools = tools;
    let apply_patch_protocols = protocols;
    engine.register_fn(
        "apply_patch",
        move |patch: ImmutableString| -> Result<ImmutableString, Box<EvalAltResult>> {
            call_tool(
                &apply_patch_tools,
                &apply_patch_protocols,
                "apply_patch",
                json!({"patch": patch.as_str()}),
            )
            .map(ImmutableString::from)
        },
    );
}

fn append(sink: &Arc<Mutex<String>>, text: &str) {
    let mut output = sink.lock().expect("codemode output lock poisoned");
    if !output.is_empty() {
        output.push('\n');
    }
    output.push_str(text);
}

fn call_tool(
    tools: &Arc<OnceLock<Weak<ModelToolRegistry>>>,
    protocols: &Arc<ProtocolRegistry>,
    name: &str,
    args: Value,
) -> std::result::Result<String, Box<EvalAltResult>> {
    let registry = tools
        .get()
        .and_then(Weak::upgrade)
        .ok_or_else(|| script_error("tool registry is not ready"))?;
    let result = tokio::runtime::Handle::current()
        .block_on(async { registry.dispatch(name, &args, protocols).await });
    match result {
        Ok(output) => Ok(output.text().to_string()),
        Err(error) => Err(script_error(format!("tool {name} failed: {error:#}"))),
    }
}

fn script_error(message: impl Into<String>) -> Box<EvalAltResult> {
    Box::new(EvalAltResult::ErrorRuntime(
        message.into().into(),
        Position::NONE,
    ))
}

async fn run_command(
    cwd: &Path,
    script: &str,
    timeout: Duration,
) -> Result<(Vec<u8>, Vec<u8>, std::process::ExitStatus)> {
    let mut command = Command::new(shell_executable());
    shell_arguments(&mut command);
    command
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let (mut child, tree) = ProcessTree::spawn(&mut command)
        .with_context(|| format!("cannot start a shell in {}", display_path(cwd)))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow::anyhow!("failed to open shell stdin"))?;
    let write = async {
        stdin.write_all(script.as_bytes()).await?;
        stdin.shutdown().await
    };
    write
        .await
        .with_context(|| "failed to send the command to the shell")?;
    drop(stdin);

    let wait = async {
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("failed to open shell stdout"))?;
        let mut stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow::anyhow!("failed to open shell stderr"))?;
        let stdout = async {
            let mut bytes = Vec::new();
            stdout.read_to_end(&mut bytes).await?;
            Ok::<_, std::io::Error>(bytes)
        };
        let stderr = async {
            let mut bytes = Vec::new();
            stderr.read_to_end(&mut bytes).await?;
            Ok::<_, std::io::Error>(bytes)
        };
        let status = child.wait();
        let (stdout, stderr, status) = tokio::join!(stdout, stderr, status);
        Ok::<_, anyhow::Error>((stdout?, stderr?, status?))
    };
    match tokio::time::timeout(timeout, wait).await {
        Ok(result) => {
            drop(tree);
            result
        }
        Err(_) => {
            tree.terminate();
            let _ = child.wait().await;
            bail!("bash command timed out after {}s", timeout.as_secs_f32());
        }
    }
}

#[cfg(unix)]
fn shell_executable() -> &'static str {
    "bash"
}

#[cfg(windows)]
fn shell_executable() -> &'static str {
    "powershell"
}

#[cfg(unix)]
fn shell_arguments(command: &mut Command) {
    command.args(["--noprofile", "--norc"]);
}

#[cfg(windows)]
fn shell_arguments(command: &mut Command) {
    command.args(["-NoLogo", "-NoProfile", "-NonInteractive", "-Command", "-"]);
}

fn truncate_output(mut output: String) -> String {
    if output.len() <= MAX_OUTPUT_BYTES {
        return output;
    }
    let mut head = MAX_OUTPUT_BYTES * 3 / 4;
    while head > 0 && !output.is_char_boundary(head) {
        head -= 1;
    }
    let mut tail_start = output.len() - (MAX_OUTPUT_BYTES - head);
    while tail_start < output.len() && !output.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    let mut truncated = output[..head].to_string();
    truncated.push_str("\n…[output truncated]…\n");
    truncated.push_str(&output[tail_start..]);
    output.clear();
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::OutputStore;
    use crate::task::TaskManager;
    use std::sync::Arc;

    async fn protocols() -> ProtocolRegistry {
        let output = Arc::new(
            OutputStore::new(
                &format!("traditional-{}", uuid::Uuid::now_v7().simple()),
                1024,
            )
            .await
            .unwrap(),
        );
        ProtocolRegistry::new(output, TaskManager::new())
    }

    #[tokio::test]
    async fn read_write_and_edit_round_trip() {
        let directory = tempfile::tempdir().unwrap();
        let protocols = protocols().await;
        let write = WriteTool::new(directory.path());
        write
            .execute(
                &json!({"path": "notes/todo.txt", "content": "alpha\nbeta\n"}),
                &protocols,
            )
            .await
            .unwrap();

        let read = ReadTool::new(directory.path());
        let output = read
            .execute(
                &json!({"path": "notes/todo.txt", "offset": 2, "limit": 1}),
                &protocols,
            )
            .await
            .unwrap();
        assert!(output.output().contains("beta"));
        assert!(!output.output().contains("alpha"));

        let edit = EditTool::new(directory.path());
        edit.execute(
            &json!({"path": "notes/todo.txt", "old_text": "beta", "new_text": "gamma"}),
            &protocols,
        )
        .await
        .unwrap();
        assert_eq!(
            fs::read_to_string(directory.path().join("notes/todo.txt"))
                .await
                .unwrap(),
            "alpha\ngamma\n"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bash_returns_output_and_exit_code() {
        let directory = tempfile::tempdir().unwrap();
        let protocols = protocols().await;
        let bash = BashTool::new(directory.path());
        let output = bash
            .execute(&json!({"command": "printf 'hello\\n'; exit 3"}), &protocols)
            .await
            .unwrap();
        assert!(output.output().contains("exit_code: 3"));
        assert!(output.output().contains("hello"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bash_runs_in_the_requested_directory() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("sub")).await.unwrap();
        fs::write(directory.path().join("sub/marker.txt"), "found\n")
            .await
            .unwrap();
        let protocols = protocols().await;
        let bash = BashTool::new(directory.path());
        let output = bash
            .execute(
                &json!({"command": "cat marker.txt", "cwd": "sub"}),
                &protocols,
            )
            .await
            .unwrap();
        assert!(output.output().contains("found"));
    }

    struct EchoTool;

    #[async_trait]
    impl ModelTool for EchoTool {
        fn descriptor(&self) -> ModelToolDescriptor {
            ModelToolDescriptor {
                name: "echo".to_string(),
                description: "Echo text.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {"text": {"type": "string"}},
                    "required": ["text"],
                    "additionalProperties": false
                }),
            }
        }

        async fn execute(
            &self,
            arguments: &Value,
            _protocols: &ProtocolRegistry,
        ) -> Result<ModelToolOutput> {
            Ok(format!("echo:{}", arguments["text"].as_str().unwrap_or_default()).into())
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn codemode_runs_a_script_and_returns_only_script_output() {
        let protocols = Arc::new(protocols().await);
        let output = Arc::new(
            OutputStore::new(
                &format!("codemode-{}", uuid::Uuid::now_v7().simple()),
                32768,
            )
            .await
            .unwrap(),
        );
        let mut registry = ModelToolRegistry::new();
        registry.register(EchoTool).unwrap();
        let registry = Arc::new(registry);
        let tool = CodemodeTool::new(protocols.clone(), output.clone());
        tool.bind(Arc::downgrade(&registry));
        let result = tool
            .execute(
                &json!({
                    "code": r#"let out = tool("echo", "{\"text\":\"hi\"}"); text(out);"#
                }),
                &protocols,
            )
            .await
            .unwrap();
        assert!(result.text().contains("echo:hi"));
        let _ = fs::remove_dir_all(output.directory()).await;
    }
}
