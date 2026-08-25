use super::file::resolve_path;
use crate::config::display_path;
use crate::plugin::{
    BinaryDownload, DownloadArchive, Plugin, PluginDownloads, PluginHost, PluginPermission,
};
use crate::protocol::{Protocol, ProtocolContext, ProtocolDescriptor, ProtocolRequest};
use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Command;

const DEFAULT_LIMIT: usize = 200;
const MAX_LIMIT: usize = 2_000;
const MAX_CONTEXT: usize = 20;
const RIPGREP_VERSION: &str = "14.1.1";

fn help(cwd: &Path) -> String {
    format!(
        r#"# grep

Search file contents and return bounded `path:line:text` matches.

Current working directory: `grep://{}`

Search reads MUST pass a nonempty search pattern in the string body. Use
`grep://<root>` for a project-relative or absolute file/directory root. The root
may be empty: `grep://` searches the current working directory.

Optional query parameters:

- `glob=<pattern>` filters searched paths with a glob pattern.
- `literal=true` treats the body as literal text instead of a regular expression.
- `ignore_case=true` enables case-insensitive matching.
- `context=<0..20>` includes surrounding lines.
- `limit=<1..2000>` bounds the number of matches; the default is 200.

Examples:

```text
read("grep://src?glob=**/*.rs&limit=100", "ProtocolRequest")
read("grep://?literal=true&ignore_case=true", "exact text")
```

`grep://help` MUST use an empty string body. This protocol supports `read` only;
it does not support `exec`.
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
            description: "Search file contents with bounded results and optional glob filtering."
                .to_string(),
            can_read: true,
            can_exec: false,
        }
    }

    async fn read(
        &self,
        request: ProtocolRequest<'_>,
        _context: ProtocolContext,
    ) -> Result<Vec<u8>> {
        if request.target == "help" {
            if !request.body.is_empty() {
                bail!("grep://help does not accept a body");
            }
            return Ok(help(&self.cwd).into_bytes());
        }
        if request.body.is_empty() {
            bail!("grep requires a nonempty search pattern in the body");
        }
        let (root, query) = request
            .target
            .split_once('?')
            .map_or((request.target, None), |(root, query)| (root, Some(query)));
        let options = GrepOptions::parse(query)?;
        let resolved = resolve_path(&self.cwd, root);
        let metadata = tokio::fs::metadata(&resolved)
            .await
            .with_context(|| format!("cannot search {}", display_path(&resolved)))?;
        if !metadata.is_dir() && !metadata.is_file() {
            bail!(
                "grep root is not a regular file or directory: {}",
                display_path(&resolved)
            );
        }
        let downloads = self
            .downloads
            .as_ref()
            .ok_or_else(|| anyhow!("grep binary download access is not attached"))?;
        let rg = downloads.ensure(&ripgrep_download()?).await?;
        run_grep(
            &rg,
            &self.cwd,
            if root.is_empty() { "." } else { root },
            request.body,
            &options,
        )
        .await
        .map(String::into_bytes)
    }
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
    fn parse(query: Option<&str>) -> Result<Self> {
        let mut options = Self {
            glob: None,
            literal: false,
            ignore_case: false,
            context: 0,
            limit: DEFAULT_LIMIT,
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
                }
                _ => bail!("unknown grep query parameter: {name}"),
            }
        }
        Ok(options)
    }
}

fn parse_bool(name: &str, value: &str) -> Result<bool> {
    match value {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => bail!("grep {name} must be true or false"),
    }
}

async fn run_grep(
    executable: &Path,
    cwd: &Path,
    root: &str,
    pattern: &str,
    options: &GrepOptions,
) -> Result<String> {
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
    if options.literal {
        command.arg("--fixed-strings");
    }
    if options.ignore_case {
        command.arg("--ignore-case");
    }
    if options.context > 0 {
        command.arg("--context").arg(options.context.to_string());
    }
    command.arg("--").arg(pattern).arg(root);
    let mut child = command
        .spawn()
        .with_context(|| format!("failed to start {}", executable.display()))?;
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
        let event: Value = serde_json::from_str(&line).context("ripgrep returned invalid JSON")?;
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
    if !truncated && !matches!(status.code(), Some(0 | 1)) {
        let message = String::from_utf8_lossy(&stderr).trim().to_string();
        bail!(
            "ripgrep failed{}",
            if message.is_empty() {
                String::new()
            } else {
                format!(": {message}")
            }
        );
    }
    if matches == 0 {
        return Ok("No matches.\n".to_string());
    }
    if truncated {
        output.push_str(&format!("\n[match limit reached: {}]\n", options.limit));
    }
    Ok(output)
}

fn append_event(output: &mut String, event: &Value, is_match: bool) -> Result<()> {
    let data = event
        .get("data")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("ripgrep event has no data object"))?;
    let path = data
        .get("path")
        .and_then(|path| path.get("text"))
        .and_then(Value::as_str)
        .unwrap_or("<non-UTF-8 path>")
        .trim_start_matches("./");
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
    output.push_str(path);
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
                glob: Some("**/*.rs".to_string()),
                literal: true,
                ignore_case: true,
                context: 2,
                limit: 10,
            }
        );
        assert!(GrepOptions::parse(Some("context=21")).is_err());
        assert!(GrepOptions::parse(Some("limit=0")).is_err());
        assert!(GrepOptions::parse(Some("literal=true&literal=false")).is_err());
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
        assert!(error.to_string().contains("nonempty search pattern"));
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
        tokio::fs::write(directory.path().join(".gitignore"), "nested/two.rs\n")
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
    async fn grep_reports_no_matches_honors_literal_mode_and_surfaces_invalid_regex() {
        let Some(rg) = rg_on_path() else {
            return;
        };
        let directory = tempfile::tempdir().unwrap();
        tokio::fs::write(directory.path().join("values.txt"), "a.b\naxb\n")
            .await
            .unwrap();
        let default_options = GrepOptions::parse(None).unwrap();

        assert_eq!(
            run_grep(&rg, directory.path(), ".", "missing", &default_options)
                .await
                .unwrap(),
            "No matches.\n"
        );
        let literal = GrepOptions {
            literal: true,
            ..GrepOptions::parse(None).unwrap()
        };
        let output = run_grep(&rg, directory.path(), ".", "a.b", &literal)
            .await
            .unwrap();
        assert!(output.contains("values.txt:1:a.b"));
        assert!(!output.contains("values.txt:2:axb"));

        let error = run_grep(&rg, directory.path(), ".", "(", &default_options)
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("ripgrep failed"));
    }
}
