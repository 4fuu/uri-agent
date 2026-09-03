use super::{CorpusSnapshot, Fragment, IndexSpec};
use crate::config::display_path;
use anyhow::{Context, Result, bail};
use ignore::WalkBuilder;
use ignore::overrides::OverrideBuilder;
use std::fs;
use std::path::{Path, PathBuf};

const EXTRACTOR_VERSION: &str = "text-lines-v1";
const MAX_FILE_BYTES: u64 = 1024 * 1024;
const LINES_PER_GROUP: usize = 60;
const FRAGMENT_CHARS: usize = 1_024;
const FRAGMENT_OVERLAP_CHARS: usize = 154;

#[derive(Debug)]
pub(crate) struct CodeCorpus {
    pub(crate) spec: IndexSpec,
    pub(crate) snapshot: CorpusSnapshot,
}

pub(crate) async fn code_corpus(cwd: &Path, root: &Path, glob: Option<&str>) -> Result<CodeCorpus> {
    let cwd = cwd.to_path_buf();
    let root = root.to_path_buf();
    let glob = glob.map(str::to_string);
    tokio::task::spawn_blocking(move || build_code_corpus(&cwd, &root, glob.as_deref()))
        .await
        .context("code corpus scanner failed")?
}

fn build_code_corpus(cwd: &Path, root: &Path, glob: Option<&str>) -> Result<CodeCorpus> {
    let canonical = root
        .canonicalize()
        .with_context(|| format!("cannot resolve code search root: {}", display_path(root)))?;
    let identity = format!(
        "{}\nglob={}",
        canonical.to_string_lossy(),
        glob.unwrap_or("")
    );
    let source = format!(
        "{}{}",
        display_path(&canonical),
        glob.map_or_else(String::new, |glob| format!(" · glob={glob}"))
    );
    let spec = IndexSpec::new("code", &identity, "Code", source, EXTRACTOR_VERSION)?;
    let mut files = matching_files(&canonical, glob)?;
    files.sort();
    let mut fragments = Vec::new();
    for path in files {
        let metadata = fs::metadata(&path)?;
        if metadata.len() > MAX_FILE_BYTES {
            continue;
        }
        let bytes = fs::read(&path)
            .with_context(|| format!("cannot read code index input: {}", display_path(&path)))?;
        if likely_binary(&bytes) {
            continue;
        }
        let Ok(text) = String::from_utf8(bytes) else {
            continue;
        };
        let source = path
            .strip_prefix(cwd)
            .ok()
            .filter(|relative| !relative.as_os_str().is_empty())
            .map_or_else(|| display_path(&path), display_path);
        fragments.extend(file_fragments(&source, &text));
    }
    Ok(CodeCorpus {
        spec,
        snapshot: CorpusSnapshot::new(fragments),
    })
}

fn matching_files(root: &Path, glob: Option<&str>) -> Result<Vec<PathBuf>> {
    if root.is_file() {
        if glob.is_some() {
            bail!("grep glob filtering requires a directory root for semantic indexing");
        }
        return Ok(vec![root.to_path_buf()]);
    }
    let mut builder = WalkBuilder::new(root);
    builder.standard_filters(true).follow_links(false);
    if let Some(glob) = glob {
        let mut overrides = OverrideBuilder::new(root);
        overrides
            .add(glob)
            .with_context(|| format!("invalid grep glob: {glob}"))?;
        builder.overrides(overrides.build()?);
    }
    let mut files = Vec::new();
    for entry in builder.build() {
        let entry = entry.context("cannot walk code search root")?;
        if entry
            .file_type()
            .is_some_and(|file_type| file_type.is_file())
        {
            files.push(entry.into_path());
        }
    }
    Ok(files)
}

fn likely_binary(bytes: &[u8]) -> bool {
    let sample = &bytes[..bytes.len().min(8 * 1024)];
    if sample.contains(&0) {
        return true;
    }
    let suspicious = sample
        .iter()
        .filter(|byte| matches!(byte, 0..=8 | 11..=12 | 14..=31))
        .count();
    !sample.is_empty() && suspicious.saturating_mul(100) > sample.len().saturating_mul(30)
}

fn file_fragments(source: &str, text: &str) -> Vec<Fragment> {
    let lines = text.split_inclusive('\n').collect::<Vec<_>>();
    let mut fragments = Vec::new();
    for (group_index, group) in lines.chunks(LINES_PER_GROUP).enumerate() {
        let start_line = group_index * LINES_PER_GROUP + 1;
        let end_line = start_line + group.len().saturating_sub(1);
        let group_text = group.concat();
        let anchor = if start_line == end_line {
            start_line.to_string()
        } else {
            format!("{start_line}-{end_line}")
        };
        let group_id = Fragment::stable_id(&[source, &anchor]);
        for (fragment_index, content) in
            overlapping_char_chunks(&group_text).into_iter().enumerate()
        {
            let id = Fragment::stable_id(&[&group_id, &fragment_index.to_string(), &content]);
            fragments.push(Fragment {
                id,
                group_id: group_id.clone(),
                source: source.to_string(),
                anchor: anchor.clone(),
                label: format!("{source}:{anchor}"),
                text: content.clone(),
                embedding_text: format!("path: {source}\nlines: {anchor}\n{content}"),
            });
        }
    }
    fragments
}

fn overlapping_char_chunks(text: &str) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    let boundaries = text
        .char_indices()
        .map(|(index, _)| index)
        .chain(std::iter::once(text.len()))
        .collect::<Vec<_>>();
    let character_count = boundaries.len() - 1;
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < character_count {
        let end = (start + FRAGMENT_CHARS).min(character_count);
        chunks.push(text[boundaries[start]..boundaries[end]].to_string());
        if end == character_count {
            break;
        }
        start = end.saturating_sub(FRAGMENT_OVERLAP_CHARS);
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_code_with_stable_line_anchors_and_character_overlap() {
        let text = (1..=61)
            .map(|line| format!("line {line}\n"))
            .collect::<String>();
        let fragments = file_fragments("src/lib.rs", &text);
        assert_eq!(fragments.len(), 2);
        assert_eq!(fragments[0].anchor, "1-60");
        assert_eq!(fragments[1].anchor, "61");
        assert!(fragments[0].embedding_text.starts_with("path: src/lib.rs"));

        let long = "界".repeat(FRAGMENT_CHARS + 20);
        let chunks = overlapping_char_chunks(&long);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].chars().count(), FRAGMENT_CHARS);
        assert_eq!(
            chunks[0]
                .chars()
                .skip(FRAGMENT_CHARS - FRAGMENT_OVERLAP_CHARS)
                .collect::<String>(),
            chunks[1]
                .chars()
                .take(FRAGMENT_OVERLAP_CHARS)
                .collect::<String>()
        );
    }

    #[tokio::test]
    async fn scanner_honors_ignore_files_globs_and_binary_limits() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir_all(directory.path().join("ignored")).unwrap();
        fs::write(
            directory.path().join("main.rs"),
            "fn semantic_search() {}\n",
        )
        .unwrap();
        fs::write(directory.path().join("notes.txt"), "semantic notes\n").unwrap();
        fs::write(directory.path().join("ignored/other.rs"), "ignored\n").unwrap();
        fs::write(directory.path().join(".ignore"), "ignored/\n").unwrap();
        fs::write(directory.path().join("binary.rs"), b"code\0binary").unwrap();
        fs::write(directory.path().join("controls.rs"), vec![1_u8; 100]).unwrap();

        let corpus = code_corpus(directory.path(), directory.path(), Some("**/*.rs"))
            .await
            .unwrap();
        assert_eq!(corpus.snapshot.len(), 1);
        assert_eq!(corpus.snapshot.fragments[0].source, "main.rs");
    }

    #[test]
    fn file_globs_are_rejected_instead_of_silently_ignored() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("main.rs");
        fs::write(&file, "fn main() {}\n").unwrap();
        let error = build_code_corpus(directory.path(), &file, Some("*.rs")).unwrap_err();
        assert!(error.to_string().contains("directory root"));
    }
}
