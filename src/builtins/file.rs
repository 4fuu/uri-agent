use crate::config::display_path;
use crate::plugin::{Plugin, PluginHost};
use crate::protocol::{Protocol, ProtocolContext, ProtocolDescriptor, ProtocolRequest};
use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use tokio::fs;

const DEFAULT_LIMIT: usize = 200;
const MAX_LIMIT: usize = 2_000;

fn help(cwd: &Path) -> String {
    format!(
        r#"# file

Read files and directories.

Current working directory: `file://{}`

- Use `file://<path>` to read a file or directory. Replace `<path>` with a
  project-relative path or an absolute filesystem path; relative paths resolve
  from the current working directory.
- Add `?offset=<line>&limit=<count>` to read a bounded range. `<line>` is the
  one-based starting line or directory-entry position, and `<count>` is the
  maximum number of lines or entries to return.
- Add `?line_numbers=true` to prefix file content with one-based line numbers. Line numbers are disabled by default.
- Reading a directory returns a bounded directory listing.
- Full outputs saved by the system are exposed as `file://` addresses.

The body is passed through but is not required by this built-in protocol.
"#,
        display_path(cwd)
    )
}

#[derive(Clone)]
pub(super) struct FileProtocol {
    cwd: PathBuf,
}

impl FileProtocol {
    pub(super) fn new(cwd: &Path) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
        }
    }
}

impl Plugin for FileProtocol {
    fn protocol_descriptors(&self) -> Vec<ProtocolDescriptor> {
        vec![self.descriptor()]
    }

    fn register(&self, host: &mut PluginHost<'_>) -> Result<()> {
        host.protocols.register(self.clone())
    }
}

#[async_trait]
impl Protocol for FileProtocol {
    fn descriptor(&self) -> ProtocolDescriptor {
        ProtocolDescriptor {
            name: "file".to_string(),
            description: "Read files and directory listings with bounded line ranges.".to_string(),
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
            return Ok(help(&self.cwd).into_bytes());
        }

        let (target, query) = split_query(request.target);
        let path = resolve_path(&self.cwd, target);
        let metadata = fs::metadata(&path)
            .await
            .with_context(|| format!("cannot read {}", display_path(&path)))?;
        let range = Range::parse(query)?;
        if metadata.is_dir() {
            read_directory(&path, range).await
        } else if metadata.is_file() {
            read_file(&path, request.uri, range).await
        } else {
            bail!("not a regular file or directory: {}", display_path(&path))
        }
    }
}

pub(crate) fn resolve_path(cwd: &Path, target: &str) -> PathBuf {
    let path = Path::new(target);
    if path.is_absolute() {
        path.to_path_buf()
    } else if target.is_empty() {
        cwd.to_path_buf()
    } else {
        cwd.join(path)
    }
}

fn split_query(target: &str) -> (&str, Option<&str>) {
    match target.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (target, None),
    }
}

#[derive(Clone, Copy)]
struct Range {
    offset: usize,
    limit: usize,
    line_numbers: bool,
}

impl Range {
    fn parse(query: Option<&str>) -> Result<Self> {
        let mut offset = 1_usize;
        let mut limit = DEFAULT_LIMIT;
        let mut line_numbers = false;
        for pair in query
            .unwrap_or_default()
            .split('&')
            .filter(|pair| !pair.is_empty())
        {
            let (key, value) = pair
                .split_once('=')
                .ok_or_else(|| anyhow!("invalid query component: {pair}"))?;
            match key {
                "offset" => {
                    offset = value
                        .parse::<usize>()
                        .with_context(|| format!("invalid offset: {value}"))?
                        .max(1)
                }
                "limit" => {
                    limit = value
                        .parse::<usize>()
                        .with_context(|| format!("invalid limit: {value}"))?
                        .clamp(1, MAX_LIMIT)
                }
                "line_numbers" => {
                    line_numbers = match value {
                        "true" => true,
                        "false" => false,
                        _ => bail!("invalid line_numbers: {value}; expected true or false"),
                    }
                }
                _ => bail!("unknown file query parameter: {key}"),
            }
        }
        Ok(Self {
            offset,
            limit,
            line_numbers,
        })
    }
}

async fn read_file(path: &Path, uri: &str, range: Range) -> Result<Vec<u8>> {
    let content = fs::read(path)
        .await
        .with_context(|| format!("cannot read {}", display_path(path)))?;
    let content = String::from_utf8_lossy(&content);
    let lines = content.lines().collect::<Vec<_>>();
    let start = range.offset.saturating_sub(1).min(lines.len());
    let end = start.saturating_add(range.limit).min(lines.len());
    let mut output = String::new();
    for (index, line) in lines[start..end].iter().enumerate() {
        if range.line_numbers {
            let line_number = start + index + 1;
            let width = end.max(1).to_string().len();
            let _ = writeln!(output, "{line_number:>width$} │ {line}");
        } else {
            let _ = writeln!(output, "{line}");
        }
    }

    if end < lines.len() {
        let base = uri.split_once('?').map_or(uri, |(base, _)| base);
        let line_numbers = if range.line_numbers {
            "&line_numbers=true"
        } else {
            ""
        };
        let _ = writeln!(
            output,
            "\n[{} more lines]\nNext: {}?offset={}&limit={}{}",
            lines.len() - end,
            base,
            end + 1,
            range.limit,
            line_numbers
        );
    }
    Ok(output.into_bytes())
}

async fn read_directory(path: &Path, range: Range) -> Result<Vec<u8>> {
    let mut directory = fs::read_dir(path)
        .await
        .with_context(|| format!("cannot list {}", display_path(path)))?;
    let mut entries = Vec::new();
    while let Some(entry) = directory.next_entry().await? {
        let file_type = entry.file_type().await?;
        let suffix = if file_type.is_dir() {
            "/"
        } else if file_type.is_symlink() {
            "@"
        } else {
            ""
        };
        entries.push(format!("{}{}", entry.file_name().to_string_lossy(), suffix));
    }
    entries.sort_unstable();
    let start = range.offset.saturating_sub(1).min(entries.len());
    let end = start.saturating_add(range.limit).min(entries.len());
    let mut output = entries[start..end].join("\n");
    if !output.is_empty() {
        output.push('\n');
    }
    if end < entries.len() {
        let _ = writeln!(output, "\n[{} more entries]", entries.len() - end);
    }
    Ok(output.into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_reports_display_path_and_opt_in_line_numbers() {
        let help = help(Path::new(r"\\?\C:\Users\4fu\project"));
        assert!(help.contains(r"Current working directory: `file://C:\Users\4fu\project`"));
        assert!(help.contains("`file://<path>`"));
        assert!(help.contains("`?offset=<line>&limit=<count>`"));
        assert!(help.contains("`?line_numbers=true`"));
        assert!(help.contains("Line numbers are disabled by default."));
    }

    #[test]
    fn custom_targets_are_not_percent_decoded() {
        let cwd = Path::new("/tmp/root");
        assert_eq!(resolve_path(cwd, "a%20b"), Path::new("/tmp/root/a%20b"));
    }

    #[test]
    fn ranges_are_one_based_and_bounded() {
        let range = Range::parse(Some("offset=0&limit=99999")).unwrap();
        assert_eq!(range.offset, 1);
        assert_eq!(range.limit, MAX_LIMIT);
        assert!(!range.line_numbers);
    }

    #[test]
    fn line_numbers_are_opt_in() {
        assert!(
            Range::parse(Some("line_numbers=true"))
                .unwrap()
                .line_numbers
        );
        assert!(
            !Range::parse(Some("line_numbers=false"))
                .unwrap()
                .line_numbers
        );
        assert!(Range::parse(Some("line_numbers=1")).is_err());
    }

    #[tokio::test]
    async fn file_output_only_includes_line_numbers_when_requested() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("file.txt");
        fs::write(&path, "alpha\nbeta\n").await.unwrap();

        let plain = read_file(&path, "file://file.txt", Range::parse(None).unwrap())
            .await
            .unwrap();
        assert_eq!(String::from_utf8(plain).unwrap(), "alpha\nbeta\n");

        let numbered = read_file(
            &path,
            "file://file.txt?line_numbers=true",
            Range::parse(Some("line_numbers=true")).unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(
            String::from_utf8(numbered).unwrap(),
            "1 │ alpha\n2 │ beta\n"
        );
    }

    #[tokio::test]
    async fn pagination_preserves_the_line_number_option() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("file.txt");
        fs::write(&path, "alpha\nbeta\n").await.unwrap();

        let output = read_file(
            &path,
            "file://file.txt?offset=1&limit=1&line_numbers=true",
            Range::parse(Some("offset=1&limit=1&line_numbers=true")).unwrap(),
        )
        .await
        .unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("Next: file://file.txt?offset=2&limit=1&line_numbers=true"));
    }
}
