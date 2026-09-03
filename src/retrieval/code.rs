use super::{CorpusCatalog, CorpusSnapshot, Fragment, IndexSpec};
use crate::config::display_path;
use anyhow::{Context, Result, bail};
use ignore::WalkBuilder;
use ignore::overrides::OverrideBuilder;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;
use tokio_util::sync::CancellationToken;

const EXTRACTOR_VERSION: &str = "text-lines-v2";
const MAX_FILE_BYTES: u64 = 1024 * 1024;
const LINES_PER_GROUP: usize = 60;
const FRAGMENT_CHARS: usize = 1_024;
const FRAGMENT_OVERLAP_CHARS: usize = 154;

#[derive(Clone, Debug)]
pub(crate) struct CodeCorpus {
    pub(crate) spec: IndexSpec,
    pub(crate) catalog: CorpusCatalog,
    files: BTreeMap<String, PathBuf>,
}

impl CodeCorpus {
    pub(crate) async fn load_all(&self, cancellation: CancellationToken) -> Result<CorpusSnapshot> {
        let sources = self.catalog.sources.keys().cloned().collect();
        self.load_sources(sources, cancellation).await
    }

    pub(crate) async fn load_sources(
        &self,
        sources: BTreeSet<String>,
        cancellation: CancellationToken,
    ) -> Result<CorpusSnapshot> {
        let corpus = self.clone();
        tokio::task::spawn_blocking(move || corpus.load_sources_blocking(&sources, &cancellation))
            .await
            .context("code corpus reader failed")?
    }

    fn load_sources_blocking(
        &self,
        sources: &BTreeSet<String>,
        cancellation: &CancellationToken,
    ) -> Result<CorpusSnapshot> {
        let mut fragments = BTreeMap::new();
        for source in sources {
            if cancellation.is_cancelled() {
                bail!("code indexing was cancelled while reading source files");
            }
            let path = self
                .files
                .get(source)
                .with_context(|| format!("code source changed while loading: {source}"))?;
            let bytes = fs::read(path)
                .with_context(|| format!("cannot read code index input: {}", display_path(path)))?;
            let source_fragments = if likely_binary(&bytes) {
                Vec::new()
            } else if let Ok(text) = String::from_utf8(bytes) {
                file_fragments(source, &text)
            } else {
                Vec::new()
            };
            fragments.insert(source.clone(), source_fragments);
        }
        CorpusSnapshot::new(self.catalog.clone(), fragments)
    }
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
    let mut sources = BTreeMap::new();
    let mut indexed_files = BTreeMap::new();
    for path in files {
        let metadata = fs::metadata(&path)?;
        if metadata.len() > MAX_FILE_BYTES {
            continue;
        }
        let source = path
            .strip_prefix(cwd)
            .ok()
            .filter(|relative| !relative.as_os_str().is_empty())
            .map_or_else(|| display_path(&path), display_path);
        sources.insert(source.clone(), metadata_revision(&metadata)?);
        indexed_files.insert(source, path);
    }
    Ok(CodeCorpus {
        spec,
        catalog: CorpusCatalog::new(sources),
        files: indexed_files,
    })
}

fn metadata_revision(metadata: &fs::Metadata) -> Result<String> {
    let modified = metadata.modified()?.duration_since(UNIX_EPOCH)?;
    Ok(format!(
        "{}:{}:{}",
        metadata.len(),
        modified.as_secs(),
        modified.subsec_nanos()
    ))
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
        let group_id = Fragment::stable_id(&[source, &format!("{start_line}-{end_line}")]);
        for (fragment_index, chunk) in overlapping_char_chunks(&group_text).into_iter().enumerate()
        {
            let fragment_start = line_for_offset(&group_text, chunk.start, start_line, false);
            let fragment_end = line_for_offset(&group_text, chunk.end, start_line, true);
            let anchor = if fragment_start == fragment_end {
                fragment_start.to_string()
            } else {
                format!("{fragment_start}-{fragment_end}")
            };
            let content = chunk.text;
            let id = Fragment::stable_id(&[&group_id, &fragment_index.to_string(), &content]);
            fragments.push(Fragment {
                id,
                group_id: group_id.clone(),
                catalog_source: source.to_string(),
                source: source.to_string(),
                anchor: anchor.clone(),
                label: format!("{source}:{anchor}"),
                text: content.clone(),
                embedding_text: format!("path: {source}\nlines: {anchor}\n{content}"),
                record_type: String::new(),
                window_id: 0,
            });
        }
    }
    fragments
}

#[derive(Debug)]
struct CharChunk {
    text: String,
    start: usize,
    end: usize,
}

fn overlapping_char_chunks(text: &str) -> Vec<CharChunk> {
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
        let start_byte = boundaries[start];
        let end_byte = boundaries[end];
        chunks.push(CharChunk {
            text: text[start_byte..end_byte].to_string(),
            start: start_byte,
            end: end_byte,
        });
        if end == character_count {
            break;
        }
        start = end.saturating_sub(FRAGMENT_OVERLAP_CHARS);
    }
    chunks
}

fn line_for_offset(text: &str, offset: usize, first_line: usize, end: bool) -> usize {
    let newlines = text.as_bytes()[..offset]
        .iter()
        .filter(|byte| **byte == b'\n')
        .count();
    if end && offset > 0 && text.as_bytes()[offset - 1] == b'\n' {
        first_line + newlines.saturating_sub(1)
    } else {
        first_line + newlines
    }
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
        assert_eq!(chunks[0].text.chars().count(), FRAGMENT_CHARS);
        assert_eq!(
            chunks[0]
                .text
                .chars()
                .skip(FRAGMENT_CHARS - FRAGMENT_OVERLAP_CHARS)
                .collect::<String>(),
            chunks[1]
                .text
                .chars()
                .take(FRAGMENT_OVERLAP_CHARS)
                .collect::<String>()
        );

        let long_lines = (1..=60)
            .map(|line| format!("line {line:02} {}\n", "x".repeat(90)))
            .collect::<String>();
        let fragments = file_fragments("src/large.rs", &long_lines);
        assert_eq!(fragments[0].anchor, "1-11");
        assert_eq!(fragments[0].label, "src/large.rs:1-11");
        assert_eq!(fragments[1].anchor, "9-20");
        assert!(fragments.iter().all(|fragment| fragment.anchor != "1-60"));
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
        let snapshot = corpus.load_all(CancellationToken::new()).await.unwrap();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot.fragments["main.rs"][0].source, "main.rs");
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
