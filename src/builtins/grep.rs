use super::file::resolve_path;
use crate::config::display_path;
use crate::plugin::{
    BinaryDownload, DownloadArchive, Plugin, PluginDownloads, PluginHost, PluginPermission,
};
use crate::prompts;
use crate::protocol::{
    Protocol, ProtocolContext, ProtocolDescriptor, ProtocolOutput, ProtocolRequest,
};
use crate::retrieval::{
    SearchFilter, SearchHit, SearchMode, code_corpus, index_status, rebuild_live_corpus,
    search_live_corpus,
};
use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

const DEFAULT_LIMIT: usize = 200;
const MAX_LIMIT: usize = 2_000;
const MAX_SEMANTIC_LIMIT: usize = 50;
const DEFAULT_SEMANTIC_LIMIT: usize = 7;
const MAX_CONTEXT: usize = 20;
const RIPGREP_VERSION: &str = "14.1.1";
const SEARCH_SCHEME: &str = "search";

fn help(cwd: &Path, scheme: &str) -> String {
    format!(
        r#"# {scheme}

Search file contents with ripgrep (`rg`) or on-demand semantic and hybrid
retrieval.

Current working directory: `{scheme}://{}`

Search reads other than `mode: "status"` MUST pass a nonempty `query` string;
`mode: "status"` takes no `query`. Use `{scheme}://<root>` for a
project-relative or absolute file/directory root. The root may be empty:
`{scheme}://` searches the current working directory. On Unix, `~` and paths
beginning with `~/` resolve from the current user's home directory; `~user` is
not expanded.

Read input fields (every field except `query` is optional):

- `query` (string, required except for `mode: "status"`): the search pattern.
- `mode` (string): `"exact"` (the default) uses ripgrep (`rg`). Patterns use
  `rg` regular-expression syntax unless `literal` is set. Use this mode for
  known identifiers, paths, syntax, or literal wording. Exact results are
  `path:line:text` lines; an output ending with `[match limit reached: <limit>]`
  covers only the first `limit` matches. `"hybrid"` combines keyword and
  semantic ranking; prefer it for conceptual searches. `"semantic"` prioritizes
  meaning over shared wording; use it when relevant results are likely to use
  different wording from the query. `"status"` reports whether the selected
  root's semantic cache is current.
- `glob` (string): filters searched paths using `rg` glob syntax.
- `literal` (boolean): always use `rg` literal matching. If `rg` rejects a
  regular expression, the read retries it as literal text anyway.
- `ignore_case` (boolean): enable case-insensitive matching.
- `context` (integer): include surrounding lines; values are clamped to 0
  through 20.
- `limit` (integer): bound the number of matches; the default is 200 and
  values are clamped to 1 through 2,000.

Semantic and hybrid reads accept only `query`, `mode`, `glob`, and `limit`;
their default limit is 7 and values are clamped to 1 through 50. A ranked read
creates or incrementally refreshes its selected root/glob cache as needed, then
searches it. Most searches return in the same call; a longer search continues
as one managed task without restarting and delivers its result automatically.
If completion marks the output as truncated, follow its `tasks://` instruction
once. Do not submit the same search again to retrieve task output.

Do not call status or index before a ranked search. Use `mode: "status"` only
to diagnose the cache. Use an exec step only to prewarm or force-rebuild that
exact root/glob cache; it takes `mode: "index"` (required) and an optional
`glob`, and nothing else:

{{"exec": "{scheme}://src", "input": {{"mode": "index", "glob": "**/*.rs"}}}}

Indexing follows standard ignore files, skips binary/non-UTF-8 files and files
larger than 1 MiB, and chunks readable text into line-ranged fragments. Results
show the actual matching fragment with its precise line range.

Examples:

{{"read": "{scheme}://src", "input": {{"glob": "**/*.rs", "limit": 100, "query": "ProtocolRequest"}}}}

{{"read": "{scheme}://src/tui/app.rs", "input": {{"query": "fn push("}}}}

{{"read": "{scheme}://", "input": {{"literal": true, "ignore_case": true, "query": "exact text"}}}}

{{"read": "{scheme}://src", "input": {{"mode": "hybrid", "glob": "**/*.rs", "limit": 10, "query": "authentication flow"}}}}

{{"read": "{scheme}://src", "input": {{"mode": "status"}}}}
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

    async fn run_semantic_grep(
        &self,
        root: PathBuf,
        glob: Option<String>,
        query: String,
        mode: SearchMode,
        limit: usize,
        context: ProtocolContext,
    ) -> Result<ProtocolOutput> {
        let cwd = self.cwd.clone();
        context
            .run_auto_background(
                SEARCH_SCHEME,
                format!("Search code under {}", display_path(&root)),
                "semantic search",
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
            .await
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
            name: SEARCH_SCHEME.to_string(),
            description: "Search file contents with ripgrep (`rg`) or on-demand semantic and hybrid retrieval.".to_string(),
            can_read: true,
            can_exec: true,
        }
    }

    async fn read(
        &self,
        request: ProtocolRequest<'_>,
        context: ProtocolContext,
    ) -> Result<ProtocolOutput> {
        if request.target == "help" {
            request.reject_input()?;
            return Ok(help(&self.cwd, SEARCH_SCHEME).into());
        }
        let root = request.target;
        let input = request.input_struct::<ReadInput>()?;
        let options = GrepOptions::from_read_input(&input, SEARCH_SCHEME)?;
        let resolved = resolve_path(&self.cwd, root)?;
        validate_root(&resolved, SEARCH_SCHEME).await?;
        match options.mode {
            GrepMode::Exact => {
                require_query(&input, request.uri)?;
                let downloads = self.downloads.as_ref().ok_or_else(|| {
                    anyhow!("{} binary download access is not attached", SEARCH_SCHEME)
                })?;
                let rg = downloads.ensure(&ripgrep_download()?).await?;
                run_grep(
                    &rg,
                    &self.cwd,
                    &grep_root_argument(&self.cwd, root, &resolved),
                    input.query.as_deref().unwrap_or_default(),
                    &options,
                    SEARCH_SCHEME,
                )
                .await
                .map(String::into_bytes)
                .map(ProtocolOutput::from)
            }
            GrepMode::Semantic(mode) => {
                require_query(&input, request.uri)?;
                validate_semantic_input(&input, SEARCH_SCHEME)?;
                self.run_semantic_grep(
                    resolved,
                    options.glob.clone(),
                    input.query.unwrap_or_default(),
                    mode,
                    options.semantic_limit(),
                    context,
                )
                .await
            }
            GrepMode::Status => {
                validate_status_input(&input, SEARCH_SCHEME)?;
                let corpus = code_corpus(&self.cwd, &resolved, options.glob.as_deref()).await?;
                Ok(index_status(&corpus.spec, &corpus.catalog)
                    .await?
                    .format("Code")
                    .into_bytes()
                    .into())
            }
        }
    }

    async fn exec(
        &self,
        request: ProtocolRequest<'_>,
        context: ProtocolContext,
    ) -> Result<ProtocolOutput> {
        let root = request.target;
        let input = request.input_struct::<ExecInput>()?;
        if input.mode != "index" {
            bail!(
                "{SEARCH_SCHEME} exec requires {{\"mode\": \"index\"}}; correct form:\n\
                 {{\"exec\": {}, \"input\": {{\"mode\": \"index\"}}}}",
                Value::from(request.uri)
            );
        }
        if let Some(glob) = &input.glob
            && glob.is_empty()
        {
            bail!("{SEARCH_SCHEME} glob cannot be empty");
        }
        let resolved = resolve_path(&self.cwd, root)?;
        validate_root(&resolved, SEARCH_SCHEME).await?;
        let label = format!("Index code under {}", display_path(&resolved));
        let record = context
            .tasks
            .allocate_background(SEARCH_SCHEME, label)
            .await?;
        let id = record.id.clone();
        let cwd = self.cwd.clone();
        let glob = input.glob.clone();
        context
            .tasks
            .spawn_with_cancellation(record, move |cancellation| async move {
                rebuild_live_corpus(
                    || code_corpus(&cwd, &resolved, glob.as_deref()),
                    "Code",
                    "code changed repeatedly while rebuilding the semantic index",
                    cancellation,
                )
                .await
            })
            .await;
        Ok(prompts::task_accepted(&id).into())
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
    let (_corpus, hits) = search_live_corpus(
        || code_corpus(cwd, root, glob),
        query,
        mode,
        limit,
        SearchFilter::default(),
        "code changed repeatedly while preparing semantic search; retry the read",
        cancellation,
    )
    .await?;
    Ok(format_semantic_results(&hits, mode).into_bytes())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GrepMode {
    Exact,
    Semantic(SearchMode),
    Status,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadInput {
    query: Option<String>,
    mode: Option<String>,
    glob: Option<String>,
    literal: Option<bool>,
    ignore_case: Option<bool>,
    context: Option<usize>,
    limit: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExecInput {
    mode: String,
    glob: Option<String>,
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
    fn from_read_input(input: &ReadInput, scheme: &str) -> Result<Self> {
        let mode = match input.mode.as_deref() {
            None | Some("exact") => GrepMode::Exact,
            Some("semantic") => GrepMode::Semantic(SearchMode::parse("semantic", scheme)?),
            Some("hybrid") => GrepMode::Semantic(SearchMode::parse("hybrid", scheme)?),
            Some("status") => GrepMode::Status,
            Some("index") => bail!("{scheme} `\"mode\": \"index\"` is available only through exec"),
            Some(other) => bail!(
                "{scheme} mode must be exact, semantic, hybrid, or status for reads, got {other:?}"
            ),
        };
        if let Some(glob) = &input.glob
            && glob.is_empty()
        {
            bail!("{scheme} glob cannot be empty");
        }
        Ok(Self {
            mode,
            glob: input.glob.clone(),
            literal: input.literal.unwrap_or(false),
            ignore_case: input.ignore_case.unwrap_or(false),
            context: input.context.unwrap_or(0).min(MAX_CONTEXT),
            limit: input.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT),
            limit_set: input.limit.is_some(),
        })
    }

    fn semantic_limit(&self) -> usize {
        if self.limit_set {
            self.limit.min(MAX_SEMANTIC_LIMIT)
        } else {
            DEFAULT_SEMANTIC_LIMIT
        }
    }
}

fn require_query(input: &ReadInput, uri: &str) -> Result<()> {
    if input.query.as_deref().is_none_or(|query| query.is_empty()) {
        bail!(
            "search requires a nonempty `query`; correct form:\n\
             {{\"read\": {}, \"input\": {{\"query\": \"<pattern>\"}}}}",
            Value::from(uri)
        );
    }
    Ok(())
}

fn validate_semantic_input(input: &ReadInput, scheme: &str) -> Result<()> {
    if input.literal.is_some() || input.ignore_case.is_some() || input.context.is_some() {
        bail!("semantic {scheme} accepts only query, mode, glob, and limit");
    }
    Ok(())
}

fn validate_status_input(input: &ReadInput, scheme: &str) -> Result<()> {
    if input.query.is_some() {
        bail!("{scheme} `\"mode\": \"status\"` takes no query");
    }
    if input.literal.is_some()
        || input.ignore_case.is_some()
        || input.context.is_some()
        || input.limit.is_some()
    {
        bail!("{scheme} semantic index operations accept only mode and glob");
    }
    Ok(())
}

async fn validate_root(resolved: &Path, scheme: &str) -> Result<()> {
    let metadata = tokio::fs::metadata(resolved)
        .await
        .with_context(|| format!("cannot search {}", display_path(resolved)))?;
    if !metadata.is_dir() && !metadata.is_file() {
        bail!(
            "{scheme} root is not a regular file or directory: {}",
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
    scheme: &str,
) -> Result<String> {
    let mut fixed_strings = options.literal;
    loop {
        let result = run_grep_once(
            executable,
            cwd,
            root,
            pattern,
            options,
            fixed_strings,
            scheme,
        )
        .await
        .with_context(|| format!("{scheme} failed"))?;
        if !fixed_strings && result.regex_parse_failed() {
            fixed_strings = true;
            continue;
        }
        return result.into_output(options.limit, scheme);
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

    fn into_output(mut self, limit: usize, scheme: &str) -> Result<String> {
        if !self.truncated && !matches!(self.status.code(), Some(0 | 1)) {
            let message = String::from_utf8_lossy(&self.stderr);
            let message = message.trim();
            let message = message.strip_prefix("rg: ").unwrap_or(message);
            bail!(
                "{scheme} failed{}",
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
    scheme: &str,
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
        append_event(&mut output, &event, kind == "match", scheme)?;
        if trailing_context == Some(0) {
            truncated = true;
            break;
        }
    }
    if truncated {
        child
            .kill()
            .await
            .with_context(|| format!("failed to stop bounded {scheme} search"))?;
    }
    let status = child.wait().await?;
    let stderr = stderr_task
        .await
        .with_context(|| format!("{scheme} stderr reader failed"))??;
    Ok(GrepRun {
        output,
        matches,
        truncated,
        status,
        stderr,
    })
}

fn append_event(output: &mut String, event: &Value, is_match: bool, scheme: &str) -> Result<()> {
    let data = event
        .get("data")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("{scheme} event has no data object"))?;
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
    use serde_json::json;

    fn rg_on_path() -> Option<PathBuf> {
        let executable = if cfg!(windows) { "rg.exe" } else { "rg" };
        let paths = std::env::var_os("PATH")?;
        std::env::split_paths(&paths).find_map(|path| {
            let candidate = path.join(executable);
            candidate.is_file().then_some(candidate)
        })
    }

    fn read_input(value: serde_json::Value) -> ReadInput {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn grep_options_are_typed_and_bounded() {
        assert_eq!(
            GrepOptions::from_read_input(
                &read_input(json!({
                    "glob": "**/*.rs",
                    "literal": true,
                    "ignore_case": true,
                    "context": 2,
                    "limit": 10,
                })),
                SEARCH_SCHEME
            )
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
            GrepOptions::from_read_input(&read_input(json!({"mode": "semantic"})), SEARCH_SCHEME)
                .unwrap()
                .semantic_limit(),
            7
        );
        let hybrid = GrepOptions::from_read_input(
            &read_input(json!({"mode": "hybrid", "limit": 51})),
            SEARCH_SCHEME,
        )
        .unwrap();
        validate_semantic_input(
            &read_input(json!({"mode": "hybrid", "limit": 51})),
            SEARCH_SCHEME,
        )
        .unwrap();
        assert_eq!(hybrid.semantic_limit(), 50);
        assert_eq!(
            GrepOptions::from_read_input(&read_input(json!({"context": 21})), SEARCH_SCHEME)
                .unwrap()
                .context,
            20
        );
        assert_eq!(
            GrepOptions::from_read_input(&read_input(json!({"limit": 0})), SEARCH_SCHEME)
                .unwrap()
                .limit,
            1
        );
        assert_eq!(
            GrepOptions::from_read_input(&read_input(json!({"limit": 99999})), SEARCH_SCHEME)
                .unwrap()
                .limit,
            2_000
        );
    }

    #[test]
    fn grep_rejects_malformed_input() {
        assert!(serde_json::from_value::<ReadInput>(json!({"limit": ">=10"})).is_err());
        assert!(serde_json::from_value::<ReadInput>(json!({"limit": "10"})).is_err());
        assert!(serde_json::from_value::<ReadInput>(json!({"literal": "true"})).is_err());
        assert!(serde_json::from_value::<ReadInput>(json!({"query": 5})).is_err());
        assert!(serde_json::from_value::<ReadInput>(json!({"mode": 1})).is_err());
        assert!(
            serde_json::from_value::<ReadInput>(json!({"unknown": true})).is_err(),
            "unknown fields must be rejected"
        );
        assert!(serde_json::from_value::<ExecInput>(json!({})).is_err());
        assert!(serde_json::from_value::<ExecInput>(json!({"mode": "status"})).is_ok());
        let error =
            GrepOptions::from_read_input(&read_input(json!({"mode": "index"})), SEARCH_SCHEME)
                .unwrap_err();
        assert!(
            format!("{error:#}").contains("`\"mode\": \"index\"` is available only through exec")
        );
        let error =
            GrepOptions::from_read_input(&read_input(json!({"mode": "reverse"})), SEARCH_SCHEME)
                .unwrap_err();
        assert!(format!("{error:#}").contains("mode must be exact, semantic, hybrid, or status"));
    }

    #[test]
    fn grep_status_and_semantic_reject_extra_fields() {
        let error =
            validate_status_input(&read_input(json!({"limit": 1})), SEARCH_SCHEME).unwrap_err();
        assert!(format!("{error:#}").contains("accept only mode and glob"));
        let error =
            validate_semantic_input(&read_input(json!({"context": 2})), SEARCH_SCHEME).unwrap_err();
        assert!(format!("{error:#}").contains("accepts only query, mode, glob, and limit"));
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
    async fn search_help_documents_the_step_contract() {
        let directory = tempfile::tempdir().unwrap();
        let protocol = GrepProtocol::new(directory.path());
        assert_eq!(protocol.descriptor().name, SEARCH_SCHEME);
        let input = serde_json::Map::new();
        let help = protocol
            .read(
                ProtocolRequest {
                    uri: "search://help",
                    target: "help",
                    input: &input,
                },
                ProtocolContext::new(TaskManager::new()),
            )
            .await
            .unwrap();
        let help = String::from_utf8(help.text_bytes().to_vec()).unwrap();
        assert!(help.contains("MUST pass a nonempty `query` string"));
        assert!(help.contains("The root may be empty"));
        assert!(help.contains("paths\nbeginning with `~/` resolve"));
        assert!(help.contains("`~user` is\nnot expanded"));
        assert!(help.contains("uses ripgrep (`rg`)"));
        assert!(help.contains("the read retries it as literal text anyway"));
        assert!(
            help.contains(r#"{"read": "search://src/tui/app.rs", "input": {"query": "fn push("}}"#)
        );
        assert!(!help.contains("grep://"));
        assert!(help.contains("prefer it for conceptual searches"));
        assert!(help.contains("values are clamped to 0\n  through 20"));
        assert!(help.contains("values are clamped to 1 through 2,000"));
        assert!(help.contains("clamped to 1 through 50"));
        assert!(help.contains("Do not call status or index before a ranked search"));
        assert!(help.contains("continues\nas one managed task without restarting"));
        assert!(help.contains(
            r#"{"exec": "search://src", "input": {"mode": "index", "glob": "**/*.rs"}}"#
        ));
        assert!(!help.contains("header"));
        assert!(!help.contains("body"));

        let input = serde_json::from_value(json!({})).unwrap();
        let error = protocol
            .read(
                ProtocolRequest {
                    uri: "search://",
                    target: "",
                    input: &input,
                },
                ProtocolContext::new(TaskManager::new()),
            )
            .await
            .unwrap_err();
        let error = error.to_string();
        assert!(error.contains("nonempty `query`"));
        assert!(error.contains(r#"{"read": "search://", "input": {"query": "<pattern>"}}"#));
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
        let input = serde_json::from_value(json!({
            "glob": "**/*.rs",
            "ignore_case": true,
            "context": 1,
            "limit": 1,
            "query": "needle",
        }))
        .unwrap();
        let output = protocol
            .read(
                ProtocolRequest {
                    uri: "search://",
                    target: "",
                    input: &input,
                },
                ProtocolContext::new(TaskManager::new()),
            )
            .await
            .unwrap();
        let output = String::from_utf8(output.text_bytes().to_vec()).unwrap();
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
        let default_options =
            GrepOptions::from_read_input(&read_input(json!({})), SEARCH_SCHEME).unwrap();

        assert_eq!(
            run_grep(
                &rg,
                directory.path(),
                Path::new("."),
                "missing",
                &default_options,
                SEARCH_SCHEME,
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
            SEARCH_SCHEME,
        )
        .await
        .unwrap();
        assert!(output.contains("values.txt:1:a.b"));
        assert!(output.contains("values.txt:2:axb"));

        let literal = GrepOptions {
            literal: true,
            ..GrepOptions::from_read_input(&read_input(json!({})), SEARCH_SCHEME).unwrap()
        };
        let output = run_grep(
            &rg,
            directory.path(),
            Path::new("."),
            "a.b",
            &literal,
            SEARCH_SCHEME,
        )
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
            SEARCH_SCHEME,
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
            SEARCH_SCHEME,
        )
        .await
        .unwrap_err();
        let error = format!("{error:#}");
        assert!(error.contains("search failed"));
        assert!(!error.contains("ripgrep"));
        assert!(!error.contains("rg:"));
    }
}
