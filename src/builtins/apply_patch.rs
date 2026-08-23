use super::atomic_write;
use super::file::resolve_path;
use crate::plugin::{Plugin, PluginHost};
use crate::protocol::{Protocol, ProtocolContext, ProtocolDescriptor, ProtocolRequest};
use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use tokio::fs;

const BEGIN_PATCH: &str = "*** Begin Patch";
const END_PATCH: &str = "*** End Patch";
const ADD_FILE: &str = "*** Add File: ";
const DELETE_FILE: &str = "*** Delete File: ";
const UPDATE_FILE: &str = "*** Update File: ";
const MOVE_TO: &str = "*** Move to: ";
const END_OF_FILE: &str = "*** End of File";
const HELP: &str = r#"# apply_patch

Apply a Codex-style multi-file patch and return the final result.

Call `exec` with `apply_patch://apply`. The body must be the patch string itself:

```text
*** Begin Patch
*** Add File: <path>
+<new content>
*** Update File: <path>
@@ <optional landmark>
-<old line>
+<new line>
*** Delete File: <path>
*** End Patch
```

Replace each `<path>` with the project-relative or absolute path for that
operation. Replace the other placeholders with the patch context and content
required by the project. Use `@@` without `<optional landmark>` when no landmark
is needed. An Update File may put `*** Move to: <new path>` immediately after
its header. Update lines begin with a space for context, `-` for removal, or `+`
for addition. `*** End of File` anchors the preceding chunk at EOF. Add File
content lines must all begin with `+`. Relative paths resolve from the startup
working directory; absolute paths are accepted.

Operations run in patch order and each write is atomic, but the complete patch
is not transactional: a later failure does not undo earlier operations. `exec`
returns the final summary after all operations succeed; errors are returned
directly.

`read` supports only `apply_patch://help`.
"#;

#[derive(Clone)]
pub(super) struct ApplyPatchProtocol {
    cwd: PathBuf,
}

impl ApplyPatchProtocol {
    pub(super) fn new(cwd: &Path) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
        }
    }
}

impl Plugin for ApplyPatchProtocol {
    fn protocol_descriptors(&self) -> Vec<ProtocolDescriptor> {
        vec![self.descriptor()]
    }

    fn register(&self, host: &mut PluginHost<'_>) -> Result<()> {
        host.protocols.register(self.clone())
    }
}

#[async_trait]
impl Protocol for ApplyPatchProtocol {
    fn descriptor(&self) -> ProtocolDescriptor {
        ProtocolDescriptor {
            name: "apply_patch".to_string(),
            description: "Apply a Codex-style multi-file patch.".to_string(),
            can_read: true,
            can_exec: true,
        }
    }

    async fn read(
        &self,
        request: ProtocolRequest<'_>,
        _context: ProtocolContext,
    ) -> Result<Vec<u8>> {
        if request.target != "help" {
            bail!("expected apply_patch://help");
        }
        Ok(HELP.as_bytes().to_vec())
    }

    async fn exec(
        &self,
        request: ProtocolRequest<'_>,
        _context: ProtocolContext,
    ) -> Result<Vec<u8>> {
        if !matches!(request.target, "" | "apply") {
            bail!("expected apply_patch://apply");
        }
        let patch = request
            .body
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| anyhow!("apply_patch body must be a patch string"))?;
        Ok(apply_patch(&self.cwd, patch).await?.into_bytes())
    }
}

#[derive(Debug, Eq, PartialEq)]
enum PatchHunk {
    Add {
        path: PathBuf,
        content: String,
    },
    Delete {
        path: PathBuf,
    },
    Update {
        path: PathBuf,
        move_to: Option<PathBuf>,
        chunks: Vec<UpdateChunk>,
    },
}

#[derive(Debug, Default, Eq, PartialEq)]
struct UpdateChunk {
    change_context: Option<String>,
    old_lines: Vec<String>,
    new_lines: Vec<String>,
    end_of_file: bool,
}

impl UpdateChunk {
    fn is_empty(&self) -> bool {
        self.old_lines.is_empty() && self.new_lines.is_empty()
    }

    fn push_context(&mut self, line: &str) {
        self.old_lines.push(line.to_string());
        self.new_lines.push(line.to_string());
    }
}

fn parse_patch(patch: &str) -> Result<Vec<PatchHunk>> {
    let patch = patch.trim();
    let lines = patch
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect::<Vec<_>>();
    if lines.first().is_none_or(|line| line.trim() != BEGIN_PATCH) {
        bail!("the first line of the patch must be '{BEGIN_PATCH}'");
    }

    let mut hunks = Vec::new();
    let mut index = 1;
    let mut ended = false;
    while index < lines.len() {
        let line = lines[index].trim();
        if line == END_PATCH {
            ended = true;
            index += 1;
            break;
        }
        if let Some(path) = line.strip_prefix(ADD_FILE) {
            let path = parse_path(path, index + 1)?;
            index += 1;
            let mut content = String::new();
            while index < lines.len() && !is_operation_or_end(lines[index].trim()) {
                let Some(line) = lines[index].strip_prefix('+') else {
                    bail!(
                        "invalid patch hunk on line {}: Add File lines must start with '+'",
                        index + 1
                    );
                };
                content.push_str(line);
                content.push('\n');
                index += 1;
            }
            hunks.push(PatchHunk::Add { path, content });
            continue;
        }
        if let Some(path) = line.strip_prefix(DELETE_FILE) {
            let path = parse_path(path, index + 1)?;
            index += 1;
            if index < lines.len() && !is_operation_or_end(lines[index].trim()) {
                bail!(
                    "invalid patch hunk on line {}: Delete File cannot contain lines",
                    index + 1
                );
            }
            hunks.push(PatchHunk::Delete { path });
            continue;
        }
        if let Some(path) = line.strip_prefix(UPDATE_FILE) {
            let path = parse_path(path, index + 1)?;
            let header_line = index + 1;
            index += 1;
            let (move_to, chunks, next) = parse_update(&lines, index, &path, header_line)?;
            index = next;
            hunks.push(PatchHunk::Update {
                path,
                move_to,
                chunks,
            });
            continue;
        }
        bail!(
            "invalid patch hunk on line {}: expected Add File, Delete File, or Update File",
            index + 1
        );
    }

    if !ended || lines[index..].iter().any(|line| !line.trim().is_empty()) {
        bail!("the last line of the patch must be '{END_PATCH}'");
    }
    if hunks.is_empty() {
        bail!("no files were modified");
    }
    Ok(hunks)
}

fn parse_update(
    lines: &[&str],
    mut index: usize,
    path: &Path,
    header_line: usize,
) -> Result<(Option<PathBuf>, Vec<UpdateChunk>, usize)> {
    let mut move_to = None;
    let mut chunks = Vec::<UpdateChunk>::new();
    while index < lines.len() {
        let line = lines[index];
        let update_line = line.trim_end();
        if is_operation_or_end(update_line) {
            break;
        }
        if chunks.is_empty()
            && move_to.is_none()
            && let Some(destination) = update_line.strip_prefix(MOVE_TO)
        {
            move_to = Some(parse_path(destination, index + 1)?);
            index += 1;
            continue;
        }
        if chunks.last().is_some_and(|chunk| chunk.end_of_file) {
            if update_line.is_empty() {
                index += 1;
                continue;
            }
            if update_line != "@@" && !update_line.starts_with("@@ ") {
                bail!(
                    "invalid patch hunk on line {}: expected an @@ context marker after {END_OF_FILE}",
                    index + 1
                );
            }
        }
        if update_line == "@@" || update_line.starts_with("@@ ") {
            if !chunks.is_empty() {
                ensure_last_chunk_has_lines(&chunks, index + 1)?;
            }
            chunks.push(UpdateChunk {
                change_context: update_line.strip_prefix("@@ ").map(str::to_string),
                ..UpdateChunk::default()
            });
            index += 1;
            continue;
        }
        if update_line == END_OF_FILE {
            ensure_last_chunk_has_lines(&chunks, index + 1)?;
            chunks.last_mut().expect("validated chunk").end_of_file = true;
            index += 1;
            continue;
        }

        if chunks.is_empty() {
            chunks.push(UpdateChunk::default());
        }
        let chunk = chunks.last_mut().expect("chunk was inserted");
        if line.is_empty() {
            chunk.push_context("");
        } else if let Some(context) = line.strip_prefix(' ') {
            chunk.push_context(context);
        } else if let Some(added) = line.strip_prefix('+') {
            chunk.new_lines.push(added.to_string());
        } else if let Some(removed) = line.strip_prefix('-') {
            chunk.old_lines.push(removed.to_string());
        } else {
            bail!(
                "invalid patch hunk on line {}: update lines must start with ' ', '+', or '-'",
                index + 1
            );
        }
        index += 1;
    }

    if chunks.is_empty() {
        bail!(
            "invalid patch hunk on line {header_line}: Update File for {} is empty",
            path.display()
        );
    }
    ensure_last_chunk_has_lines(&chunks, index + 1)?;
    Ok((move_to, chunks, index))
}

fn ensure_last_chunk_has_lines(chunks: &[UpdateChunk], line: usize) -> Result<()> {
    if chunks.last().is_none_or(UpdateChunk::is_empty) {
        bail!("invalid patch hunk on line {line}: update hunk does not contain any lines");
    }
    Ok(())
}

fn parse_path(path: &str, line: usize) -> Result<PathBuf> {
    if path.is_empty() {
        bail!("invalid patch hunk on line {line}: file path cannot be empty");
    }
    Ok(PathBuf::from(path))
}

fn is_operation_or_end(line: &str) -> bool {
    line == END_PATCH
        || line.starts_with(ADD_FILE)
        || line.starts_with(DELETE_FILE)
        || line.starts_with(UPDATE_FILE)
}

async fn apply_patch(cwd: &Path, patch: &str) -> Result<String> {
    let hunks = parse_patch(patch)?;
    let mut added = Vec::new();
    let mut modified = Vec::new();
    let mut deleted = Vec::new();

    for hunk in hunks {
        match hunk {
            PatchHunk::Add { path, content } => {
                atomic_write(
                    &resolve_path(cwd, path.to_string_lossy().as_ref()),
                    content.as_bytes(),
                )
                .await?;
                added.push(path);
            }
            PatchHunk::Delete { path } => {
                let resolved = resolve_path(cwd, path.to_string_lossy().as_ref());
                let metadata = fs::symlink_metadata(&resolved)
                    .await
                    .with_context(|| format!("failed to delete file {}", resolved.display()))?;
                if metadata.is_dir() {
                    bail!(
                        "failed to delete file {}: path is a directory",
                        resolved.display()
                    );
                }
                fs::remove_file(&resolved)
                    .await
                    .with_context(|| format!("failed to delete file {}", resolved.display()))?;
                deleted.push(path);
            }
            PatchHunk::Update {
                path,
                move_to,
                chunks,
            } => {
                let source = resolve_path(cwd, path.to_string_lossy().as_ref());
                let updated = derive_updated_content(&source, &chunks).await?;
                if let Some(destination) = move_to {
                    let resolved_destination =
                        resolve_path(cwd, destination.to_string_lossy().as_ref());
                    atomic_write(&resolved_destination, updated.as_bytes()).await?;
                    if source != resolved_destination {
                        fs::remove_file(&source).await.with_context(|| {
                            format!("failed to remove original {}", source.display())
                        })?;
                    }
                    modified.push(destination);
                } else {
                    atomic_write(&source, updated.as_bytes()).await?;
                    modified.push(path);
                }
            }
        }
    }

    let mut output = String::from("Success. Updated the following files:\n");
    for path in added {
        output.push_str(&format!("A {}\n", path.display()));
    }
    for path in modified {
        output.push_str(&format!("M {}\n", path.display()));
    }
    for path in deleted {
        output.push_str(&format!("D {}\n", path.display()));
    }
    Ok(output)
}

async fn derive_updated_content(path: &Path, chunks: &[UpdateChunk]) -> Result<String> {
    let original = fs::read_to_string(path)
        .await
        .with_context(|| format!("failed to read file to update {}", path.display()))?;
    let mut lines = original.split('\n').map(str::to_string).collect::<Vec<_>>();
    if lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }

    let mut replacements = Vec::<(usize, usize, Vec<String>)>::new();
    let mut cursor = 0;
    for chunk in chunks {
        if let Some(context) = &chunk.change_context {
            let context = std::slice::from_ref(context);
            let Some(found) = seek_sequence(&lines, context, cursor, false) else {
                bail!(
                    "failed to find context '{}' in {}",
                    context[0],
                    path.display()
                );
            };
            cursor = found + 1;
        }

        if chunk.old_lines.is_empty() {
            replacements.push((lines.len(), 0, chunk.new_lines.clone()));
            continue;
        }

        let mut old_lines = chunk.old_lines.as_slice();
        let mut new_lines = chunk.new_lines.as_slice();
        let mut found = seek_sequence(&lines, old_lines, cursor, chunk.end_of_file);
        if found.is_none() && old_lines.last().is_some_and(String::is_empty) {
            old_lines = &old_lines[..old_lines.len() - 1];
            if new_lines.last().is_some_and(String::is_empty) {
                new_lines = &new_lines[..new_lines.len() - 1];
            }
            found = seek_sequence(&lines, old_lines, cursor, chunk.end_of_file);
        }
        let Some(start) = found else {
            bail!(
                "failed to find expected lines in {}:\n{}",
                path.display(),
                chunk.old_lines.join("\n")
            );
        };
        replacements.push((start, old_lines.len(), new_lines.to_vec()));
        cursor = start + old_lines.len();
    }

    replacements.sort_by_key(|(start, _, _)| *start);
    for (start, old_len, replacement) in replacements.into_iter().rev() {
        lines.splice(start..start + old_len, replacement);
    }
    if !lines.last().is_some_and(String::is_empty) {
        lines.push(String::new());
    }
    Ok(lines.join("\n"))
}

fn seek_sequence(lines: &[String], pattern: &[String], start: usize, eof: bool) -> Option<usize> {
    if pattern.is_empty() {
        return Some(start);
    }
    if pattern.len() > lines.len() {
        return None;
    }
    let last = lines.len() - pattern.len();
    let search_start = if eof { last } else { start };
    if search_start > last {
        return None;
    }

    let candidates = search_start..=last;
    for index in candidates.clone() {
        if lines[index..index + pattern.len()] == *pattern {
            return Some(index);
        }
    }
    for index in candidates.clone() {
        if pattern
            .iter()
            .enumerate()
            .all(|(offset, expected)| lines[index + offset].trim_end() == expected.trim_end())
        {
            return Some(index);
        }
    }
    for index in candidates.clone() {
        if pattern
            .iter()
            .enumerate()
            .all(|(offset, expected)| lines[index + offset].trim() == expected.trim())
        {
            return Some(index);
        }
    }
    candidates.into_iter().find(|&index| {
        pattern.iter().enumerate().all(|(offset, expected)| {
            normalize_for_match(&lines[index + offset]) == normalize_for_match(expected)
        })
    })
}

fn normalize_for_match(value: &str) -> String {
    value
        .trim()
        .chars()
        .map(|character| match character {
            '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}'
            | '\u{2212}' => '-',
            '\u{2018}' | '\u{2019}' | '\u{201a}' | '\u{201b}' => '\'',
            '\u{201c}' | '\u{201d}' | '\u{201e}' | '\u{201f}' => '"',
            '\u{00a0}' | '\u{2002}' | '\u{2003}' | '\u{2004}' | '\u{2005}' | '\u{2006}'
            | '\u{2007}' | '\u{2008}' | '\u{2009}' | '\u{200a}' | '\u{202f}' | '\u{205f}'
            | '\u{3000}' => ' ',
            other => other,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::TaskManager;

    #[tokio::test]
    async fn protocol_exec_returns_the_completed_patch_directly() {
        let directory = tempfile::tempdir().unwrap();
        let protocol = ApplyPatchProtocol::new(directory.path());
        let tasks = TaskManager::new();
        let context = ProtocolContext {
            tasks: tasks.clone(),
        };
        let patch = serde_json::Value::String(
            "*** Begin Patch\n*** Add File: added.txt\n+added\n*** End Patch".to_string(),
        );
        let help = protocol
            .read(
                ProtocolRequest {
                    uri: "apply_patch://help",
                    target: "help",
                    body: None,
                },
                context.clone(),
            )
            .await
            .unwrap();
        let help = String::from_utf8(help).unwrap();
        assert!(help.contains("*** Add File: <path>"));
        assert!(help.contains("returns the final summary"));

        let output = protocol
            .exec(
                ProtocolRequest {
                    uri: "apply_patch://apply",
                    target: "apply",
                    body: Some(&patch),
                },
                context.clone(),
            )
            .await
            .unwrap();

        assert!(String::from_utf8(output).unwrap().contains("A added.txt"));
        assert_eq!(
            fs::read_to_string(directory.path().join("added.txt"))
                .await
                .unwrap(),
            "added\n"
        );
        assert!(tasks.list().await.is_empty());
        assert!(
            protocol
                .read(
                    ProtocolRequest {
                        uri: "apply_patch://tasks",
                        target: "tasks",
                        body: None,
                    },
                    context,
                )
                .await
                .unwrap_err()
                .to_string()
                .contains("apply_patch://help")
        );
    }

    #[tokio::test]
    async fn codex_patch_adds_updates_moves_and_deletes_files() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("update.txt"), "alpha\nbeta\n")
            .await
            .unwrap();
        fs::write(directory.path().join("move.txt"), "move me\n")
            .await
            .unwrap();
        fs::write(directory.path().join("delete.txt"), "delete me\n")
            .await
            .unwrap();
        let patch = r#"*** Begin Patch
*** Add File: nested/add.txt
+added
*** Update File: update.txt
@@
 alpha
-beta
+gamma
*** Update File: move.txt
*** Move to: nested/moved.txt
@@
-move me
+moved
*** Delete File: delete.txt
*** End Patch"#;

        let output = apply_patch(directory.path(), patch).await.unwrap();

        assert_eq!(
            output,
            "Success. Updated the following files:\nA nested/add.txt\nM update.txt\nM nested/moved.txt\nD delete.txt\n"
        );
        assert_eq!(
            fs::read_to_string(directory.path().join("nested/add.txt"))
                .await
                .unwrap(),
            "added\n"
        );
        assert_eq!(
            fs::read_to_string(directory.path().join("update.txt"))
                .await
                .unwrap(),
            "alpha\ngamma\n"
        );
        assert!(!directory.path().join("move.txt").exists());
        assert_eq!(
            fs::read_to_string(directory.path().join("nested/moved.txt"))
                .await
                .unwrap(),
            "moved\n"
        );
        assert!(!directory.path().join("delete.txt").exists());
    }

    #[tokio::test]
    async fn failed_later_hunk_keeps_earlier_changes_but_not_the_failed_update() {
        let directory = tempfile::tempdir().unwrap();
        let existing = directory.path().join("existing.txt");
        fs::write(&existing, "unchanged\n").await.unwrap();
        let patch = r#"*** Begin Patch
*** Add File: created.txt
+created
*** Update File: existing.txt
@@
-missing
+changed
*** End Patch"#;

        let error = apply_patch(directory.path(), patch).await.unwrap_err();

        assert!(error.to_string().contains("failed to find expected lines"));
        assert_eq!(
            fs::read_to_string(directory.path().join("created.txt"))
                .await
                .unwrap(),
            "created\n"
        );
        assert_eq!(fs::read_to_string(existing).await.unwrap(), "unchanged\n");
    }

    #[test]
    fn malformed_patch_is_rejected_before_application() {
        let error =
            parse_patch("*** Begin Patch\n*** Add File: file.txt\nmissing-prefix\n*** End Patch")
                .unwrap_err();
        assert!(error.to_string().contains("must start with '+'"));
    }
}
