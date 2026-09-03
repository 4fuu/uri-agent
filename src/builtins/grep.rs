use super::file::resolve_path;
use crate::config::display_path;
use crate::plugin::{
    BinaryDownload, DownloadArchive, Plugin, PluginDownloads, PluginHost, PluginPermission,
};
use crate::prompts;
use crate::protocol::{Protocol, ProtocolContext, ProtocolDescriptor, ProtocolRequest};
use crate::retrieval::{
    SearchFilter, SearchHit, SearchMode, code_corpus, index_checkpoint, index_status,
    rebuild_index, search_index, sync_index,
};
use crate::task::AutoTask;
use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

const DEFAULT_LIMIT: usize = 200;
const MAX_LIMIT: usize = 2_000;
const MAX_SEMANTIC_LIMIT: usize = 50;
const MAX_CONTEXT: usize = 20;
const RIPGREP_VERSION: &str = "14.1.1";
const AUTO_BACKGROUND_AFTER: Duration = Duration::from_secs(60);
const MAX_INDEX_RETRIES: usize = 3;

fn help(cwd: &Path) -> String {
    format!(
        r#"# grep

Search file contents with exact ripgrep matching or on-demand semantic retrieval.

Current working directory: `grep://{}`

Search reads MUST pass a nonempty search pattern in the string body. Use
`grep://<root>` for a project-relative or absolute file/directory root. The root
may be empty: `grep://` searches the current working directory. On Unix, `~`
and paths beginning with `~/` resolve from the current user's home directory;
`~user` is not expanded.

Optional query parameters:

- `mode=exact` (the default) uses ripgrep. Use it for known identifiers, paths,
  syntax, or literal wording.
- `mode=hybrid` combines keyword and semantic ranking. Prefer it for conceptual
  searches.
- `mode=semantic` prioritizes meaning over shared wording. Use it when relevant
  results are likely to use different wording from the query.
- `mode=status` is a diagnostic that reports whether the selected root's
  semantic cache is current; its body must be empty.
- `glob=<pattern>` filters searched paths with a glob pattern.
- Search patterns are regular expressions by default. If a pattern is not valid
  as a regular expression, the protocol retries it as literal text.
- `literal=true` always treats the body as literal text.
- `ignore_case=true` enables case-insensitive matching.
- `context=<0..20>` includes surrounding lines.
- `limit=<1..2000>` bounds the number of matches; the default is 200.

Semantic and hybrid reads accept only `mode`, `glob`, and `limit`; their
default limit is 7 and maximum is 50. A ranked read creates or incrementally
refreshes its selected root/glob cache as needed, then searches it. Most
searches return in the same call; a longer search continues as one managed task
without restarting and delivers its result automatically. If completion marks
the output as truncated, follow its `tasks://` instruction once. Do not submit
the same search again to retrieve task output.

Do not call status or index before a ranked search. Use `mode=status` only to
diagnose the cache. Use `exec` only to prewarm or force-rebuild that exact
root/glob cache:

```text
exec("grep://<root>?mode=index&glob=<pattern>", "")
```

Indexing follows standard ignore files, skips binary/non-UTF-8 files and files
larger than 1 MiB, and chunks readable text into line-ranged fragments. Results
show the actual matching fragment with its precise line range. Index data is a
private rebuildable cache; source files are never changed.

Examples:

```text
read("grep://src?glob=**/*.rs&limit=100", "ProtocolRequest")
read("grep://src/tui/app.rs", "fn push(")
read("grep://?literal=true&ignore_case=true", "exact text")
read("grep://src?mode=hybrid&glob=**/*.rs&limit=10", "authentication flow")
exec("grep://src?mode=index&glob=**/*.rs", "")
```

`grep://help` MUST use an empty string body. `exec` supports only
`mode=index` with an empty body.
"#,
        display_path(cwd)
    )
}

#[derive(Clone)]
pub(super) struct GrepProtocol {
    cwd: PathBuf,
    downloads: Option<PluginDownloads>,
}

impl GrepProtocol {
    pub(super) fn new(cwd: &Path) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
            downloads: None,
        }
    }

    #[cfg(test)]
    fn with_downloads(mut self, downloads: PluginDownloads) -> Self {
        self.downloads = Some(downloads);
        self
    }
}

impl Plugin for GrepProtocol {
    fn protocol_descriptors(&self) -> Vec<ProtocolDescriptor> {
        vec![self.descriptor()]
    }

    fn permissions(&self) -> Vec<PluginPermission> {
        vec![PluginPermission::Downloads]
    }

    fn register(&self, host: &mut PluginHost<'_>) -> Result<()> {
        let mut protocol = self.clone();
        protocol.downloads = Some(host.downloads()?);
        host.protocols.register(protocol)
    }
}

#[async_trait]
impl Protocol for GrepProtocol {
    fn descriptor(&self) -> ProtocolDescriptor {
        ProtocolDescriptor {
            name: "grep".to_string(),
            description: "Search file contents with exact ripgrep matching or on-demand semantic and hybrid retrieval.".to_string(),
            can_read: true,
            can_exec: true,
        }
    }

    async fn read(
        &self,
        request: ProtocolRequest<'_>,
        context: ProtocolContext,
    ) -> Result<Vec<u8>> {
        if request.target == "help" {
            if !request.body.is_empty() {
                bail!(r#"grep://help requires an empty body; retry read("grep://help", "")"#);
            }
            return Ok(help(&self.cwd).into_bytes());
        }
        let (root, query) = request
            .target
            .split_once('?')
            .map_or((request.target, None), |(root, query)| (root, Some(query)));
        let options = GrepOptions::parse(query)?;
        let resolved = resolve_path(&self.cwd, root)?;
        validate_root(&resolved).await?;
        match options.mode {
            GrepMode::Exact => {
                require_search_body(request.body, request.uri)?;
                let downloads = self
                    .downloads
                    .as_ref()
                    .ok_or_else(|| anyhow!("grep binary download access is not attached"))?;
                let rg = downloads.ensure(&ripgrep_download()?).await?;
                run_grep(
                    &rg,
                    &self.cwd,
                    &grep_root_argument(&self.cwd, root, &resolved),
                    request.body,
                    &options,
                )
                .await
                .map(String::into_bytes)
            }
            GrepMode::Semantic(mode) => {
                require_search_body(request.body, request.uri)?;
                options.validate_semantic()?;
                run_semantic_grep(
                    self.cwd.clone(),
                    resolved,
                    options.glob.clone(),
                    request.body.to_string(),
                    mode,
                    options.semantic_limit(),
                    context,
                )
                .await
            }
            GrepMode::Status => {
                if !request.body.is_empty() {
                    bail!("grep semantic index status requires an empty body");
                }
                options.validate_index_operation()?;
                let corpus = code_corpus(&self.cwd, &resolved, options.glob.as_deref()).await?;
                Ok(index_status(&corpus.spec, &corpus.catalog)
                    .await?
                    .format("Code")
                    .into_bytes())
            }
        }
    }

    async fn exec(
        &self,
        request: ProtocolRequest<'_>,
        context: ProtocolContext,
    ) -> Result<Vec<u8>> {
        let (root, query) = request
            .target
            .split_once('?')
            .map_or((request.target, None), |(root, query)| (root, Some(query)));
        let options = GrepOptions::parse_exec(query)?;
        if !request.body.is_empty() {
            bail!("grep semantic indexing requires an empty body");
        }
        options.validate_index_operation()?;
        let resolved = resolve_path(&self.cwd, root)?;
        validate_root(&resolved).await?;
        let label = format!("Index code under {}", display_path(&resolved));
        let record = context.tasks.allocate_background("grep", label).await?;
        let id = record.id.clone();
        let cwd = self.cwd.clone();
        let glob = options.glob.clone();
        context
            .tasks
            .spawn_with_cancellation(record, move |cancellation| async move {
                rebuild_code_index(&cwd, &resolved, glob.as_deref(), cancellation).await
            })
            .await;
        Ok(prompts::task_accepted(&id).into_bytes())
    }
}

async fn run_semantic_grep(
    cwd: PathBuf,
    root: PathBuf,
    glob: Option<String>,
    query: String,
    mode: SearchMode,
    limit: usize,
    context: ProtocolContext,
) -> Result<Vec<u8>> {
    let record = context
        .tasks
        .allocate("grep", format!("Search code under {}", display_path(&root)))
        .await;
    match context
        .tasks
        .run_with_auto_background(
            record,
            AUTO_BACKGROUND_AFTER,
            move |cancellation| async move {
                semantic_grep(
                    &cwd,
                    &root,
                    glob.as_deref(),
                    &query,
                    mode,
                    limit,
                    cancellation,
                )
                .await
            },
        )
        .await?
    {
        AutoTask::Background(id) => Ok(prompts::task_accepted(&id).into_bytes()),
        AutoTask::Terminal(record) => record.terminal_result("semantic grep"),
    }
}

async fn semantic_grep(
    cwd: &Path,
    root: &Path,
    glob: Option<&str>,
    query: &str,
    mode: SearchMode,
    limit: usize,
    cancellation: CancellationToken,
) -> Result<Vec<u8>> {
    for _ in 0..MAX_INDEX_RETRIES {
        let corpus = code_corpus(cwd, root, glob).await?;
        let checkpoint = index_checkpoint(&corpus.spec).await?;
        let sources = corpus.catalog.changed_sources(&checkpoint);
        let snapshot = match corpus.load_sources(sources, cancellation.clone()).await {
            Ok(snapshot) => snapshot,
            Err(error) => {
                if code_corpus(cwd, root, glob).await?.catalog != corpus.catalog {
                    continue;
                }
                return Err(error);
            }
        };
        if !sync_index(
            &corpus.spec,
            &corpus.catalog,
            snapshot,
            cancellation.clone(),
        )
        .await?
        {
            continue;
        }
        if code_corpus(cwd, root, glob).await?.catalog != corpus.catalog {
            continue;
        }
        let hits = search_index(
            &corpus.spec,
            &corpus.catalog,
            query,
            mode,
            limit,
            SearchFilter::default(),
            cancellation.clone(),
        )
        .await?;
        if code_corpus(cwd, root, glob).await?.catalog == corpus.catalog {
            return Ok(format_semantic_results(&hits, mode).into_bytes());
        }
    }
    bail!("code changed repeatedly while preparing semantic search; retry the read")
}

async fn rebuild_code_index(
    cwd: &Path,
    root: &Path,
    glob: Option<&str>,
    cancellation: CancellationToken,
) -> Result<Vec<u8>> {
    for _ in 0..MAX_INDEX_RETRIES {
        let corpus = code_corpus(cwd, root, glob).await?;
        let snapshot = corpus.load_all(cancellation.clone()).await?;
        let status = rebuild_index(&corpus.spec, snapshot, cancellation.clone()).await?;
        if code_corpus(cwd, root, glob).await?.catalog == corpus.catalog {
            return Ok(status.format("Code").into_bytes());
        }
    }
    bail!("code changed repeatedly while rebuilding the semantic index")
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GrepMode {
    Exact,
    Semantic(SearchMode),
    Status,
}

#[derive(Debug, Eq, PartialEq)]
struct GrepOptions {
    mode: GrepMode,
    glob: Option<String>,
    literal: bool,
    ignore_case: bool,
    context: usize,
    limit: usize,
    limit_set: bool,
}

impl GrepOptions {
    fn parse(query: Option<&str>) -> Result<Self> {
        let mut options = Self {
            mode: GrepMode::Exact,
            glob: None,
            literal: false,
            ignore_case: false,
            context: 0,
            limit: DEFAULT_LIMIT,
            limit_set: false,
        };
        let mut seen = std::collections::HashSet::new();
        for pair in query
            .unwrap_or_default()
            .split('&')
            .filter(|pair| !pair.is_empty())
        {
            let (name, value) = pair
                .split_once('=')
                .ok_or_else(|| anyhow!("invalid grep query component: {pair}"))?;
            if !seen.insert(name) {
                bail!("duplicate grep query parameter: {name}");
            }
            match name {
                "mode" => {
                    options.mode = match value {
                        "exact" => GrepMode::Exact,
                        "semantic" | "hybrid" => {
                            GrepMode::Semantic(SearchMode::parse(value, "grep")?)
                        }
                        "status" => GrepMode::Status,
                        "index" => bail!("grep mode=index is available only through exec"),
                        _ => {
                            bail!("grep mode must be exact, semantic, hybrid, or status for reads")
                        }
                    }
                }
                "glob" if !value.is_empty() => options.glob = Some(value.to_string()),
                "glob" => bail!("grep glob cannot be empty"),
                "literal" => options.literal = parse_bool(name, value)?,
                "ignore_case" => options.ignore_case = parse_bool(name, value)?,
                "context" => {
                    options.context = value
                        .parse::<usize>()
                        .with_context(|| format!("invalid grep context: {value}"))?;
                    if options.context > MAX_CONTEXT {
                        bail!("grep context cannot exceed {MAX_CONTEXT}");
                    }
                }
                "limit" => {
                    options.limit = value
                        .parse::<usize>()
                        .with_context(|| format!("invalid grep limit: {value}"))?;
                    if !(1..=MAX_LIMIT).contains(&options.limit) {
                        bail!("grep limit must be between 1 and {MAX_LIMIT}");
                    }
                    options.limit_set = true;
                }
                _ => bail!("unknown grep query parameter: {name}"),
            }
        }
        Ok(options)
    }

    fn parse_exec(query: Option<&str>) -> Result<Self> {
        let Some(query) = query else {
            bail!("grep exec requires mode=index");
        };
        let rewritten = query
            .split('&')
            .map(|pair| {
                if pair == "mode=index" {
                    "mode=status"
                } else {
                    pair
                }
            })
            .collect::<Vec<_>>()
            .join("&");
        if !query.split('&').any(|pair| pair == "mode=index") {
            bail!("grep exec requires mode=index");
        }
        let mut options = Self::parse(Some(&rewritten))?;
        options.mode = GrepMode::Status;
        Ok(options)
    }

    fn validate_semantic(&self) -> Result<()> {
        if self.literal || self.ignore_case || self.context != 0 {
            bail!("semantic grep accepts only mode, glob, and limit");
        }
        if self.limit_set && self.limit > MAX_SEMANTIC_LIMIT {
            bail!("semantic grep limit cannot exceed {MAX_SEMANTIC_LIMIT}");
        }
        Ok(())
    }

    fn validate_index_operation(&self) -> Result<()> {
        if self.literal || self.ignore_case || self.context != 0 || self.limit_set {
            bail!("grep semantic index operations accept only mode and glob");
        }
        Ok(())
    }

    fn semantic_limit(&self) -> usize {
        if self.limit_set { self.limit } else { 7 }
    }
}

fn require_search_body(body: &str, uri: &str) -> Result<()> {
    if body.is_empty() {
        bail!(
            "grep requires a nonempty search pattern in the body; use read({uri:?}, \"<pattern>\")"
        );
    }
    Ok(())
}

async fn validate_root(resolved: &Path) -> Result<()> {
    let metadata = tokio::fs::metadata(resolved)
        .await
        .with_context(|| format!("cannot search {}", display_path(resolved)))?;
    if !metadata.is_dir() && !metadata.is_file() {
        bail!(
            "grep root is not a regular file or directory: {}",
            display_path(resolved)
        );
    }
    Ok(())
}

fn format_semantic_results(hits: &[SearchHit], mode: SearchMode) -> String {
    if hits.is_empty() {
        return "No matches.\n".to_string();
    }
    let mut output = format!("Code {} search · ranked results\n", mode.label());
    for hit in hits {
        output.push_str(&format!("\n{}\n", hit.label));
        output.push_str(hit.text.trim_end());
        output.push('\n');
    }
    output
}

fn parse_bool(name: &str, value: &str) -> Result<bool> {
    match value {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => bail!("grep {name} must be true or false"),
    }
}

fn grep_root_argument(cwd: &Path, root: &str, resolved: &Path) -> PathBuf {
    if root.is_empty() {
        return PathBuf::from(".");
    }
    let original = Path::new(root);
    if cwd.join(original) == resolved {
        original.to_path_buf()
    } else {
        resolved.to_path_buf()
    }
}

async fn run_grep(
    executable: &Path,
    cwd: &Path,
    root: &Path,
    pattern: &str,
    options: &GrepOptions,
) -> Result<String> {
    let mut fixed_strings = options.literal;
    loop {
        let result = run_grep_once(executable, cwd, root, pattern, options, fixed_strings)
            .await
            .context("grep failed")?;
        if !fixed_strings && result.regex_parse_failed() {
            fixed_strings = true;
            continue;
        }
        return result.into_output(options.limit);
    }
}

struct GrepRun {
    output: String,
    matches: usize,
    truncated: bool,
    status: std::process::ExitStatus,
    stderr: Vec<u8>,
}

impl GrepRun {
    fn regex_parse_failed(&self) -> bool {
        self.matches == 0
            && !self.truncated
            && !matches!(self.status.code(), Some(0 | 1))
            && String::from_utf8_lossy(&self.stderr).contains("regex parse error:")
    }

    fn into_output(mut self, limit: usize) -> Result<String> {
        if !self.truncated && !matches!(self.status.code(), Some(0 | 1)) {
            let message = String::from_utf8_lossy(&self.stderr);
            let message = message.trim();
            let message = message.strip_prefix("rg: ").unwrap_or(message);
            bail!(
                "grep failed{}",
                if message.is_empty() {
                    String::new()
                } else {
                    format!(": {message}")
                }
            );
        }
        if self.matches == 0 {
            return Ok("No matches.\n".to_string());
        }
        if self.truncated {
            self.output
                .push_str(&format!("\n[match limit reached: {limit}]\n"));
        }
        Ok(self.output)
    }
}

async fn run_grep_once(
    executable: &Path,
    cwd: &Path,
    root: &Path,
    pattern: &str,
    options: &GrepOptions,
    fixed_strings: bool,
) -> Result<GrepRun> {
    let mut command = Command::new(executable);
    command
        .current_dir(cwd)
        .arg("--json")
        .arg("--no-config")
        .arg("--color=never")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(glob) = &options.glob {
        command.arg("--glob").arg(glob);
    }
    if fixed_strings {
        command.arg("--fixed-strings");
    }
    if options.ignore_case {
        command.arg("--ignore-case");
    }
    if options.context > 0 {
        command.arg("--context").arg(options.context.to_string());
    }
    command.arg("--").arg(pattern).arg(root);
    let mut child = command.spawn().context("cannot start search process")?;
    let stdout = child.stdout.take().expect("grep stdout is piped");
    let mut stderr = child.stderr.take().expect("grep stderr is piped");
    let stderr_task = tokio::spawn(async move {
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).await.map(|_| bytes)
    });
    let mut lines = BufReader::new(stdout).lines();
    let mut output = String::new();
    let mut matches = 0usize;
    let mut trailing_context = None;
    let mut truncated = false;
    while let Some(line) = lines.next_line().await? {
        let event: Value =
            serde_json::from_str(&line).context("search process returned invalid JSON")?;
        let kind = event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !matches!(kind, "match" | "context") {
            if trailing_context.is_some() && kind == "end" {
                truncated = true;
                break;
            }
            continue;
        }
        if kind == "match" {
            if matches >= options.limit {
                truncated = true;
                break;
            }
            matches += 1;
            if matches == options.limit {
                trailing_context = Some(options.context);
            }
        } else if let Some(remaining) = &mut trailing_context {
            if *remaining == 0 {
                truncated = true;
                break;
            }
            *remaining -= 1;
        }
        append_event(&mut output, &event, kind == "match")?;
        if trailing_context == Some(0) {
            truncated = true;
            break;
        }
    }
    if truncated {
        child
            .kill()
            .await
            .context("failed to stop bounded grep search")?;
    }
    let status = child.wait().await?;
    let stderr = stderr_task.await.context("grep stderr reader failed")??;
    Ok(GrepRun {
        output,
        matches,
        truncated,
        status,
        stderr,
    })
}

fn append_event(output: &mut String, event: &Value, is_match: bool) -> Result<()> {
    let data = event
        .get("data")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("grep event has no data object"))?;
    let path = data
        .get("path")
        .and_then(|path| path.get("text"))
        .and_then(Value::as_str)
        .unwrap_or("<non-UTF-8 path>")
        .trim_start_matches("./");
    let path = display_path(Path::new(path));
    let line = data
        .get("line_number")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let text = data
        .get("lines")
        .and_then(|lines| lines.get("text"))
        .and_then(Value::as_str)
        .unwrap_or("<non-UTF-8 line>")
        .trim_end_matches(['\r', '\n']);
    output.push_str(&path);
    output.push(if is_match { ':' } else { '-' });
    output.push_str(&line.to_string());
    output.push(if is_match { ':' } else { '-' });
    output.push_str(text);
    output.push('\n');
    Ok(())
}

fn ripgrep_download() -> Result<BinaryDownload> {
    let (asset, sha256, archive, executable) = match (std::env::consts::OS, std::env::consts::ARCH)
    {
        ("linux", "x86_64") => (
            "ripgrep-14.1.1-x86_64-unknown-linux-musl.tar.gz",
            "4cf9f2741e6c465ffdb7c26f38056a59e2a2544b51f7cc128ef28337eeae4d8e",
            DownloadArchive::TarGz,
            "ripgrep-14.1.1-x86_64-unknown-linux-musl/rg",
        ),
        ("linux", "aarch64") => (
            "ripgrep-14.1.1-aarch64-unknown-linux-gnu.tar.gz",
            "c827481c4ff4ea10c9dc7a4022c8de5db34a5737cb74484d62eb94a95841ab2f",
            DownloadArchive::TarGz,
            "ripgrep-14.1.1-aarch64-unknown-linux-gnu/rg",
        ),
        ("macos", "x86_64") => (
            "ripgrep-14.1.1-x86_64-apple-darwin.tar.gz",
            "fc87e78f7cb3fea12d69072e7ef3b21509754717b746368fd40d88963630e2b3",
            DownloadArchive::TarGz,
            "ripgrep-14.1.1-x86_64-apple-darwin/rg",
        ),
        ("macos", "aarch64") => (
            "ripgrep-14.1.1-aarch64-apple-darwin.tar.gz",
            "24ad76777745fbff131c8fbc466742b011f925bfa4fffa2ded6def23b5b937be",
            DownloadArchive::TarGz,
            "ripgrep-14.1.1-aarch64-apple-darwin/rg",
        ),
        ("windows", "x86_64") => (
            "ripgrep-14.1.1-x86_64-pc-windows-msvc.zip",
            "d0f534024c42afd6cb4d38907c25cd2b249b79bbe6cc1dbee8e3e37c2b6e25a1",
            DownloadArchive::Zip,
            "ripgrep-14.1.1-x86_64-pc-windows-msvc/rg.exe",
        ),
        (os, arch) => bail!("automatic ripgrep installation is unsupported on {os}/{arch}"),
    };
    Ok(BinaryDownload {
        name: "ripgrep",
        version: RIPGREP_VERSION,
        url: format!(
            "https://github.com/BurntSushi/ripgrep/releases/download/{RIPGREP_VERSION}/{asset}"
        ),
        sha256,
        archive,
        archive_path: executable,
        executable_name: "rg",
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::TaskManager;

    fn rg_on_path() -> Option<PathBuf> {
        let executable = if cfg!(windows) { "rg.exe" } else { "rg" };
        let paths = std::env::var_os("PATH")?;
        std::env::split_paths(&paths).find_map(|path| {
            let candidate = path.join(executable);
            candidate.is_file().then_some(candidate)
        })
    }

    #[test]
    fn grep_options_are_typed_bounded_and_reject_duplicates() {
        assert_eq!(
            GrepOptions::parse(Some(
                "glob=**/*.rs&literal=true&ignore_case=true&context=2&limit=10"
            ))
            .unwrap(),
            GrepOptions {
                mode: GrepMode::Exact,
                glob: Some("**/*.rs".to_string()),
                literal: true,
                ignore_case: true,
                context: 2,
                limit: 10,
                limit_set: true,
            }
        );
        assert_eq!(
            GrepOptions::parse(Some("mode=semantic"))
                .unwrap()
                .semantic_limit(),
            7
        );
        assert!(
            GrepOptions::parse(Some("mode=hybrid&limit=51"))
                .unwrap()
                .validate_semantic()
                .is_err()
        );
        assert!(GrepOptions::parse_exec(Some("mode=index&glob=**/*.rs")).is_ok());
        assert!(GrepOptions::parse_exec(Some("mode=semantic")).is_err());
        assert!(
            GrepOptions::parse_exec(Some("mode=index&limit=1"))
                .unwrap()
                .validate_index_operation()
                .is_err()
        );
        assert!(GrepOptions::parse(Some("context=21")).is_err());
        assert!(GrepOptions::parse(Some("limit=0")).is_err());
        assert!(GrepOptions::parse(Some("literal=true&literal=false")).is_err());
    }

    #[test]
    fn grep_uses_resolved_home_paths_but_preserves_ordinary_relative_roots() {
        let cwd = Path::new("/project");

        assert_eq!(
            grep_root_argument(cwd, "src", Path::new("/project/src")),
            Path::new("src")
        );
        assert_eq!(
            grep_root_argument(cwd, "~/notes", Path::new("/home/ada/notes")),
            Path::new("/home/ada/notes")
        );
        assert_eq!(
            grep_root_argument(cwd, "", Path::new("/project")),
            Path::new(".")
        );
    }

    #[tokio::test]
    async fn grep_help_distinguishes_empty_root_from_nonempty_body() {
        let directory = tempfile::tempdir().unwrap();
        let protocol = GrepProtocol::new(directory.path());
        let help = protocol
            .read(
                ProtocolRequest {
                    uri: "grep://help",
                    target: "help",
                    body: "",
                },
                ProtocolContext {
                    tasks: TaskManager::new(),
                },
            )
            .await
            .unwrap();
        let help = String::from_utf8(help).unwrap();
        assert!(help.contains("MUST pass a nonempty search pattern"));
        assert!(help.contains("The root\nmay be empty"));
        assert!(help.contains("paths beginning with `~/` resolve"));
        assert!(help.contains("`~user` is not expanded"));
        assert!(help.contains("retries it as literal text"));
        assert!(help.contains(r#"read("grep://src/tui/app.rs", "fn push(")"#));
        assert!(help.contains("Prefer it for conceptual\n  searches"));
        assert!(help.contains("Do not call status or index before a ranked search"));
        assert!(help.contains("continues as one managed task\nwithout restarting"));
        assert!(help.contains("`grep://help` MUST use an empty string body"));

        let error = protocol
            .read(
                ProtocolRequest {
                    uri: "grep://",
                    target: "",
                    body: "",
                },
                ProtocolContext {
                    tasks: TaskManager::new(),
                },
            )
            .await
            .unwrap_err();
        let error = error.to_string();
        assert!(error.contains("nonempty search pattern"));
        assert!(error.contains(r#"read("grep://", "<pattern>")"#));
    }

    #[test]
    fn ranked_grep_results_keep_order_and_anchors_without_raw_scores() {
        let output = format_semantic_results(
            &[SearchHit {
                source: "src/auth.rs".to_string(),
                label: "src/auth.rs:42-56".to_string(),
                text: "fn refresh_credentials() {}".to_string(),
                record_type: String::new(),
            }],
            SearchMode::Hybrid,
        );

        assert!(output.starts_with("Code hybrid search · ranked results"));
        assert!(output.contains("src/auth.rs:42-56\nfn refresh_credentials() {}"));
        assert!(!output.contains("score="));
    }

    #[tokio::test]
    async fn grep_searches_without_a_shell_and_honors_glob_ignore_and_limit() {
        let directory = tempfile::tempdir().unwrap();
        tokio::fs::create_dir_all(directory.path().join("nested"))
            .await
            .unwrap();
        tokio::fs::write(directory.path().join("one.rs"), "Alpha needle\nsecond\n")
            .await
            .unwrap();
        tokio::fs::write(directory.path().join("nested/two.rs"), "needle two\n")
            .await
            .unwrap();
        tokio::fs::write(directory.path().join("nested/no.txt"), "needle text\n")
            .await
            .unwrap();
        tokio::fs::write(directory.path().join(".ignore"), "nested/\n")
            .await
            .unwrap();
        let protocol =
            GrepProtocol::new(directory.path()).with_downloads(PluginDownloads::for_test());
        let output = protocol
            .read(
                ProtocolRequest {
                    uri: "grep://?glob=**/*.rs&ignore_case=true&context=1&limit=1",
                    target: "?glob=**/*.rs&ignore_case=true&context=1&limit=1",
                    body: "needle",
                },
                ProtocolContext {
                    tasks: TaskManager::new(),
                },
            )
            .await
            .unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("one.rs:1:Alpha needle"));
        assert!(output.contains("one.rs-2-second"));
        assert!(!output.contains("two.rs"));
        assert!(!output.contains("no.txt"));
    }

    #[tokio::test]
    async fn grep_preserves_regex_and_literal_modes_and_retries_invalid_regex_as_literal() {
        let Some(rg) = rg_on_path() else {
            return;
        };
        let directory = tempfile::tempdir().unwrap();
        tokio::fs::write(directory.path().join("values.txt"), "a.b\naxb\nfn push(\n")
            .await
            .unwrap();
        let default_options = GrepOptions::parse(None).unwrap();

        assert_eq!(
            run_grep(
                &rg,
                directory.path(),
                Path::new("."),
                "missing",
                &default_options,
            )
            .await
            .unwrap(),
            "No matches.\n"
        );
        let output = run_grep(
            &rg,
            directory.path(),
            Path::new("."),
            "a.b",
            &default_options,
        )
        .await
        .unwrap();
        assert!(output.contains("values.txt:1:a.b"));
        assert!(output.contains("values.txt:2:axb"));

        let literal = GrepOptions {
            literal: true,
            ..GrepOptions::parse(None).unwrap()
        };
        let output = run_grep(&rg, directory.path(), Path::new("."), "a.b", &literal)
            .await
            .unwrap();
        assert!(output.contains("values.txt:1:a.b"));
        assert!(!output.contains("values.txt:2:axb"));

        let output = run_grep(
            &rg,
            directory.path(),
            Path::new("."),
            "fn push(",
            &default_options,
        )
        .await
        .unwrap();
        assert!(output.contains("values.txt:3:fn push("));

        let error = run_grep(
            &rg,
            directory.path(),
            Path::new("missing-root"),
            "needle",
            &default_options,
        )
        .await
        .unwrap_err();
        let error = format!("{error:#}");
        assert!(error.contains("grep failed"));
        assert!(!error.contains("ripgrep"));
        assert!(!error.contains("rg:"));
    }
}
