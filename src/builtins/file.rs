use crate::plugin::{Plugin, PluginHost};
use crate::prompts;
use crate::protocol::{Protocol, ProtocolContext, ProtocolDescriptor, ProtocolRequest};
use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use tokio::fs;

const DEFAULT_LIMIT: usize = 200;
const MAX_LIMIT: usize = 2_000;

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
            return Ok(prompts::FILE_HELP.as_bytes().to_vec());
        }

        let (target, query) = split_query(request.target);
        let path = resolve_path(&self.cwd, target);
        let metadata = fs::metadata(&path)
            .await
            .with_context(|| format!("cannot read {}", path.display()))?;
        let range = Range::parse(query)?;
        if metadata.is_dir() {
            read_directory(&path, range).await
        } else if metadata.is_file() {
            read_file(&path, request.uri, range).await
        } else {
            bail!("not a regular file or directory: {}", path.display())
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
}

impl Range {
    fn parse(query: Option<&str>) -> Result<Self> {
        let mut offset = 1_usize;
        let mut limit = DEFAULT_LIMIT;
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
                _ => bail!("unknown file query parameter: {key}"),
            }
        }
        Ok(Self { offset, limit })
    }
}

async fn read_file(path: &Path, uri: &str, range: Range) -> Result<Vec<u8>> {
    let content = fs::read(path)
        .await
        .with_context(|| format!("cannot read {}", path.display()))?;
    let content = String::from_utf8_lossy(&content);
    let lines = content.lines().collect::<Vec<_>>();
    let start = range.offset.saturating_sub(1).min(lines.len());
    let end = start.saturating_add(range.limit).min(lines.len());
    let width = end.max(1).to_string().len();
    let mut output = String::new();
    for (index, line) in lines[start..end].iter().enumerate() {
        let line_number = start + index + 1;
        let _ = writeln!(output, "{line_number:>width$} │ {line}");
    }

    if end < lines.len() {
        let base = uri.split_once('?').map_or(uri, |(base, _)| base);
        let _ = writeln!(
            output,
            "\n[{} more lines]\nNext: {}?offset={}&limit={}",
            lines.len() - end,
            base,
            end + 1,
            range.limit
        );
    }
    Ok(output.into_bytes())
}

async fn read_directory(path: &Path, range: Range) -> Result<Vec<u8>> {
    let mut directory = fs::read_dir(path)
        .await
        .with_context(|| format!("cannot list {}", path.display()))?;
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
    fn custom_targets_are_not_percent_decoded() {
        let cwd = Path::new("/tmp/root");
        assert_eq!(resolve_path(cwd, "a%20b"), Path::new("/tmp/root/a%20b"));
    }

    #[test]
    fn ranges_are_one_based_and_bounded() {
        let range = Range::parse(Some("offset=0&limit=99999")).unwrap();
        assert_eq!(range.offset, 1);
        assert_eq!(range.limit, MAX_LIMIT);
    }
}
