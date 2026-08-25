use super::file::resolve_path;
use super::{EditableText, atomic_write};
use crate::plugin::{ModelTool, ModelToolDescriptor, Plugin, PluginHost};
use crate::protocol::ProtocolRegistry;
use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use tokio::fs;

const BEGIN_PATCH: &str = "*** Begin Patch";
const END_PATCH: &str = "*** End Patch";
const ADD_FILE: &str = "*** Add File: ";
const DELETE_FILE: &str = "*** Delete File: ";
const UPDATE_FILE: &str = "*** Update File: ";
const MOVE_TO: &str = "*** Move to: ";
const END_OF_FILE: &str = "*** End of File";
const PATCH_ARGUMENT_DESCRIPTION: &str = concat!(
    "Complete Codex-style patch in this format:\n",
    "*** Begin Patch\n",
    "*** Add File: <path>\n",
    "+<new content>\n",
    "*** Update File: <path>\n",
    "@@ <optional landmark>\n",
    "-<old line>\n",
    "+<new line>\n",
    "*** Delete File: <path>\n",
    "*** End Patch\n\n",
    "Replace placeholders with actual values. Use @@ without a landmark when none is needed. ",
    "An Update File may put *** Move to: <new path> immediately after its header. ",
    "Update lines begin with a space for context, - for removal, or + for addition. ",
    "*** End of File anchors the preceding chunk at EOF. Every Add File content line ",
    "begins with +. Relative paths resolve from the startup working directory; absolute ",
    "paths are accepted."
);

#[derive(Clone)]
pub(super) struct ApplyPatchTool {
    cwd: PathBuf,
}

impl ApplyPatchTool {
    pub(super) fn new(cwd: &Path) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
        }
    }
}

impl Plugin for ApplyPatchTool {
    fn model_tool_descriptors(&self) -> Vec<ModelToolDescriptor> {
        vec![<Self as ModelTool>::descriptor(self)]
    }

    fn register(&self, host: &mut PluginHost<'_>) -> Result<()> {
        host.model_tools.register(self.clone())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ApplyPatchArguments {
    patch: String,
}

#[async_trait]
impl ModelTool for ApplyPatchTool {
    fn descriptor(&self) -> ModelToolDescriptor {
        ModelToolDescriptor {
            name: "apply_patch".to_string(),
            description: "Apply a transactional Codex-style multi-file patch. The complete patch is parsed and applied to an in-memory plan before files change; commit failures roll back every affected file.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "patch": {
                        "type": "string",
                        "description": PATCH_ARGUMENT_DESCRIPTION
                    }
                },
                "required": ["patch"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, arguments: &Value, _protocols: &ProtocolRegistry) -> Result<String> {
        let arguments: ApplyPatchArguments = serde_json::from_value(arguments.clone())
            .context("invalid apply_patch tool arguments")?;
        if arguments.patch.is_empty() {
            bail!("apply_patch patch must be nonempty");
        }
        apply_patch(&self.cwd, &arguments.patch).await
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
    let patch = EditableText::new(patch);
    let lines = patch.normalized().trim().split('\n').collect::<Vec<_>>();
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
    let mut files = BTreeMap::<PathBuf, PlannedFile>::new();
    let mut added = Vec::new();
    let mut modified = Vec::new();
    let mut deleted = Vec::new();

    for hunk in hunks {
        match hunk {
            PatchHunk::Add { path, content } => {
                let resolved = resolve_path(cwd, path.to_string_lossy().as_ref());
                load_planned_file(&mut files, &resolved)
                    .await?
                    .final_content = Some(content.into_bytes());
                added.push(path);
            }
            PatchHunk::Delete { path } => {
                let resolved = resolve_path(cwd, path.to_string_lossy().as_ref());
                let file = load_planned_file(&mut files, &resolved).await?;
                if file.final_content.is_none() {
                    bail!(
                        "failed to delete file {}: file not found",
                        resolved.display()
                    );
                }
                file.final_content = None;
                deleted.push(path);
            }
            PatchHunk::Update {
                path,
                move_to,
                chunks,
            } => {
                let source = resolve_path(cwd, path.to_string_lossy().as_ref());
                let source_content = load_planned_file(&mut files, &source)
                    .await?
                    .final_content
                    .clone()
                    .ok_or_else(|| anyhow!("failed to read file to update {}", source.display()))?;
                let source_text = String::from_utf8(source_content).with_context(|| {
                    format!("file to update is not UTF-8: {}", source.display())
                })?;
                let updated = derive_updated_content(&source, &source_text, &chunks)?;
                if let Some(destination) = move_to {
                    let resolved_destination =
                        resolve_path(cwd, destination.to_string_lossy().as_ref());
                    if source != resolved_destination {
                        load_planned_file(&mut files, &resolved_destination)
                            .await?
                            .final_content = Some(updated.into_bytes());
                        files
                            .get_mut(&source)
                            .expect("source was loaded")
                            .final_content = None;
                    } else {
                        files
                            .get_mut(&source)
                            .expect("source was loaded")
                            .final_content = Some(updated.into_bytes());
                    }
                    modified.push(destination);
                } else {
                    files
                        .get_mut(&source)
                        .expect("source was loaded")
                        .final_content = Some(updated.into_bytes());
                    modified.push(path);
                }
            }
        }
    }

    commit_plan(&files).await?;

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

#[derive(Clone)]
struct OriginalFile {
    content: Vec<u8>,
    permissions: std::fs::Permissions,
}

struct PlannedFile {
    original: Option<OriginalFile>,
    final_content: Option<Vec<u8>>,
}

async fn load_planned_file<'a>(
    files: &'a mut BTreeMap<PathBuf, PlannedFile>,
    path: &Path,
) -> Result<&'a mut PlannedFile> {
    if !files.contains_key(path) {
        let original = match fs::symlink_metadata(path).await {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    bail!("patch paths cannot be symbolic links: {}", path.display());
                }
                if !metadata.is_file() {
                    bail!("patch path is not a regular file: {}", path.display());
                }
                Some(OriginalFile {
                    content: fs::read(path)
                        .await
                        .with_context(|| format!("failed to read {}", path.display()))?,
                    permissions: metadata.permissions(),
                })
            }
            Err(error) if error.kind() == ErrorKind::NotFound => None,
            Err(error) => {
                return Err(error).with_context(|| format!("failed to inspect {}", path.display()));
            }
        };
        files.insert(
            path.to_path_buf(),
            PlannedFile {
                final_content: original.as_ref().map(|file| file.content.clone()),
                original,
            },
        );
    }
    Ok(files.get_mut(path).expect("planned file was inserted"))
}

async fn commit_plan(files: &BTreeMap<PathBuf, PlannedFile>) -> Result<()> {
    let changed = files
        .iter()
        .filter(|(_, file)| {
            file.original.as_ref().map(|file| &file.content) != file.final_content.as_ref()
        })
        .map(|(path, _)| path.clone())
        .collect::<Vec<_>>();
    let mut created_directories = BTreeSet::new();
    for path in &changed {
        if files[path].final_content.is_some() {
            collect_missing_parent_directories(path, &mut created_directories).await?;
        }
    }

    let mut applied = Vec::new();
    let result = async {
        for path in changed
            .iter()
            .filter(|path| files[*path].final_content.is_some())
        {
            atomic_write(
                path,
                files[path]
                    .final_content
                    .as_deref()
                    .expect("filtered planned write"),
            )
            .await?;
            applied.push(path.clone());
        }
        for path in changed
            .iter()
            .filter(|path| files[*path].final_content.is_none())
        {
            fs::remove_file(path)
                .await
                .with_context(|| format!("failed to delete file {}", path.display()))?;
            applied.push(path.clone());
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let Err(error) = result else {
        return Ok(());
    };

    let rollback_errors = rollback_plan(files, &applied, &created_directories).await;
    if rollback_errors.is_empty() {
        return Err(error).context("patch commit failed; all file changes were rolled back");
    }
    bail!(
        "patch commit failed: {error:#}; rollback also failed: {}",
        rollback_errors.join("; ")
    )
}

async fn collect_missing_parent_directories(
    path: &Path,
    directories: &mut BTreeSet<PathBuf>,
) -> Result<()> {
    let mut parent = path.parent();
    while let Some(directory) = parent {
        match fs::metadata(directory).await {
            Ok(metadata) if metadata.is_dir() => break,
            Ok(_) => bail!(
                "cannot create patch file because parent is not a directory: {}",
                directory.display()
            ),
            Err(error) if error.kind() == ErrorKind::NotFound => {
                directories.insert(directory.to_path_buf());
                parent = directory.parent();
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to inspect {}", directory.display()));
            }
        }
    }
    Ok(())
}

async fn rollback_plan(
    files: &BTreeMap<PathBuf, PlannedFile>,
    applied: &[PathBuf],
    created_directories: &BTreeSet<PathBuf>,
) -> Vec<String> {
    let mut errors = Vec::new();
    for path in applied.iter().rev() {
        let result = match &files[path].original {
            Some(original) => {
                async {
                    atomic_write(path, &original.content).await?;
                    fs::set_permissions(path, original.permissions.clone()).await?;
                    Ok::<_, anyhow::Error>(())
                }
                .await
            }
            None => match fs::remove_file(path).await {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error.into()),
            },
        };
        if let Err(error) = result {
            errors.push(format!("{}: {error:#}", path.display()));
        }
    }
    let mut directories = created_directories.iter().collect::<Vec<_>>();
    directories.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    for directory in directories {
        match fs::remove_dir(directory).await {
            Ok(()) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::NotFound | ErrorKind::DirectoryNotEmpty
                ) => {}
            Err(error) => errors.push(format!("{}: {error}", directory.display())),
        }
    }
    errors
}

fn derive_updated_content(path: &Path, original: &str, chunks: &[UpdateChunk]) -> Result<String> {
    let original = EditableText::new(original);
    let had_final_newline = original.normalized().ends_with('\n');
    let mut lines = if original.normalized().is_empty() {
        Vec::new()
    } else {
        let mut lines = original
            .normalized()
            .split('\n')
            .map(str::to_string)
            .collect::<Vec<_>>();
        if had_final_newline {
            lines.pop();
        }
        lines
    };

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
    if had_final_newline {
        lines.push(String::new());
    }
    let updated = lines.join("\n");
    Ok(original.restore(&updated))
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
    use crate::output::OutputStore;
    use crate::task::TaskManager;
    use std::sync::Arc;

    #[tokio::test]
    async fn direct_tool_returns_the_completed_patch() {
        let directory = tempfile::tempdir().unwrap();
        let output_store = Arc::new(
            OutputStore::new(
                &format!("apply-patch-{}", uuid::Uuid::now_v7().simple()),
                1024,
            )
            .await
            .unwrap(),
        );
        let protocols = ProtocolRegistry::new(output_store.clone(), TaskManager::new());
        let tool = ApplyPatchTool::new(directory.path());
        let patch = "*** Begin Patch\n*** Add File: added.txt\n+added\n*** End Patch";

        let output = tool
            .execute(
                &json!({
                    "patch": patch
                }),
                &protocols,
            )
            .await
            .unwrap();

        assert!(output.contains("A added.txt"));
        assert_eq!(
            fs::read_to_string(directory.path().join("added.txt"))
                .await
                .unwrap(),
            "added\n"
        );
        let descriptor = tool.descriptor();
        assert!(
            descriptor.parameters["properties"]["patch"]["description"]
                .as_str()
                .unwrap()
                .contains("*** Add File: <path>")
        );
        let _ = fs::remove_dir_all(output_store.directory()).await;
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
    async fn codex_patch_matches_lf_and_preserves_crlf_bom_and_blank_lines() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("update.txt");
        fs::write(&path, "\u{feff}alpha\r\nbeta\r\n\r\n")
            .await
            .unwrap();
        let patch = r#"*** Begin Patch
*** Update File: update.txt
@@
 alpha
-beta
+gamma

*** End Patch"#;

        apply_patch(directory.path(), patch).await.unwrap();

        assert_eq!(
            fs::read_to_string(path).await.unwrap(),
            "\u{feff}alpha\r\ngamma\r\n\r\n"
        );
    }

    #[tokio::test]
    async fn codex_patch_preserves_missing_final_newline() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("update.txt");
        fs::write(&path, "\u{feff}alpha\r\nbeta").await.unwrap();
        let patch = r#"*** Begin Patch
*** Update File: update.txt
@@
 alpha
-beta
+gamma
+
*** End Patch"#;

        apply_patch(directory.path(), patch).await.unwrap();

        assert_eq!(
            fs::read_to_string(path).await.unwrap(),
            "\u{feff}alpha\r\ngamma"
        );
    }

    #[tokio::test]
    async fn codex_patch_normalizes_bom_and_crlf_in_patch_input_and_updates_empty_files() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("empty.txt");
        fs::write(&path, "").await.unwrap();
        let patch = "\u{feff}*** Begin Patch\r\n*** Update File: empty.txt\r\n@@\r\n+alpha\r\n+beta\r\n*** End Patch";

        apply_patch(directory.path(), patch).await.unwrap();

        assert_eq!(fs::read_to_string(path).await.unwrap(), "alpha\nbeta");
    }

    #[tokio::test]
    async fn failed_later_hunk_does_not_write_any_files() {
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
        assert!(!directory.path().join("created.txt").exists());
        assert_eq!(fs::read_to_string(existing).await.unwrap(), "unchanged\n");
    }

    #[tokio::test]
    async fn commit_failure_rolls_back_an_earlier_write() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("a.txt");
        let failing = directory.path().join("z-directory");
        fs::write(&first, "original\n").await.unwrap();
        fs::create_dir(&failing).await.unwrap();
        let metadata = fs::metadata(&first).await.unwrap();
        let mut files = BTreeMap::new();
        files.insert(
            first.clone(),
            PlannedFile {
                original: Some(OriginalFile {
                    content: b"original\n".to_vec(),
                    permissions: metadata.permissions(),
                }),
                final_content: Some(b"changed\n".to_vec()),
            },
        );
        files.insert(
            failing.clone(),
            PlannedFile {
                original: None,
                final_content: Some(b"cannot replace a directory\n".to_vec()),
            },
        );

        let error = commit_plan(&files).await.unwrap_err();

        assert!(
            error
                .to_string()
                .contains("all file changes were rolled back")
        );
        assert_eq!(fs::read_to_string(first).await.unwrap(), "original\n");
        assert!(failing.is_dir());
    }

    #[test]
    fn malformed_patch_is_rejected_before_application() {
        let error =
            parse_patch("*** Begin Patch\n*** Add File: file.txt\nmissing-prefix\n*** End Patch")
                .unwrap_err();
        assert!(error.to_string().contains("must start with '+'"));
    }
}
