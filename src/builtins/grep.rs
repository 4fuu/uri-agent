use super::file::resolve_path;
use crate::config::display_path;
use crate::plugin::{
    BinaryDownload, DownloadArchive, Plugin, PluginDownloads, PluginHost, PluginPermission,
};
use crate::prompts;
use crate::protocol::{
    Comparison, Protocol, ProtocolContext, ProtocolDescriptor, ProtocolRequest, RequestHeader,
    parse_comparison,
};
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
const SEARCH_SCHEME: &str = "search";

fn help(cwd: &Path, scheme: &str) -> String {
    format!(
        r#"# {scheme}

Search file contents with ripgrep (`rg`) or on-demand semantic and hybrid
retrieval.

Current working directory: `{scheme}://{}`

Search reads other than `mode: status` MUST pass a nonempty search pattern in
the request body; `mode: status` takes no body. Use
`{scheme}://<root>` for a project-relative or absolute file/directory root. The
root may be empty: `{scheme}://` searches the current working directory. On Unix, `~`
and paths beginning with `~/` resolve from the current user's home directory;
`~user` is not expanded.

Optional request headers (`*** <name>: <value>` lines between the operation
line and a `*** Body:` separator):

- `mode: exact` (the default) uses ripgrep (`rg`). Patterns use `rg`
  regular-expression syntax unless `literal: true` is set. Use this mode for
  known identifiers, paths, syntax, or literal wording. Exact results are
  `path:line:text` lines; an output ending with `[match limit reached: <limit>]`
  covers only the first `limit` matches.
- `mode: hybrid` combines keyword and semantic ranking. Prefer it for conceptual
  searches.
- `mode: semantic` prioritizes meaning over shared wording. Use it when relevant
  results are likely to use different wording from the query.
- `mode: status` reports whether the selected root's semantic cache is current;
  it takes no body and accepts no headers other than `glob`.
- `glob: <pattern>` filters searched paths using `rg` glob syntax.
- If `rg` rejects a regular expression, the protocol retries it as literal
  text.
- `literal: true` always uses `rg` literal matching.
- `ignore_case: true` enables case-insensitive matching.
- `context: <lines>` includes surrounding lines; values are clamped to 0 through 20.
- `limit: <count>` bounds the number of matches; the default is 200 and values are
  clamped to 1 through 2,000.

The numeric `context` and `limit` headers accept a comparison prefix —
`>=10`, `>10`, `<=50`, `<50` — which bounds the effective value while its
default still applies within the bounds; one lower and one upper bound may
combine into a range. `!=` is not supported.

Semantic and hybrid reads accept only `mode`, `glob`, and `limit`; their
default limit is 7 and values are clamped to 1 through 50. A ranked read creates
or incrementally refreshes its selected root/glob cache as needed, then searches
it. Most searches return in the same call; a longer search continues as one
managed task without restarting and delivers its result automatically. If
completion marks the output as truncated, follow its `tasks://` instruction
once. Do not submit the same search again to retrieve task output.

Do not call status or index before a ranked search. Use `mode: status` only to
diagnose the cache. Use an `*** Exec:` request only to prewarm or force-rebuild
that exact root/glob cache:

```text
*** Begin Request
*** Exec: {scheme}://<root>
*** mode: index
*** glob: <pattern>
*** End Request
```

Indexing follows standard ignore files, skips binary/non-UTF-8 files and files
larger than 1 MiB, and chunks readable text into line-ranged fragments. Results
show the actual matching fragment with its precise line range.

Examples:

```text
*** Begin Request
*** Read: {scheme}://src
*** glob: **/*.rs
*** limit: 100
*** Body:
ProtocolRequest
*** End Request

*** Begin Request
*** Read: {scheme}://src/tui/app.rs
fn push(
*** End Request

*** Begin Request
*** Read: {scheme}://
*** literal: true
*** ignore_case: true
*** Body:
exact text
*** End Request

*** Begin Request
*** Read: {scheme}://src
*** mode: hybrid
*** glob: **/*.rs
*** limit: 10
*** Body:
authentication flow
*** End Request

*** Begin Request
*** Read: {scheme}://src
*** mode: hybrid
*** glob: **/*.rs
*** limit: <=50
*** Body:
authentication flow
*** End Request

*** Begin Request
*** Exec: {scheme}://src
*** mode: index
*** glob: **/*.rs
*** End Request
```

`*** Exec:` requests support only `mode: index` (optionally with `glob`) and
take no request body; `status` and `index` accept no other headers.
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
    ) -> Result<Vec<u8>> {
        let record = context
            .tasks
            .allocate(
                SEARCH_SCHEME,
                format!("Search code under {}", display_path(&root)),
            )
            .await;
        let cwd = self.cwd.clone();
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
            AutoTask::Terminal(record) => record.terminal_result("semantic search"),
        }
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
    ) -> Result<Vec<u8>> {
        if request.target == "help" {
            if !request.headers.is_empty() {
                bail!("{}://help accepts no request headers", SEARCH_SCHEME);
            }
            if !request.body.is_empty() {
                bail!("{}://help requires an empty body", SEARCH_SCHEME);
            }
            return Ok(help(&self.cwd, SEARCH_SCHEME).into_bytes());
        }
        let root = request.target;
        let options = GrepOptions::parse(request.headers, SEARCH_SCHEME)?;
        let resolved = resolve_path(&self.cwd, root)?;
        validate_root(&resolved, SEARCH_SCHEME).await?;
        match options.mode {
            GrepMode::Exact => {
                require_search_body(request.body, request.uri, SEARCH_SCHEME)?;
                let downloads = self.downloads.as_ref().ok_or_else(|| {
                    anyhow!("{} binary download access is not attached", SEARCH_SCHEME)
                })?;
                let rg = downloads.ensure(&ripgrep_download()?).await?;
                run_grep(
                    &rg,
                    &self.cwd,
                    &grep_root_argument(&self.cwd, root, &resolved),
                    request.body,
                    &options,
                    SEARCH_SCHEME,
                )
                .await
                .map(String::into_bytes)
            }
            GrepMode::Semantic(mode) => {
                require_search_body(request.body, request.uri, SEARCH_SCHEME)?;
                options.validate_semantic(SEARCH_SCHEME)?;
                self.run_semantic_grep(
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
                    bail!(
                        "{} semantic index status requires an empty body",
                        SEARCH_SCHEME
                    );
                }
                options.validate_index_operation(SEARCH_SCHEME)?;
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
        let root = request.target;
        let options = GrepOptions::parse_exec(request.headers, SEARCH_SCHEME)?;
        if !request.body.is_empty() {
            bail!("{} semantic indexing requires an empty body", SEARCH_SCHEME);
        }
        options.validate_index_operation(SEARCH_SCHEME)?;
        let resolved = resolve_path(&self.cwd, root)?;
        validate_root(&resolved, SEARCH_SCHEME).await?;
        let label = format!("Index code under {}", display_path(&resolved));
        let record = context
            .tasks
            .allocate_background(SEARCH_SCHEME, label)
            .await?;
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

/// Headers accepted by every `search` operation. Comparable numeric headers
/// (`context`, `limit`) accept a comparison prefix; the rest are
/// equality-only.
const HEADER_NAMES: [&str; 6] = ["mode", "glob", "literal", "ignore_case", "context", "limit"];

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
    fn parse(headers: &[RequestHeader], scheme: &str) -> Result<Self> {
        let mut options = Self {
            mode: GrepMode::Exact,
            glob: None,
            literal: false,
            ignore_case: false,
            context: 0,
            limit: DEFAULT_LIMIT,
            limit_set: false,
        };
        options.apply_headers(headers, scheme)?;
        Ok(options)
    }

    fn set_option(&mut self, name: &str, value: &str, scheme: &str) -> Result<()> {
        match name {
            "mode" => {
                self.mode = match value {
                    "exact" => GrepMode::Exact,
                    "semantic" | "hybrid" => GrepMode::Semantic(SearchMode::parse(value, scheme)?),
                    "status" => GrepMode::Status,
                    "index" => {
                        bail!("{scheme} mode=index is available only through exec")
                    }
                    _ => {
                        bail!("{scheme} mode must be exact, semantic, hybrid, or status for reads")
                    }
                };
            }
            "glob" if !value.is_empty() => self.glob = Some(value.to_string()),
            "glob" => bail!("{scheme} glob cannot be empty"),
            "literal" => self.literal = parse_bool(name, value, scheme)?,
            "ignore_case" => self.ignore_case = parse_bool(name, value, scheme)?,
            "context" => {
                self.context = value
                    .parse::<usize>()
                    .with_context(|| format!("invalid {scheme} context: {value}"))?
                    .min(MAX_CONTEXT);
            }
            "limit" => {
                self.limit = value
                    .parse::<usize>()
                    .with_context(|| format!("invalid {scheme} limit: {value}"))?
                    .clamp(1, MAX_LIMIT);
                self.limit_set = true;
            }
            _ => bail!("unknown {scheme} header: {name}"),
        }
        Ok(())
    }

    fn apply_headers(&mut self, headers: &[RequestHeader], scheme: &str) -> Result<()> {
        if let Some(header) = headers
            .iter()
            .find(|header| !HEADER_NAMES.contains(&header.name.as_str()))
        {
            bail!(
                "unknown {scheme} header: {}; supported headers: {}",
                header.name,
                HEADER_NAMES.join(", ")
            );
        }
        for name in ["mode", "glob", "literal", "ignore_case"] {
            let values: Vec<&str> = headers
                .iter()
                .filter(|header| header.name == name)
                .map(|header| header.value.as_str())
                .collect();
            if values.len() > 1 {
                bail!("duplicate {scheme} header: {name}");
            }
            if let Some(value) = values.first() {
                self.set_option(name, value, scheme)?;
            }
        }
        for name in ["context", "limit"] {
            self.apply_comparison_headers(name, headers, scheme)?;
        }
        Ok(())
    }

    /// Applies the comparison-capable numeric headers `context` and `limit`.
    /// An exact value sets the option as usual. Bounds clamp the option's
    /// default: one lower bound (`>` or `>=`) and one upper bound (`<` or
    /// `<=`) may combine into a range.
    fn apply_comparison_headers(
        &mut self,
        name: &str,
        headers: &[RequestHeader],
        scheme: &str,
    ) -> Result<()> {
        let values: Vec<&str> = headers
            .iter()
            .filter(|header| header.name == name)
            .map(|header| header.value.as_str())
            .collect();
        if values.is_empty() {
            return Ok(());
        }
        let mut exact = None;
        let mut lower = None::<usize>;
        let mut upper = None::<usize>;
        for value in values {
            let (comparison, operand) = parse_comparison(value);
            let number = operand
                .parse::<usize>()
                .with_context(|| format!("invalid {scheme} {name}: {operand}"))?;
            match comparison {
                Comparison::Eq => {
                    if exact.replace(number).is_some() {
                        bail!("duplicate {scheme} header: {name}");
                    }
                }
                Comparison::Ne => {
                    bail!(
                        "{scheme} header {name} does not support `!=`; use an exact value or a bound"
                    );
                }
                Comparison::Gt | Comparison::Ge => {
                    let bound = if comparison == Comparison::Gt {
                        number.saturating_add(1)
                    } else {
                        number
                    };
                    if lower.replace(bound).is_some() {
                        bail!("duplicate lower bound for {scheme} header: {name}");
                    }
                }
                Comparison::Lt | Comparison::Le => {
                    if comparison == Comparison::Lt && number == 0 {
                        bail!("{scheme} header {name} has an empty range below 0");
                    }
                    let bound = if comparison == Comparison::Lt {
                        number - 1
                    } else {
                        number
                    };
                    if upper.replace(bound).is_some() {
                        bail!("duplicate upper bound for {scheme} header: {name}");
                    }
                }
            }
        }
        if exact.is_some() && (lower.is_some() || upper.is_some()) {
            bail!("{scheme} header {name} combines an exact value with a bound");
        }
        let (default, minimum, maximum) = match name {
            "context" => (0, 0, MAX_CONTEXT),
            _ => (DEFAULT_LIMIT, 1, MAX_LIMIT),
        };
        let resolved = match exact {
            Some(exact) => exact.clamp(minimum, maximum),
            None => {
                let lower = lower.unwrap_or(minimum);
                let upper = upper.unwrap_or(maximum);
                if lower > upper {
                    bail!(
                        "{scheme} header {name} has an empty range: lower bound {lower} exceeds upper bound {upper}"
                    );
                }
                default.clamp(lower, upper).clamp(minimum, maximum)
            }
        };
        match name {
            "context" => self.context = resolved,
            _ => {
                self.limit = resolved;
                self.limit_set = true;
            }
        }
        Ok(())
    }

    fn parse_exec(headers: &[RequestHeader], scheme: &str) -> Result<Self> {
        let mode_headers: Vec<&str> = headers
            .iter()
            .filter(|header| header.name == "mode")
            .map(|header| header.value.as_str())
            .collect();
        if mode_headers.len() > 1 {
            bail!("duplicate {scheme} header: mode");
        }
        if mode_headers.first().is_none_or(|value| *value != "index") {
            bail!("{scheme} exec requires a `mode: index` header");
        }
        let rewritten_headers: Vec<RequestHeader> = headers
            .iter()
            .map(|header| {
                if header.name == "mode" {
                    RequestHeader::new("mode", "status")
                } else {
                    header.clone()
                }
            })
            .collect();
        let mut options = Self::parse(&rewritten_headers, scheme)?;
        options.mode = GrepMode::Status;
        Ok(options)
    }

    fn validate_semantic(&self, scheme: &str) -> Result<()> {
        if self.literal || self.ignore_case || self.context != 0 {
            bail!("semantic {scheme} accepts only mode, glob, and limit");
        }
        Ok(())
    }

    fn validate_index_operation(&self, scheme: &str) -> Result<()> {
        if self.literal || self.ignore_case || self.context != 0 || self.limit_set {
            bail!("{scheme} semantic index operations accept only mode and glob");
        }
        Ok(())
    }

    fn semantic_limit(&self) -> usize {
        if self.limit_set {
            self.limit.min(MAX_SEMANTIC_LIMIT)
        } else {
            7
        }
    }
}

fn require_search_body(body: &str, uri: &str, scheme: &str) -> Result<()> {
    if body.is_empty() {
        bail!(
            "{scheme} requires a nonempty search pattern in the request body; correct form:\n\
             *** Begin Request\n\
             *** Read: {uri}\n\
             <pattern>\n\
             *** End Request"
        );
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

fn parse_bool(name: &str, value: &str, scheme: &str) -> Result<bool> {
    match value {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => bail!("{scheme} {name} must be true or false"),
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
        let headers = |pairs: &[(&str, &str)]| {
            pairs
                .iter()
                .map(|(name, value)| RequestHeader::new(name, value))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            GrepOptions::parse(
                &headers(&[
                    ("glob", "**/*.rs"),
                    ("literal", "true"),
                    ("ignore_case", "true"),
                    ("context", "2"),
                    ("limit", "10"),
                ]),
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
            GrepOptions::parse(&headers(&[("mode", "semantic")]), SEARCH_SCHEME)
                .unwrap()
                .semantic_limit(),
            7
        );
        let hybrid = GrepOptions::parse(
            &headers(&[("mode", "hybrid"), ("limit", "51")]),
            SEARCH_SCHEME,
        )
        .unwrap();
        hybrid.validate_semantic(SEARCH_SCHEME).unwrap();
        assert_eq!(hybrid.semantic_limit(), 50);
        assert!(
            GrepOptions::parse_exec(
                &headers(&[("mode", "index"), ("glob", "**/*.rs")]),
                SEARCH_SCHEME
            )
            .is_ok()
        );
        assert!(GrepOptions::parse_exec(&headers(&[("mode", "semantic")]), SEARCH_SCHEME).is_err());
        assert!(
            GrepOptions::parse_exec(
                &headers(&[("mode", "index"), ("limit", "1")]),
                SEARCH_SCHEME
            )
            .unwrap()
            .validate_index_operation(SEARCH_SCHEME)
            .is_err()
        );
        assert_eq!(
            GrepOptions::parse(&headers(&[("context", "21")]), SEARCH_SCHEME)
                .unwrap()
                .context,
            20
        );
        assert_eq!(
            GrepOptions::parse(&headers(&[("limit", "0")]), SEARCH_SCHEME)
                .unwrap()
                .limit,
            1
        );
        assert_eq!(
            GrepOptions::parse(&headers(&[("limit", "99999")]), SEARCH_SCHEME)
                .unwrap()
                .limit,
            2_000
        );
        assert!(
            GrepOptions::parse(
                &headers(&[("literal", "true"), ("literal", "false")]),
                SEARCH_SCHEME
            )
            .is_err()
        );
    }

    #[test]
    fn grep_options_accept_headers_with_comparison_bounds() {
        use crate::retrieval::SearchMode;

        // Header names are case-insensitive and normalized by the request
        // parser.
        let options = GrepOptions::parse(
            &[
                RequestHeader::new("Mode", "hybrid"),
                RequestHeader::new("glob", "**/*.rs"),
            ],
            SEARCH_SCHEME,
        )
        .unwrap();
        assert_eq!(options.mode, GrepMode::Semantic(SearchMode::Hybrid));
        assert_eq!(options.glob.as_deref(), Some("**/*.rs"));

        // Exact values set the option; bounds clamp the default.
        let exact =
            GrepOptions::parse(&[RequestHeader::new("limit", "10")], SEARCH_SCHEME).unwrap();
        assert_eq!(exact.limit, 10);
        assert!(exact.limit_set);
        let lower =
            GrepOptions::parse(&[RequestHeader::new("limit", ">=1000")], SEARCH_SCHEME).unwrap();
        assert_eq!(lower.limit, 1000);
        let upper =
            GrepOptions::parse(&[RequestHeader::new("limit", "<=50")], SEARCH_SCHEME).unwrap();
        assert_eq!(upper.limit, 50);
        // The default (200) lands inside the range and clamps to the upper bound.
        let range = GrepOptions::parse(
            &[
                RequestHeader::new("limit", ">=10"),
                RequestHeader::new("limit", "<=50"),
            ],
            SEARCH_SCHEME,
        )
        .unwrap();
        assert_eq!(range.limit, 50);
        // `>1` raises the lower bound to 2; context defaults to 0.
        let context =
            GrepOptions::parse(&[RequestHeader::new("context", ">1")], SEARCH_SCHEME).unwrap();
        assert_eq!(context.context, 2);
        // A lower bound below the default leaves the default untouched.
        let bounded_default =
            GrepOptions::parse(&[RequestHeader::new("limit", ">=10")], SEARCH_SCHEME).unwrap();
        assert_eq!(bounded_default.limit, 200);
    }

    #[test]
    fn grep_options_reject_header_misuse() {
        let unknown = GrepOptions::parse(&[RequestHeader::new("offset", "5")], SEARCH_SCHEME);
        assert!(format!("{:#}", unknown.unwrap_err()).contains("unknown search header: offset"));
        let duplicate = GrepOptions::parse(
            &[
                RequestHeader::new("limit", "10"),
                RequestHeader::new("limit", "20"),
            ],
            SEARCH_SCHEME,
        );
        assert!(format!("{:#}", duplicate.unwrap_err()).contains("duplicate search header: limit"));
        let inequality = GrepOptions::parse(&[RequestHeader::new("limit", "!=10")], SEARCH_SCHEME);
        assert!(format!("{:#}", inequality.unwrap_err()).contains("does not support `!=`"));
        let empty = GrepOptions::parse(
            &[
                RequestHeader::new("limit", ">=100"),
                RequestHeader::new("limit", "<=50"),
            ],
            SEARCH_SCHEME,
        );
        assert!(format!("{:#}", empty.unwrap_err()).contains("empty range"));
        let mixed = GrepOptions::parse(
            &[
                RequestHeader::new("limit", "10"),
                RequestHeader::new("limit", ">=5"),
            ],
            SEARCH_SCHEME,
        );
        assert!(
            format!("{:#}", mixed.unwrap_err()).contains("combines an exact value with a bound")
        );
        // Equality-only headers do not take comparison prefixes.
        let compared = GrepOptions::parse(&[RequestHeader::new("mode", ">hybrid")], SEARCH_SCHEME);
        assert!(compared.is_err());
    }

    #[test]
    fn grep_exec_accepts_mode_index_as_a_header() {
        let options =
            GrepOptions::parse_exec(&[RequestHeader::new("mode", "index")], SEARCH_SCHEME).unwrap();
        assert_eq!(options.mode, GrepMode::Status);
        assert!(
            GrepOptions::parse_exec(&[RequestHeader::new("mode", "status")], SEARCH_SCHEME)
                .is_err()
        );
        assert!(GrepOptions::parse_exec(&[], SEARCH_SCHEME).is_err());
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
    async fn search_help_uses_rg_contract() {
        let directory = tempfile::tempdir().unwrap();
        let protocol = GrepProtocol::new(directory.path());
        assert_eq!(protocol.descriptor().name, SEARCH_SCHEME);
        let help = protocol
            .read(
                ProtocolRequest {
                    uri: "search://help",
                    target: "help",
                    headers: &[],
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
        assert!(help.contains("The\nroot may be empty"));
        assert!(help.contains("paths beginning with `~/` resolve"));
        assert!(help.contains("`~user` is not expanded"));
        assert!(help.contains("uses ripgrep (`rg`)"));
        assert!(help.contains("Patterns use `rg`\n  regular-expression syntax"));
        assert!(help.contains("retries it as literal\n  text"));
        assert!(help.contains("*** Read: search://src/tui/app.rs\nfn push("));
        assert!(!help.contains("grep://"));
        assert!(help.contains("Prefer it for conceptual\n  searches"));
        assert!(help.contains("values are clamped to 0 through 20"));
        assert!(help.contains("clamped to 1 through 2,000"));
        assert!(help.contains("clamped to 1 through 50"));
        assert!(help.contains("Do not call status or index before a ranked search"));
        assert!(help.contains("continues as one\nmanaged task without restarting"));
        assert!(help.contains("`*** Exec:` requests support only `mode: index`"));

        let error = protocol
            .read(
                ProtocolRequest {
                    uri: "search://",
                    target: "",
                    headers: &[],
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
        assert!(error.contains("*** Read: search://\n<pattern>"));
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
                    uri: "search://",
                    target: "",
                    headers: &[
                        RequestHeader::new("glob", "**/*.rs"),
                        RequestHeader::new("ignore_case", "true"),
                        RequestHeader::new("context", "1"),
                        RequestHeader::new("limit", "1"),
                    ],
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
        let default_options = GrepOptions::parse(&[], SEARCH_SCHEME).unwrap();

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
            ..GrepOptions::parse(&[], SEARCH_SCHEME).unwrap()
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
