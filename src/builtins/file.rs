use super::normalized_line_view;
use crate::config::display_path;
use crate::plugin::{
    Plugin, PluginHost, TuiCompletionContext, TuiCompletionItem, TuiCompletionProvider,
    TuiCompletions, TuiTextPosition, TuiTextRange,
};
use crate::protocol::{
    Protocol, ProtocolContext, ProtocolDescriptor, ProtocolImage, ProtocolImageMediaType,
    ProtocolOutput, ProtocolRequest,
};
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use ignore::{DirEntry, WalkBuilder, overrides::OverrideBuilder};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::cmp::Reverse;
use std::fmt::Write as _;
use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};
use tokio::fs;

const DEFAULT_LIMIT: usize = 200;
const MAX_LIMIT: usize = 2_000;
const MAX_COMPLETIONS: usize = 20;
const MAX_SCANNED_FILES: usize = 50_000;
const TAIL_SCAN_CHUNK: usize = 64 * 1024;

fn help(cwd: &Path) -> String {
    format!(
        r#"# file

Read text files, images, and directories.

Current working directory: `file://{}`

- Use `file://<path>` to read a file or directory. Replace `<path>` with a
  project-relative path or an absolute filesystem path; relative paths resolve
  from the current working directory. On Unix, `~` and paths beginning with
  `~/` resolve from the current user's home directory; `~user` is not expanded.
- PNG, JPEG, GIF, and WebP files are detected from their contents and returned
  as images for models that accept image input. Image reads take no input.
- Pass `input` fields to bound or shape a read. Every field is optional;
  unknown fields are rejected:
  - `offset` (integer): one-based starting line or directory-entry position.
  - `limit` (integer): maximum number of lines or entries to return; the
    default is 200 and the maximum is 2000.
  - `tail` (integer): efficiently read the last lines of a text file; the
    maximum is 2000. `tail` cannot be combined with `offset`, `limit`, or
    `glob`.
  - `line_numbers` (boolean): prefix file content with its original one-based
    line numbers. Line numbers are disabled by default and cannot be combined
    with `glob`.
  - `glob` (string): with a directory read, list matching files recursively
    with standard ignore rules. Patterns are relative to that directory. A
    glob scans at most 50000 files; narrow the root for larger trees.
- Out-of-range numeric values are clamped to their bounds.
- Paginated file, directory, and glob reads return an exact `Next:` step.
  Empty directories return `No entries.` and empty globs return `No matches.`.
- Full outputs saved by the system are exposed as `file://` addresses.

Examples:

{{"read": "file://src/main.rs"}}

{{"read": "file://src/main.rs", "input": {{"offset": 401, "limit": 200, "line_numbers": true}}}}

{{"read": "file://logs/app.log", "input": {{"tail": 50}}}}

{{"read": "file://src", "input": {{"glob": "**/*.rs"}}}}
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

    async fn read_request(&self, request: ProtocolRequest<'_>) -> Result<ProtocolOutput> {
        if request.target == "help" {
            request.reject_input()?;
            return Ok(help(&self.cwd).into());
        }

        let path = resolve_path(&self.cwd, request.target)?;
        let input = request.input_struct::<ReadInput>()?;
        let range = Range::from_input(&input)?;
        let has_input = !request.input.is_empty();
        let metadata = fs::metadata(&path)
            .await
            .with_context(|| format!("cannot read {}", display_path(&path)))?;
        if let Some(pattern) = range.glob.clone() {
            if !metadata.is_dir() {
                bail!("file glob root is not a directory: {}", display_path(&path));
            }
            if range.line_numbers {
                bail!("line_numbers is not supported with file glob");
            }
            return Ok(read_glob(&self.cwd, &path, &pattern, request.uri, range)
                .await?
                .into());
        }
        if metadata.is_dir() {
            if range.tail.is_some() {
                bail!("tail is only supported for text file reads");
            }
            Ok(read_directory(&path, request.uri, range).await?.into())
        } else if metadata.is_file() {
            read_file(&path, request.uri, has_input, range).await
        } else {
            bail!("not a regular file or directory: {}", display_path(&path))
        }
    }
}

impl Plugin for FileProtocol {
    fn protocol_descriptors(&self) -> Vec<ProtocolDescriptor> {
        vec![self.descriptor()]
    }

    fn register(&self, host: &mut PluginHost<'_>) -> Result<()> {
        host.protocols.register(self.clone())?;
        host.tui.register_completion("files", self.clone())
    }
}

#[async_trait]
impl TuiCompletionProvider for FileProtocol {
    async fn complete(&self, context: &TuiCompletionContext) -> Result<Option<TuiCompletions>> {
        let Some((start, query)) = file_reference_query(context) else {
            return Ok(None);
        };
        let cwd = self.cwd.clone();
        let items = tokio::task::spawn_blocking(move || file_suggestions(&cwd, &query))
            .await
            .context("file completion worker stopped unexpectedly")??;
        Ok((!items.is_empty()).then_some(TuiCompletions {
            replacement: TuiTextRange {
                start: TuiTextPosition {
                    line: context.cursor.line,
                    column: start,
                },
                end: context.cursor,
            },
            items,
        }))
    }
}

fn file_reference_query(context: &TuiCompletionContext) -> Option<(usize, String)> {
    let line = context.lines.get(context.cursor.line)?;
    let prefix = line.chars().take(context.cursor.column).collect::<String>();
    let start = prefix
        .chars()
        .enumerate()
        .filter_map(|(index, character)| character.is_whitespace().then_some(index + 1))
        .last()
        .unwrap_or_default();
    let token = prefix.chars().skip(start).collect::<String>();
    let query = token.strip_prefix('@')?;
    if query.starts_with('@') || query.contains(['"', '\'']) {
        return None;
    }
    Some((
        start,
        query.strip_prefix("file://").unwrap_or(query).to_string(),
    ))
}

fn file_suggestions(cwd: &Path, query: &str) -> Result<Vec<TuiCompletionItem>> {
    let query = query.replace('\\', "/").to_lowercase();
    let root = cwd.to_path_buf();
    let root_for_filter = root.clone();
    let mut walker = WalkBuilder::new(cwd);
    walker
        .standard_filters(true)
        .hidden(false)
        .filter_entry(move |entry| include_completion_entry(&root_for_filter, entry));
    let mut candidates = Vec::new();
    for entry in walker.build().take(MAX_SCANNED_FILES) {
        let entry =
            entry.with_context(|| format!("cannot scan files under {}", display_path(cwd)))?;
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let Ok(relative) = entry.path().strip_prefix(cwd) else {
            continue;
        };
        let path = completion_path(relative);
        if path.is_empty() || path.contains(['\n', '\r']) || path.contains(['"', '\'']) {
            continue;
        }
        let Some(score) = file_match_score(&path.to_lowercase(), &query) else {
            continue;
        };
        candidates.push((score, Reverse(path.matches('/').count()), path));
    }
    candidates.sort_by(|left, right| {
        left.0
            .cmp(&right.0)
            .then_with(|| right.1.cmp(&left.1))
            .then_with(|| left.2.cmp(&right.2))
    });
    Ok(candidates
        .into_iter()
        .take(MAX_COMPLETIONS)
        .map(|(_, _, path)| {
            let insert_path = if path.chars().any(char::is_whitespace) {
                format!("@\"file://{path}\" ")
            } else {
                format!("@file://{path} ")
            };
            let description = Path::new(&path)
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .map_or_else(|| "project file".to_string(), display_path);
            TuiCompletionItem {
                insert_text: insert_path,
                label: path,
                description,
            }
        })
        .collect())
}

fn include_completion_entry(root: &Path, entry: &DirEntry) -> bool {
    entry.path() == root
        || entry
            .path()
            .strip_prefix(root)
            .ok()
            .and_then(|path| path.components().next())
            .is_none_or(|component| component.as_os_str() != ".git")
}

fn completion_path(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn file_match_score(path: &str, query: &str) -> Option<usize> {
    if query.is_empty() {
        return Some(path.matches('/').count() * 20 + path.len());
    }
    if path == query {
        return Some(0);
    }
    if path.starts_with(query) {
        return Some(1);
    }
    if let Some(position) = path.find(query) {
        return Some(position + 2);
    }
    let mut cursor = 0usize;
    let mut score = 100usize;
    for needle in query.chars() {
        let suffix = path.get(cursor..)?;
        let position = suffix.find(needle)?;
        score = score.saturating_add(position);
        cursor = cursor.saturating_add(position + needle.len_utf8());
    }
    Some(score)
}

#[async_trait]
impl Protocol for FileProtocol {
    fn descriptor(&self) -> ProtocolDescriptor {
        ProtocolDescriptor {
            name: "file".to_string(),
            description: "Read text files, supported images, and directory listings with bounded ranges and efficient text tails."
                .to_string(),
            can_read: true,
            can_exec: false,
        }
    }

    async fn read(
        &self,
        request: ProtocolRequest<'_>,
        _context: ProtocolContext,
    ) -> Result<ProtocolOutput> {
        self.read_request(request).await
    }
}

pub(crate) fn resolve_path(cwd: &Path, target: &str) -> Result<PathBuf> {
    #[cfg(unix)]
    if target == "~" || target.starts_with("~/") {
        let home = dirs::home_dir().context("cannot determine the home directory")?;
        let relative = Path::new(target)
            .strip_prefix("~")
            .expect("home-relative path starts with a tilde component");
        return Ok(home.join(relative));
    }

    let path = Path::new(target);
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else if target.is_empty() {
        Ok(cwd.to_path_buf())
    } else {
        Ok(cwd.join(path))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadInput {
    offset: Option<usize>,
    limit: Option<usize>,
    tail: Option<usize>,
    line_numbers: Option<bool>,
    glob: Option<String>,
}

#[derive(Clone)]
struct Range {
    offset: usize,
    limit: usize,
    tail: Option<usize>,
    line_numbers: bool,
    glob: Option<String>,
}

impl Range {
    fn from_input(input: &ReadInput) -> Result<Self> {
        let tail = input.tail.map(|tail| tail.clamp(1, MAX_LIMIT));
        if tail.is_some()
            && [
                input.offset.is_some(),
                input.limit.is_some(),
                input.glob.is_some(),
            ]
            .into_iter()
            .any(|set| set)
        {
            bail!("tail cannot be combined with offset, limit, or glob");
        }
        if let Some(pattern) = &input.glob
            && pattern.is_empty()
        {
            bail!("file glob pattern cannot be empty");
        }
        Ok(Self {
            offset: input.offset.unwrap_or(1).max(1),
            limit: input.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT),
            tail,
            line_numbers: input.line_numbers.unwrap_or(false),
            glob: input.glob.clone(),
        })
    }
}

/// The exact continuation step appended after a truncated read.
fn next_step(uri: &str, input: Value) -> String {
    let mut step = Map::new();
    step.insert("read".to_string(), Value::String(uri.to_string()));
    if let Some(fields) = input.as_object()
        && !fields.is_empty()
    {
        step.insert("input".to_string(), input);
    }
    format!("Next: {}", Value::Object(step))
}

async fn read_glob(
    cwd: &Path,
    root: &Path,
    pattern: &str,
    uri: &str,
    range: Range,
) -> Result<Vec<u8>> {
    let cwd = cwd.to_path_buf();
    let root = root.to_path_buf();
    let pattern = pattern.to_string();
    let worker_pattern = pattern.clone();
    let entries = tokio::task::spawn_blocking(move || glob_entries(&cwd, &root, &worker_pattern))
        .await
        .context("file glob worker stopped unexpectedly")??;
    if entries.is_empty() {
        return Ok(b"No matches.".to_vec());
    }
    let start = range.offset.saturating_sub(1).min(entries.len());
    let end = start.saturating_add(range.limit).min(entries.len());
    let mut output = entries[start..end].join("\n");
    if !output.is_empty() {
        output.push('\n');
    }
    if end < entries.len() {
        let continuation = next_step(
            uri,
            json!({"glob": pattern, "offset": end + 1, "limit": range.limit}),
        );
        let _ = writeln!(output, "\n{continuation}");
    }
    Ok(output.into_bytes())
}

fn glob_entries(cwd: &Path, root: &Path, pattern: &str) -> Result<Vec<String>> {
    let mut overrides = OverrideBuilder::new(root);
    overrides
        .add(pattern)
        .with_context(|| format!("invalid file glob pattern: {pattern}"))?;
    let overrides = overrides.build()?;
    let mut walker = WalkBuilder::new(root);
    walker.standard_filters(true);
    let mut entries = Vec::new();
    let mut scanned_files = 0usize;
    for entry in walker.build() {
        let entry =
            entry.with_context(|| format!("cannot scan files under {}", display_path(root)))?;
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        scanned_files += 1;
        if scanned_files > MAX_SCANNED_FILES {
            bail!("file glob scan exceeds {MAX_SCANNED_FILES} files; narrow the root directory");
        }
        if !overrides.matched(entry.path(), false).is_whitelist() {
            continue;
        }
        entries.push(glob_output_path(cwd, entry.path()));
    }
    entries.sort_unstable();
    Ok(entries)
}

fn glob_output_path(cwd: &Path, path: &Path) -> String {
    path.strip_prefix(cwd)
        .map_or_else(|_| display_path(path).replace('\\', "/"), completion_path)
}

async fn read_file(
    path: &Path,
    uri: &str,
    has_input: bool,
    range: Range,
) -> Result<ProtocolOutput> {
    if let Some(tail) = range.tail {
        return read_file_tail(path, tail, range.line_numbers).await;
    }
    let content = fs::read(path)
        .await
        .with_context(|| format!("cannot read {}", display_path(path)))?;
    if let Some(media_type) = ProtocolImageMediaType::detect(&content) {
        if has_input {
            bail!("file input is not supported for image reads");
        }
        let size = content.len();
        let output = format!(
            "Read image {} ({}, {} bytes).",
            display_path(path),
            media_type.mime_type(),
            size
        );
        return Ok(ProtocolOutput::new(
            output.into_bytes(),
            None,
            vec![ProtocolImage::new(content, media_type)],
        ));
    }
    let content = String::from_utf8_lossy(&content);
    let content = content.strip_prefix('\u{feff}').unwrap_or(&content);
    let content = normalized_line_view(content);
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
        let mut input = json!({"offset": end + 1, "limit": range.limit});
        if range.line_numbers {
            input["line_numbers"] = Value::Bool(true);
        }
        let _ = writeln!(output, "\n{}", next_step(uri, input));
    }
    Ok(output.into_bytes().into())
}

struct TailRead {
    content: Vec<u8>,
    total_lines: Option<usize>,
    starts_at_beginning: bool,
}

async fn read_file_tail(path: &Path, count: usize, line_numbers: bool) -> Result<ProtocolOutput> {
    let path = path.to_path_buf();
    let tail = tokio::task::spawn_blocking(move || read_tail(&path, count, line_numbers))
        .await
        .context("file tail worker stopped unexpectedly")??;
    let content = String::from_utf8_lossy(&tail.content);
    let content = if tail.starts_at_beginning {
        content.strip_prefix('\u{feff}').unwrap_or(&content)
    } else {
        &content
    };
    let content = normalized_line_view(content);
    let lines = content.lines().collect::<Vec<_>>();
    let first_line = tail
        .total_lines
        .map_or(1, |total| total.saturating_sub(lines.len()) + 1);
    let width = first_line
        .saturating_add(lines.len().saturating_sub(1))
        .max(1)
        .to_string()
        .len();
    let mut output = String::new();
    for (index, line) in lines.iter().enumerate() {
        if line_numbers {
            let line_number = first_line + index;
            let _ = writeln!(output, "{line_number:>width$} │ {line}");
        } else {
            let _ = writeln!(output, "{line}");
        }
    }
    Ok(output.into_bytes().into())
}

fn read_tail(path: &Path, count: usize, count_all_lines: bool) -> Result<TailRead> {
    let mut file =
        std::fs::File::open(path).with_context(|| format!("cannot read {}", display_path(path)))?;
    let length = file
        .metadata()
        .with_context(|| format!("cannot inspect {}", display_path(path)))?
        .len();
    let mut header = [0_u8; 12];
    let header_length = length.min(header.len() as u64) as usize;
    file.read_exact(&mut header[..header_length])
        .with_context(|| format!("cannot read {}", display_path(path)))?;
    if ProtocolImageMediaType::detect(&header[..header_length]).is_some() {
        bail!("file input is not supported for image reads");
    }

    let mut position = length;
    let mut buffer = vec![0_u8; TAIL_SCAN_CHUNK];
    let mut separator_count = 0_usize;
    let mut start = None;
    let mut at_end = true;
    let mut skip_cr_before_lf = false;
    'chunks: while position > 0 {
        let chunk_start = position.saturating_sub(TAIL_SCAN_CHUNK as u64);
        let chunk_length = (position - chunk_start) as usize;
        file.seek(SeekFrom::Start(chunk_start))
            .with_context(|| format!("cannot read {}", display_path(path)))?;
        file.read_exact(&mut buffer[..chunk_length])
            .with_context(|| format!("cannot read {}", display_path(path)))?;

        for index in (0..chunk_length).rev() {
            let byte = buffer[index];
            if skip_cr_before_lf {
                skip_cr_before_lf = false;
                if byte == b'\r' {
                    continue;
                }
            }
            if at_end {
                at_end = false;
                if byte == b'\n' {
                    skip_cr_before_lf = true;
                    continue;
                }
                if byte == b'\r' {
                    continue;
                }
            }

            let separator_end = match byte {
                b'\n' => {
                    skip_cr_before_lf = true;
                    Some(chunk_start + index as u64 + 1)
                }
                b'\r' => Some(chunk_start + index as u64 + 1),
                _ => None,
            };
            if let Some(separator_end) = separator_end {
                separator_count = separator_count.saturating_add(1);
                if separator_count == count {
                    start = Some(separator_end);
                    if !count_all_lines {
                        break 'chunks;
                    }
                }
            }
        }
        position = chunk_start;
    }

    let start = start.unwrap_or(0);
    file.seek(SeekFrom::Start(start))
        .with_context(|| format!("cannot read {}", display_path(path)))?;
    let mut content = Vec::new();
    file.take(length - start)
        .read_to_end(&mut content)
        .with_context(|| format!("cannot read {}", display_path(path)))?;
    Ok(TailRead {
        content,
        total_lines: count_all_lines.then_some(if length == 0 {
            0
        } else {
            separator_count.saturating_add(1)
        }),
        starts_at_beginning: start == 0,
    })
}

async fn read_directory(path: &Path, uri: &str, range: Range) -> Result<Vec<u8>> {
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
    if entries.is_empty() {
        return Ok(b"No entries.".to_vec());
    }
    let start = range.offset.saturating_sub(1).min(entries.len());
    let end = start.saturating_add(range.limit).min(entries.len());
    let mut output = entries[start..end].join("\n");
    if !output.is_empty() {
        output.push('\n');
    }
    if end < entries.len() {
        let continuation = next_step(uri, json!({"offset": end + 1, "limit": range.limit}));
        let _ = writeln!(output, "\n{continuation}");
    }
    Ok(output.into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn parse_range(value: Value) -> Result<Range> {
        Range::from_input(&serde_json::from_value(value)?)
    }

    #[test]
    fn help_documents_input_fields_and_examples() {
        let help = help(Path::new(r"\\?\C:\Users\4fu\project"));
        assert!(help.contains(r"Current working directory: `file://C:\Users\4fu\project`"));
        assert!(help.contains("`file://<path>`"));
        assert!(help.contains("`offset` (integer): one-based starting line"));
        assert!(help.contains("the\n    default is 200 and the maximum is 2000"));
        assert!(help.contains("`tail` (integer): efficiently read the last lines"));
        assert!(help.contains("`tail` cannot be combined with `offset`, `limit`, or"));
        assert!(help.contains("`line_numbers` (boolean): prefix file content"));
        assert!(help.contains("cannot be combined\n    with `glob`"));
        assert!(help.contains("`glob` (string): with a directory read"));
        assert!(help.contains("at most 50000 files"));
        assert!(help.contains("Out-of-range numeric values are clamped to their bounds"));
        assert!(help.contains("PNG, JPEG, GIF, and WebP"));
        assert!(help.contains("Image reads take no input."));
        assert!(help.contains("paths beginning with\n  `~/` resolve"));
        assert!(help.contains("`~user` is not expanded"));
        assert!(help.contains(r#"{"read": "file://src/main.rs"}"#));
        assert!(help.contains(r#"{"read": "file://src/main.rs", "input": {"offset": 401, "limit": 200, "line_numbers": true}}"#));
        assert!(help.contains(r#"{"read": "file://logs/app.log", "input": {"tail": 50}}"#));
        assert!(help.contains(r#"{"read": "file://src", "input": {"glob": "**/*.rs"}}"#));
        assert!(!help.contains("header"));
        assert!(!help.contains("body"));
    }

    #[test]
    fn custom_targets_are_not_percent_decoded() {
        let cwd = Path::new("/tmp/root");
        assert_eq!(
            resolve_path(cwd, "a%20b").unwrap(),
            Path::new("/tmp/root/a%20b")
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_home_relative_paths_expand_without_interpreting_usernames() {
        let cwd = Path::new("/tmp/root");
        let home = dirs::home_dir().unwrap();

        assert_eq!(resolve_path(cwd, "~").unwrap(), home);
        assert_eq!(
            resolve_path(cwd, "~/notes/file.txt").unwrap(),
            home.join("notes/file.txt")
        );
        assert_eq!(
            resolve_path(cwd, "~someone/file.txt").unwrap(),
            cwd.join("~someone/file.txt")
        );
        assert_eq!(
            resolve_path(cwd, "notes/~/file.txt").unwrap(),
            cwd.join("notes/~/file.txt")
        );
    }

    #[cfg(not(unix))]
    #[test]
    fn tilde_paths_remain_relative_on_non_unix_platforms() {
        let cwd = Path::new("root");

        assert_eq!(resolve_path(cwd, "~").unwrap(), cwd.join("~"));
        assert_eq!(
            resolve_path(cwd, "~/notes/file.txt").unwrap(),
            cwd.join("~/notes/file.txt")
        );
    }

    #[tokio::test]
    async fn help_rejects_any_input_field() {
        let directory = tempfile::tempdir().unwrap();
        let input = input_map(json!({"offset": 1}));
        let error = FileProtocol::new(directory.path())
            .read(
                request("file://help", "help", &input),
                ProtocolContext::new(crate::task::TaskManager::new()),
            )
            .await
            .unwrap_err();

        assert!(error.to_string().contains("takes no input fields"));
    }

    #[tokio::test]
    async fn unknown_input_fields_are_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let input = input_map(json!({"lines": 10}));
        let error = FileProtocol::new(directory.path())
            .read(
                ProtocolRequest {
                    uri: "file://src/main.rs",
                    target: "src/main.rs",
                    input: &input,
                },
                ProtocolContext::new(crate::task::TaskManager::new()),
            )
            .await
            .unwrap_err();

        assert!(format!("{error:#}").contains("unknown field `lines`"));
        assert!(format!("{error:#}").contains("expected one of"));
    }

    #[tokio::test]
    async fn directories_reject_tail_reads() {
        let directory = tempfile::tempdir().unwrap();
        let input = input_map(json!({"tail": 1}));

        let error = FileProtocol::new(directory.path())
            .read(
                request("file://", "", &input),
                ProtocolContext::new(crate::task::TaskManager::new()),
            )
            .await
            .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("tail is only supported for text file reads")
        );
    }

    #[test]
    fn ranges_are_one_based_and_bounded() {
        let range = parse_range(json!({"offset": 0, "limit": 99999})).unwrap();
        assert_eq!(range.offset, 1);
        assert_eq!(range.limit, MAX_LIMIT);
        assert_eq!(range.tail, None);
        assert!(!range.line_numbers);
        assert!(serde_json::from_value::<ReadInput>(json!({"offset": "2"})).is_err());
        assert!(serde_json::from_value::<ReadInput>(json!({"limit": 1.5})).is_err());
        assert!(serde_json::from_value::<ReadInput>(json!({"line_numbers": "true"})).is_err());
        assert!(serde_json::from_value::<ReadInput>(json!({"line_numbers": 1})).is_err());
    }

    #[test]
    fn tail_ranges_are_bounded() {
        assert_eq!(parse_range(json!({"tail": 0})).unwrap().tail, Some(1));
        assert_eq!(
            parse_range(json!({"tail": 99999})).unwrap().tail,
            Some(MAX_LIMIT)
        );
        assert!(parse_range(json!({"tail": 2, "line_numbers": true})).is_ok());
        assert!(parse_range(json!({"tail": 2, "offset": 1})).is_err());
        assert!(parse_range(json!({"limit": 2, "tail": 2})).is_err());
        assert!(parse_range(json!({"glob": "*.log", "tail": 2})).is_err());
        assert!(parse_range(json!({"glob": ""})).is_err());
    }

    #[test]
    fn glob_input_values_are_used_verbatim() {
        let range = parse_range(json!({"glob": "reports + draft/?&#%*.md"})).unwrap();

        assert_eq!(range.glob.as_deref(), Some("reports + draft/?&#%*.md"));
    }

    #[test]
    fn line_numbers_are_opt_in() {
        assert!(
            parse_range(json!({"line_numbers": true}))
                .unwrap()
                .line_numbers
        );
        assert!(!parse_range(json!({})).unwrap().line_numbers);
        assert!(
            !parse_range(json!({"line_numbers": false}))
                .unwrap()
                .line_numbers
        );
    }

    #[test]
    fn file_completion_handles_tokens_and_leaves_double_at_for_other_providers() {
        let context = TuiCompletionContext {
            cwd: PathBuf::from("/project"),
            session_id: "session".to_string(),
            lines: vec!["inspect @src/ma".to_string()],
            cursor: TuiTextPosition {
                line: 0,
                column: 15,
            },
        };
        assert_eq!(
            file_reference_query(&context),
            Some((8, "src/ma".to_string()))
        );

        let mut session_context = context;
        session_context.lines = vec!["inspect @@old".to_string()];
        session_context.cursor.column = 13;
        assert_eq!(file_reference_query(&session_context), None);

        let uri_context = TuiCompletionContext {
            lines: vec!["inspect @file://src/ma".to_string()],
            cursor: TuiTextPosition {
                line: 0,
                column: 22,
            },
            ..session_context
        };
        assert_eq!(
            file_reference_query(&uri_context),
            Some((8, "src/ma".to_string()))
        );
    }

    #[test]
    fn file_completion_quotes_paths_with_spaces() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("design notes.md"), "notes").unwrap();

        let items = file_suggestions(directory.path(), "design").unwrap();

        assert_eq!(items[0].label, "design notes.md");
        assert_eq!(items[0].insert_text, "@\"file://design notes.md\" ");
    }

    #[tokio::test]
    async fn file_output_only_includes_line_numbers_when_requested() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("file.txt");
        fs::write(&path, "alpha\nbeta\n").await.unwrap();

        let plain = read_file(
            &path,
            "file://file.txt",
            false,
            parse_range(json!({})).unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(
            String::from_utf8(plain.text_bytes().to_vec()).unwrap(),
            "alpha\nbeta\n"
        );

        let numbered = read_file(
            &path,
            "file://file.txt",
            true,
            parse_range(json!({"line_numbers": true})).unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(
            String::from_utf8(numbered.text_bytes().to_vec()).unwrap(),
            "1 │ alpha\n2 │ beta\n"
        );
    }

    #[tokio::test]
    async fn file_output_hides_bom_and_normalizes_line_endings() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("file.txt");
        fs::write(&path, "\u{feff}alpha\r\nbeta\rgamma\r\n")
            .await
            .unwrap();

        let output = read_file(
            &path,
            "file://file.txt",
            false,
            parse_range(json!({})).unwrap(),
        )
        .await
        .unwrap();

        assert_eq!(
            String::from_utf8(output.text_bytes().to_vec()).unwrap(),
            "alpha\nbeta\ngamma\n"
        );
    }

    #[tokio::test]
    async fn tail_returns_the_last_lines_with_or_without_a_final_newline() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("file.txt");

        for content in ["zero\none\ntwo", "zero\none\ntwo\n"] {
            fs::write(&path, content).await.unwrap();
            let output = read_file(
                &path,
                "file://file.txt",
                true,
                parse_range(json!({"tail": 2})).unwrap(),
            )
            .await
            .unwrap();

            assert_eq!(
                String::from_utf8(output.text_bytes().to_vec()).unwrap(),
                "one\ntwo\n"
            );
        }
    }

    #[tokio::test]
    async fn tail_matches_normalized_line_semantics_for_empty_lines() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("file.txt");

        for (content, expected) in [
            ("", ""),
            ("alpha", "alpha\n"),
            ("alpha\n", "alpha\n"),
            ("alpha\n\n", "\n"),
            ("\r\n\r\n", "\n"),
            ("zero\n\u{feff}one", "\u{feff}one\n"),
        ] {
            fs::write(&path, content).await.unwrap();
            let output = read_file(
                &path,
                "file://file.txt",
                true,
                parse_range(json!({"tail": 1})).unwrap(),
            )
            .await
            .unwrap();

            assert_eq!(
                String::from_utf8(output.text_bytes().to_vec()).unwrap(),
                expected
            );
        }
    }

    #[tokio::test]
    async fn tail_normalizes_crlf_and_lone_cr_line_endings() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("file.txt");
        fs::write(&path, "\u{feff}zero\r\none\rtwo\r\n")
            .await
            .unwrap();

        let output = read_file(
            &path,
            "file://file.txt",
            true,
            parse_range(json!({"tail": 20})).unwrap(),
        )
        .await
        .unwrap();

        assert_eq!(
            String::from_utf8(output.text_bytes().to_vec()).unwrap(),
            "zero\none\ntwo\n"
        );
    }

    #[tokio::test]
    async fn tail_preserves_original_line_numbers() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("file.txt");
        let content = (1..=12)
            .map(|line| format!("line {line}\n"))
            .collect::<String>();
        fs::write(&path, content).await.unwrap();

        let output = read_file(
            &path,
            "file://file.txt",
            true,
            parse_range(json!({"tail": 2, "line_numbers": true})).unwrap(),
        )
        .await
        .unwrap();

        assert_eq!(
            String::from_utf8(output.text_bytes().to_vec()).unwrap(),
            "11 │ line 11\n12 │ line 12\n"
        );
    }

    #[tokio::test]
    async fn oversized_tail_returns_the_complete_file_without_a_continuation() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("file.txt");
        fs::write(&path, "zero\none\ntwo\n").await.unwrap();

        let output = read_file(
            &path,
            "file://file.txt",
            true,
            parse_range(json!({"tail": 20})).unwrap(),
        )
        .await
        .unwrap();
        let output = String::from_utf8(output.text_bytes().to_vec()).unwrap();

        assert_eq!(output, "zero\none\ntwo\n");
        assert!(!output.contains("Next:"));
    }

    #[tokio::test]
    async fn tail_scans_across_multiple_chunks() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("file.txt");
        let suffix = "x".repeat(TAIL_SCAN_CHUNK - 1);
        let content = format!("before\nmiddle\r\n{suffix}");
        assert!(content.len() > TAIL_SCAN_CHUNK);
        fs::write(&path, &content).await.unwrap();

        let output = read_file(
            &path,
            "file://file.txt",
            true,
            parse_range(json!({"tail": 2})).unwrap(),
        )
        .await
        .unwrap();
        let expected = format!("middle\n{suffix}\n");

        assert_eq!(
            String::from_utf8(output.text_bytes().to_vec()).unwrap(),
            expected
        );
    }

    #[tokio::test]
    async fn pagination_preserves_the_line_number_option() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("file.txt");
        fs::write(&path, "alpha\nbeta\n").await.unwrap();

        let output = read_file(
            &path,
            "file://file.txt",
            true,
            parse_range(json!({"offset": 1, "limit": 1, "line_numbers": true})).unwrap(),
        )
        .await
        .unwrap();
        let output = String::from_utf8(output.text_bytes().to_vec()).unwrap();
        assert!(output.contains(
            r#"Next: {"read":"file://file.txt","input":{"offset":2,"limit":1,"line_numbers":true}}"#
        ));
    }

    #[tokio::test]
    async fn image_reads_return_typed_content_detected_from_bytes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("screenshot.bin");
        let bytes = b"\x89PNG\r\n\x1a\nimage-data".to_vec();
        fs::write(&path, &bytes).await.unwrap();

        let output = read_file(
            &path,
            "file://screenshot.bin",
            false,
            parse_range(json!({})).unwrap(),
        )
        .await
        .unwrap();

        assert!(String::from_utf8_lossy(output.text_bytes()).contains("image/png"));
        assert_eq!(output.images().len(), 1);
        assert_eq!(output.images()[0].bytes(), bytes);
        assert_eq!(output.images()[0].media_type(), ProtocolImageMediaType::Png);

        let error = read_file(
            &path,
            "file://screenshot.bin",
            true,
            parse_range(json!({"limit": 1})).unwrap(),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("not supported for image reads"));

        let error = read_file(
            &path,
            "file://screenshot.bin",
            true,
            parse_range(json!({"tail": 1})).unwrap(),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("not supported for image reads"));
    }

    #[test]
    fn glob_entries_are_sorted_and_honor_standard_ignore_rules() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(directory.path().join("nested")).unwrap();
        std::fs::create_dir(directory.path().join(".git")).unwrap();
        std::fs::write(directory.path().join("z.rs"), "z").unwrap();
        std::fs::write(directory.path().join("a.rs"), "a").unwrap();
        std::fs::write(directory.path().join(".hidden.rs"), "hidden").unwrap();
        std::fs::write(directory.path().join("nested/b.rs"), "b").unwrap();
        std::fs::write(directory.path().join("nested/ignored.rs"), "ignored").unwrap();
        std::fs::write(directory.path().join(".gitignore"), "nested/ignored.rs\n").unwrap();

        assert_eq!(
            glob_entries(directory.path(), directory.path(), "**/*.rs").unwrap(),
            ["a.rs", "nested/b.rs", "z.rs"]
        );
        assert!(glob_entries(directory.path(), directory.path(), "[").is_err());
    }

    #[test]
    fn glob_output_preserves_absolute_paths_outside_the_working_directory() {
        let cwd = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let path = outside.path().join("outside.rs");
        std::fs::write(&path, "outside").unwrap();

        let entries = glob_entries(cwd.path(), outside.path(), "**/*.rs").unwrap();

        assert_eq!(entries, [display_path(&path).replace('\\', "/")]);
        #[cfg(unix)]
        assert!(!entries[0].starts_with("//"));
    }

    #[tokio::test]
    async fn glob_pagination_returns_a_complete_continuation_step() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir_all(directory.path().join("nested"))
            .await
            .unwrap();
        fs::write(directory.path().join("a.rs"), "a").await.unwrap();
        fs::write(directory.path().join("nested/b.rs"), "b")
            .await
            .unwrap();
        let range = parse_range(json!({"glob": "**/*.rs", "offset": 1, "limit": 1})).unwrap();

        let output = read_glob(
            directory.path(),
            directory.path(),
            "**/*.rs",
            "file://",
            range,
        )
        .await
        .unwrap();
        let output = String::from_utf8(output).unwrap();

        assert!(output.starts_with("a.rs\n"));
        assert!(output.contains(
            r#"Next: {"read":"file://","input":{"glob":"**/*.rs","offset":2,"limit":1}}"#
        ));
        assert!(!output.contains("more matches"));
    }

    #[tokio::test]
    async fn glob_pagination_preserves_the_pattern_in_the_continuation_step() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("a&b.rs"), "a")
            .await
            .unwrap();
        fs::write(directory.path().join("c&b.rs"), "c")
            .await
            .unwrap();
        let pattern = "*&b.rs";

        let output = read_glob(
            directory.path(),
            directory.path(),
            pattern,
            "file://",
            parse_range(json!({"glob": pattern, "offset": 1, "limit": 1})).unwrap(),
        )
        .await
        .unwrap();
        let output = String::from_utf8(output).unwrap();

        assert!(output.contains(
            r#"Next: {"read":"file://","input":{"glob":"*&b.rs","offset":2,"limit":1}}"#
        ));
    }

    #[tokio::test]
    async fn empty_globs_and_directories_are_explicit() {
        let directory = tempfile::tempdir().unwrap();

        let glob = read_glob(
            directory.path(),
            directory.path(),
            "**/*.rs",
            "file://",
            parse_range(json!({"glob": "**/*.rs"})).unwrap(),
        )
        .await
        .unwrap();
        let listing = read_directory(directory.path(), "file://", parse_range(json!({})).unwrap())
            .await
            .unwrap();

        assert_eq!(glob, b"No matches.");
        assert_eq!(listing, b"No entries.");
    }

    #[tokio::test]
    async fn directory_pagination_returns_only_the_continuation_step() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("a.txt"), "a")
            .await
            .unwrap();
        fs::write(directory.path().join("b.txt"), "b")
            .await
            .unwrap();

        let output = read_directory(
            directory.path(),
            "file://",
            parse_range(json!({"offset": 1, "limit": 1})).unwrap(),
        )
        .await
        .unwrap();
        let output = String::from_utf8(output).unwrap();

        assert_eq!(
            output,
            "a.txt\n\nNext: {\"read\":\"file://\",\"input\":{\"offset\":2,\"limit\":1}}\n"
        );
        assert!(!output.contains("more entries"));
    }

    #[test]
    fn next_step_omits_an_empty_input_object() {
        assert_eq!(
            next_step("file://src/main.rs", json!({})),
            r#"Next: {"read":"file://src/main.rs"}"#
        );
        assert_eq!(
            next_step("file://src/main.rs", json!({"offset": 2})),
            r#"Next: {"read":"file://src/main.rs","input":{"offset":2}}"#
        );
    }
}
