use super::resolve_path;
use crate::config::display_path;
use crate::plugin::{BinaryDownload, DownloadArchive, PluginDownloads};
use crate::retrieval::{SearchFilter, SearchHit, SearchMode, code_corpus, search_live_corpus};
use anyhow::{Context, Result, anyhow, bail};
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

/// Exact ripgrep search, returned as `path:line:text` lines.
pub(crate) async fn exact_search(
    downloads: &PluginDownloads,
    cwd: &Path,
    root: &str,
    query: &str,
    glob: Option<&str>,
    limit: Option<usize>,
) -> Result<String> {
    let resolved = resolve_path(cwd, root)?;
    validate_root(&resolved, "search").await?;
    let input = ReadInput {
        glob: glob.map(str::to_string),
        literal: None,
        ignore_case: None,
        context: None,
        limit,
    };
    let options = GrepOptions::from_read_input(&input, "search")?;
    let rg = downloads.ensure(&ripgrep_download()?).await?;
    let root_argument = grep_root_argument(cwd, root, &resolved);
    run_grep(&rg, cwd, &root_argument, query, &options, "search").await
}

/// Semantic or hybrid code search.
pub(crate) async fn semantic_search(
    cwd: &Path,
    root: &str,
    query: &str,
    mode: SearchMode,
    glob: Option<&str>,
    limit: Option<usize>,
) -> Result<String> {
    let resolved = resolve_path(cwd, root)?;
    validate_root(&resolved, "search").await?;
    let limit = limit
        .unwrap_or(DEFAULT_SEMANTIC_LIMIT)
        .clamp(1, MAX_SEMANTIC_LIMIT);
    let bytes = semantic_grep(
        cwd,
        &resolved,
        glob,
        query,
        mode,
        limit,
        CancellationToken::new(),
    )
    .await?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadInput {
    glob: Option<String>,
    literal: Option<bool>,
    ignore_case: Option<bool>,
    context: Option<usize>,
    limit: Option<usize>,
}

#[derive(Debug, Eq, PartialEq)]
struct GrepOptions {
    glob: Option<String>,
    literal: bool,
    ignore_case: bool,
    context: usize,
    limit: usize,
}

impl GrepOptions {
    fn from_read_input(input: &ReadInput, scheme: &str) -> Result<Self> {
        if let Some(glob) = &input.glob
            && glob.is_empty()
        {
            bail!("{scheme} glob cannot be empty");
        }
        Ok(Self {
            glob: input.glob.clone(),
            literal: input.literal.unwrap_or(false),
            ignore_case: input.ignore_case.unwrap_or(false),
            context: input.context.unwrap_or(0).min(MAX_CONTEXT),
            limit: input.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT),
        })
    }
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
