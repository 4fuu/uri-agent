mod code;
mod embedding;

pub(crate) use code::code_corpus;
pub(crate) use embedding::{EMBEDDING_DIMENSION, MODEL_ID, MODEL_REVISION};

use anyhow::{Context, Result, anyhow, bail, ensure};
use fs2::FileExt as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use zvec_rust::{
    Collection, CollectionSchema, DataType, Doc, FieldSchema, Fts, IndexParams, MetricType,
    SearchQuery,
};

const SCHEMA_VERSION: u32 = 2;
const ZVEC_VERSION: &str = "0.7.0";
const MODEL_SHA256: &str = "75cf7a6c2171b230ad19b1e7d8e0b1aee86da5a02af8e7cacedd9921d227623c";
const TOKENIZER_SHA256: &str = "107bbdcbad4bff1d299b7a4c3a2fb17c52890688b7dd0e4c9deab79d3c4f3d45";
const COLLECTION_DIRECTORY: &str = "collection";
const MANIFEST_FILE: &str = "manifest.json";
const INITIAL_RECALL: usize = 200;
const MAX_RECALL: usize = 2_000;
const RRF_K: f32 = 60.0;
const WRITE_BATCH: usize = 256;
const CONVERSATION_EXTRACTOR_VERSION: &str = "conversation-records-v1";
const CONVERSATION_FRAGMENT_CHARS: usize = 1_024;
const CONVERSATION_FRAGMENT_OVERLAP: usize = 154;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SearchMode {
    Semantic,
    Hybrid,
}

impl SearchMode {
    pub(crate) fn parse(value: &str, protocol: &str) -> Result<Self> {
        match value {
            "semantic" => Ok(Self::Semantic),
            "hybrid" => Ok(Self::Hybrid),
            _ => bail!("{protocol} mode must be exact, semantic, or hybrid"),
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Semantic => "semantic",
            Self::Hybrid => "hybrid",
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Fragment {
    pub(crate) id: String,
    pub(crate) group_id: String,
    pub(crate) catalog_source: String,
    pub(crate) source: String,
    pub(crate) anchor: String,
    pub(crate) label: String,
    pub(crate) text: String,
    pub(crate) embedding_text: String,
    pub(crate) record_type: String,
    pub(crate) window_id: u64,
}

impl Fragment {
    pub(crate) fn stable_id(parts: &[&str]) -> String {
        let mut digest = Sha256::new();
        for part in parts {
            digest.update(part.len().to_le_bytes());
            digest.update(part.as_bytes());
        }
        format!("f{:x}", digest.finalize())
    }

    fn sanitize(mut self) -> Self {
        self.group_id = self.group_id.replace('\0', " ");
        self.catalog_source = self.catalog_source.replace('\0', " ");
        self.source = self.source.replace('\0', " ");
        self.anchor = self.anchor.replace('\0', " ");
        self.label = self.label.replace('\0', " ");
        self.text = self.text.replace('\0', " ");
        self.embedding_text = self.embedding_text.replace('\0', " ");
        self.record_type = self.record_type.replace('\0', " ");
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CorpusCatalog {
    sources: BTreeMap<String, String>,
    digest: String,
}

impl CorpusCatalog {
    pub(crate) fn new(sources: impl IntoIterator<Item = (String, String)>) -> Self {
        let sources = sources.into_iter().collect::<BTreeMap<_, _>>();
        Self {
            digest: source_digest(&sources),
            sources,
        }
    }

    pub(crate) fn changed_sources(&self, checkpoint: &IndexCheckpoint) -> BTreeSet<String> {
        if !checkpoint.compatible {
            return self.sources.keys().cloned().collect();
        }
        self.sources
            .iter()
            .filter(|(source, revision)| checkpoint.sources.get(*source) != Some(*revision))
            .map(|(source, _)| source.clone())
            .collect()
    }

    pub(crate) fn all_sources(&self) -> BTreeSet<String> {
        self.sources.keys().cloned().collect()
    }
}

#[derive(Clone, Debug)]
pub(crate) struct CorpusSnapshot {
    catalog: CorpusCatalog,
    fragments: BTreeMap<String, Vec<Fragment>>,
}

impl CorpusSnapshot {
    pub(crate) fn new(
        catalog: CorpusCatalog,
        fragments: BTreeMap<String, Vec<Fragment>>,
    ) -> Result<Self> {
        ensure!(
            fragments
                .keys()
                .all(|source| catalog.sources.contains_key(source)),
            "semantic corpus snapshot contains a source outside its catalog"
        );
        ensure!(
            fragments.iter().all(|(source, fragments)| fragments
                .iter()
                .all(|fragment| fragment.catalog_source == *source)),
            "semantic corpus fragment does not match its catalog source"
        );
        Ok(Self { catalog, fragments })
    }

    fn flattened(self) -> Vec<Fragment> {
        self.fragments.into_values().flatten().collect()
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.fragments.values().map(Vec::len).sum()
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ConversationDocument {
    pub(crate) session_id: String,
    pub(crate) cwd: String,
    pub(crate) anchor: String,
    pub(crate) header: String,
    pub(crate) text: String,
    pub(crate) record_type: String,
    pub(crate) window_id: u64,
}

pub(crate) fn conversation_source_key(session_id: &str, anchor: &str) -> String {
    format!("{session_id}/{anchor}")
}

pub(crate) fn conversation_catalog(documents: &[ConversationDocument]) -> CorpusCatalog {
    CorpusCatalog::new(documents.iter().map(|document| {
        let key = conversation_source_key(&document.session_id, &document.anchor);
        let revision = Fragment::stable_id(&[
            &document.cwd,
            &document.header,
            &document.text,
            &document.record_type,
            &document.window_id.to_string(),
        ]);
        (key, revision)
    }))
}

pub(crate) fn conversation_snapshot(
    catalog: CorpusCatalog,
    sources: BTreeSet<String>,
    documents: Vec<ConversationDocument>,
) -> Result<CorpusSnapshot> {
    let mut fragments = BTreeMap::<String, Vec<Fragment>>::new();
    for document in documents {
        let catalog_source = conversation_source_key(&document.session_id, &document.anchor);
        ensure!(
            sources.contains(&catalog_source),
            "conversation snapshot contains an unrequested source: {catalog_source}"
        );
        let group_id = Fragment::stable_id(&[&document.session_id, &document.anchor]);
        for (index, text) in conversation_chunks(&document.text).into_iter().enumerate() {
            fragments
                .entry(catalog_source.clone())
                .or_default()
                .push(Fragment {
                    id: Fragment::stable_id(&[&group_id, &index.to_string(), &text]),
                    group_id: group_id.clone(),
                    catalog_source: catalog_source.clone(),
                    source: document.session_id.clone(),
                    anchor: document.anchor.clone(),
                    label: document.header.clone(),
                    embedding_text: format!(
                        "session: {}\nworking directory: {}\n{}\n{}",
                        document.session_id, document.cwd, document.header, text
                    ),
                    text,
                    record_type: document.record_type.clone(),
                    window_id: document.window_id,
                });
        }
        fragments.entry(catalog_source).or_default();
    }
    for source in sources {
        fragments.entry(source).or_default();
    }
    CorpusSnapshot::new(catalog, fragments)
}

pub(crate) fn conversation_spec(
    namespace: &str,
    identity: &str,
    corpus: &str,
    source: String,
) -> Result<IndexSpec> {
    IndexSpec::new(
        namespace,
        identity,
        corpus,
        source,
        CONVERSATION_EXTRACTOR_VERSION,
    )
}

fn conversation_chunks(text: &str) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    let boundaries = text
        .char_indices()
        .map(|(index, _)| index)
        .chain(std::iter::once(text.len()))
        .collect::<Vec<_>>();
    let count = boundaries.len() - 1;
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < count {
        let end = (start + CONVERSATION_FRAGMENT_CHARS).min(count);
        chunks.push(text[boundaries[start]..boundaries[end]].to_string());
        if end == count {
            break;
        }
        start = end.saturating_sub(CONVERSATION_FRAGMENT_OVERLAP);
    }
    chunks
}

#[derive(Clone, Debug)]
pub(crate) struct IndexSpec {
    directory: PathBuf,
    corpus: String,
    source: String,
    extractor: String,
}

impl IndexSpec {
    pub(crate) fn new(
        namespace: &str,
        identity: &str,
        corpus: &str,
        source: String,
        extractor: &str,
    ) -> Result<Self> {
        let key = Fragment::stable_id(&[identity]);
        let root = dirs::cache_dir()
            .map(|directory| directory.join("uri-agent"))
            .unwrap_or(std::env::current_dir()?.join(".uri-agent"))
            .join("retrieval")
            .join(format!("v{SCHEMA_VERSION}"))
            .join(namespace);
        Ok(Self {
            directory: root.join(key),
            corpus: corpus.to_string(),
            source,
            extractor: extractor.to_string(),
        })
    }

    #[cfg(test)]
    fn at(directory: PathBuf, corpus: &str, source: &str, extractor: &str) -> Self {
        Self {
            directory,
            corpus: corpus.to_string(),
            source: source.to_string(),
            extractor: extractor.to_string(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum IndexState {
    Missing,
    Stale,
    Current,
}

#[derive(Clone, Debug)]
pub(crate) struct IndexStatus {
    pub(crate) state: IndexState,
    pub(crate) indexed_fragments: usize,
    pub(crate) current_sources: usize,
}

impl IndexStatus {
    pub(crate) fn format(&self, corpus: &str) -> String {
        match self.state {
            IndexState::Missing => format!(
                "{corpus} semantic index is missing · current_sources={}",
                self.current_sources
            ),
            IndexState::Stale => format!(
                "{corpus} semantic index is stale · indexed_fragments={} · current_sources={}",
                self.indexed_fragments, self.current_sources
            ),
            IndexState::Current => format!(
                "{corpus} semantic index is current · fragments={}",
                self.indexed_fragments
            ),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct SearchHit {
    pub(crate) source: String,
    pub(crate) label: String,
    pub(crate) text: String,
    pub(crate) record_type: String,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct SearchFilter {
    record_types: Vec<String>,
    window_id: Option<u64>,
}

impl SearchFilter {
    pub(crate) fn conversation(
        record_types: impl IntoIterator<Item = String>,
        window_id: Option<u64>,
    ) -> Self {
        Self {
            record_types: record_types.into_iter().collect(),
            window_id,
        }
    }

    fn expression(&self) -> Option<String> {
        let mut parts = Vec::new();
        if !self.record_types.is_empty() {
            let values = self
                .record_types
                .iter()
                .map(|value| filter_string(value))
                .collect::<Vec<_>>()
                .join(", ");
            parts.push(format!("record_type IN ({values})"));
        }
        if let Some(window_id) = self.window_id {
            parts.push(format!("window_id = {window_id}"));
        }
        (!parts.is_empty()).then(|| parts.join(" AND "))
    }
}

#[derive(Clone, Debug)]
pub(crate) struct IndexCheckpoint {
    compatible: bool,
    sources: BTreeMap<String, String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SyncOutcome {
    Current,
    Retry,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct IndexManifest {
    schema_version: u32,
    corpus: String,
    source: String,
    extractor: String,
    zvec_version: String,
    model_id: String,
    model_revision: String,
    model_sha256: String,
    tokenizer_sha256: String,
    dimension: usize,
    metric: String,
    source_digest: String,
    sources: BTreeMap<String, String>,
    fragments: usize,
}

impl IndexManifest {
    fn expected(spec: &IndexSpec, catalog: &CorpusCatalog, fragments: usize) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            corpus: spec.corpus.clone(),
            source: spec.source.clone(),
            extractor: spec.extractor.clone(),
            zvec_version: ZVEC_VERSION.to_string(),
            model_id: MODEL_ID.to_string(),
            model_revision: MODEL_REVISION.to_string(),
            model_sha256: MODEL_SHA256.to_string(),
            tokenizer_sha256: TOKENIZER_SHA256.to_string(),
            dimension: EMBEDDING_DIMENSION,
            metric: "cosine".to_string(),
            source_digest: catalog.digest.clone(),
            sources: catalog.sources.clone(),
            fragments,
        }
    }

    fn compatible(&self, spec: &IndexSpec) -> bool {
        self.schema_version == SCHEMA_VERSION
            && self.corpus == spec.corpus
            && self.source == spec.source
            && self.extractor == spec.extractor
            && self.zvec_version == ZVEC_VERSION
            && self.model_id == MODEL_ID
            && self.model_revision == MODEL_REVISION
            && self.model_sha256 == MODEL_SHA256
            && self.tokenizer_sha256 == TOKENIZER_SHA256
            && self.dimension == EMBEDDING_DIMENSION
            && self.metric == "cosine"
    }

    fn usable_checkpoint(&self, spec: &IndexSpec) -> bool {
        self.compatible(spec) && self.source_digest == source_digest(&self.sources)
    }
}

pub(crate) async fn index_checkpoint(spec: &IndexSpec) -> Result<IndexCheckpoint> {
    let spec = spec.clone();
    run_blocking(move || {
        let _lock = IndexLock::shared(&spec.directory)?;
        let manifest = read_manifest(&spec);
        let compatible = manifest
            .as_ref()
            .is_some_and(|manifest| manifest.usable_checkpoint(&spec) && collection_exists(&spec));
        Ok(IndexCheckpoint {
            compatible,
            sources: manifest
                .filter(|_| compatible)
                .map_or_else(BTreeMap::new, |manifest| manifest.sources),
        })
    })
    .await
}

pub(crate) async fn index_status(spec: &IndexSpec, catalog: &CorpusCatalog) -> Result<IndexStatus> {
    let spec = spec.clone();
    let catalog = catalog.clone();
    run_blocking(move || {
        let _lock = IndexLock::shared(&spec.directory)?;
        Ok(read_status(&spec, &catalog))
    })
    .await
}

pub(crate) async fn rebuild_index(
    spec: &IndexSpec,
    snapshot: CorpusSnapshot,
    cancellation: CancellationToken,
) -> Result<IndexStatus> {
    let spec = spec.clone();
    run_blocking(move || rebuild_index_blocking(&spec, snapshot, &cancellation)).await
}

pub(crate) async fn sync_index(
    spec: &IndexSpec,
    catalog: &CorpusCatalog,
    snapshot: CorpusSnapshot,
    cancellation: CancellationToken,
) -> Result<bool> {
    ensure!(
        snapshot.catalog == *catalog,
        "semantic sync snapshot does not match its catalog"
    );
    let spec = spec.clone();
    let catalog = catalog.clone();
    run_blocking(move || {
        sync_index_blocking(&spec, &catalog, snapshot.fragments, &cancellation)
            .map(|outcome| outcome == SyncOutcome::Current)
    })
    .await
}

pub(crate) async fn search_index(
    spec: &IndexSpec,
    catalog: &CorpusCatalog,
    query: &str,
    mode: SearchMode,
    limit: usize,
    filter: SearchFilter,
    cancellation: CancellationToken,
) -> Result<Vec<SearchHit>> {
    ensure!(
        !query.trim().is_empty(),
        "semantic search query must not be empty"
    );
    let spec = spec.clone();
    let catalog = catalog.clone();
    let query = query.to_string();
    run_blocking(move || {
        search_index_blocking(&spec, &catalog, &query, mode, limit, &filter, &cancellation)
    })
    .await
}

fn rebuild_index_blocking(
    spec: &IndexSpec,
    snapshot: CorpusSnapshot,
    cancellation: &CancellationToken,
) -> Result<IndexStatus> {
    let _lock = IndexLock::exclusive(&spec.directory)?;
    rebuild_index_locked(spec, snapshot, cancellation)
}

fn rebuild_index_locked(
    spec: &IndexSpec,
    snapshot: CorpusSnapshot,
    cancellation: &CancellationToken,
) -> Result<IndexStatus> {
    check_cancellation(cancellation)?;
    initialize_zvec()?;
    let model = embedding_model()?;
    let catalog = snapshot.catalog.clone();
    ensure!(
        snapshot.fragments.keys().eq(catalog.sources.keys()),
        "full semantic rebuild requires every catalog source"
    );
    let fragments = snapshot
        .flattened()
        .into_iter()
        .map(Fragment::sanitize)
        .collect::<Vec<_>>();
    let expected = IndexManifest::expected(spec, &catalog, fragments.len());
    let parent = spec
        .directory
        .parent()
        .ok_or_else(|| anyhow!("semantic index directory has no parent"))?;
    ensure_private_index_parent(&spec.directory).with_context(|| {
        format!(
            "cannot create semantic index directory: {}",
            parent.display()
        )
    })?;
    let temporary = parent.join(format!(
        ".{}.{}.tmp",
        spec.directory
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("index"),
        Uuid::now_v7().simple()
    ));
    let backup = parent.join(format!(
        ".{}.{}.old",
        spec.directory
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("index"),
        Uuid::now_v7().simple()
    ));
    ensure_private_directory(&temporary)?;

    let build_result = (|| -> Result<()> {
        let collection_path = temporary.join(COLLECTION_DIRECTORY);
        let schema = collection_schema(&jieba_directory()?)?;
        let collection = Collection::create_and_open(path_text(&collection_path)?, &schema, None)
            .context("cannot create zvec semantic collection")?;
        write_fragments(
            &collection,
            &model,
            &fragments,
            FragmentWrite::Insert,
            cancellation,
        )?;
        check_cancellation(cancellation)?;
        collection.optimize()?;
        check_cancellation(cancellation)?;
        collection.flush()?;
        check_cancellation(cancellation)?;
        drop(collection);
        write_manifest(&temporary, &expected)?;
        Ok(())
    })();
    if let Err(error) = build_result {
        let _ = fs::remove_dir_all(&temporary);
        return Err(error).context("cannot rebuild semantic index");
    }

    if let Err(error) = check_cancellation(cancellation) {
        let _ = fs::remove_dir_all(&temporary);
        return Err(error);
    }
    let had_previous = spec.directory.exists();
    if had_previous {
        fs::rename(&spec.directory, &backup).with_context(|| {
            format!(
                "cannot preserve previous semantic index: {}",
                spec.directory.display()
            )
        })?;
    }
    if let Err(error) = fs::rename(&temporary, &spec.directory) {
        if had_previous {
            let _ = fs::rename(&backup, &spec.directory);
        }
        let _ = fs::remove_dir_all(&temporary);
        return Err(error).with_context(|| {
            format!(
                "cannot activate rebuilt semantic index: {}",
                spec.directory.display()
            )
        });
    }
    if had_previous {
        let _ = fs::remove_dir_all(&backup);
    }
    Ok(IndexStatus {
        state: IndexState::Current,
        indexed_fragments: expected.fragments,
        current_sources: expected.sources.len(),
    })
}

fn sync_index_blocking(
    spec: &IndexSpec,
    catalog: &CorpusCatalog,
    mut fragments: BTreeMap<String, Vec<Fragment>>,
    cancellation: &CancellationToken,
) -> Result<SyncOutcome> {
    let _lock = IndexLock::exclusive(&spec.directory)?;
    check_cancellation(cancellation)?;
    let manifest = read_manifest(spec)
        .filter(|manifest| manifest.usable_checkpoint(spec) && collection_exists(spec));
    let previous_sources = manifest
        .as_ref()
        .map_or_else(BTreeMap::new, |manifest| manifest.sources.clone());
    let changed = catalog
        .sources
        .iter()
        .filter(|(source, revision)| previous_sources.get(*source) != Some(*revision))
        .map(|(source, _)| source.clone())
        .collect::<BTreeSet<_>>();
    if changed.iter().any(|source| !fragments.contains_key(source)) {
        return Ok(SyncOutcome::Retry);
    }
    if manifest.is_none() {
        if catalog
            .sources
            .keys()
            .any(|source| !fragments.contains_key(source))
        {
            return Ok(SyncOutcome::Retry);
        }
        return rebuild_index_locked(
            spec,
            CorpusSnapshot::new(catalog.clone(), fragments)?,
            cancellation,
        )
        .map(|_| SyncOutcome::Current);
    }
    if changed.is_empty() && previous_sources.len() == catalog.sources.len() {
        return Ok(SyncOutcome::Current);
    }

    initialize_zvec()?;
    let manifest_path = spec.directory.join(MANIFEST_FILE);
    fs::remove_file(&manifest_path).with_context(|| {
        format!(
            "cannot invalidate semantic index manifest: {}",
            manifest_path.display()
        )
    })?;
    let collection_path = spec.directory.join(COLLECTION_DIRECTORY);
    let collection = Collection::open(path_text(&collection_path)?, None)
        .context("cannot open zvec semantic collection for refresh")?;

    let removed = previous_sources
        .keys()
        .filter(|source| !catalog.sources.contains_key(*source))
        .cloned()
        .collect::<BTreeSet<_>>();
    for source in removed.iter().chain(changed.iter()) {
        check_cancellation(cancellation)?;
        collection.delete_by_filter(&format!("catalog_source = {}", filter_string(source)))?;
    }
    let mut model = None;
    for source in &changed {
        let source_fragments = fragments
            .remove(source)
            .expect("changed semantic source was checked above")
            .into_iter()
            .map(Fragment::sanitize)
            .collect::<Vec<_>>();
        if !source_fragments.is_empty() {
            let model = match &model {
                Some(model) => model,
                None => model.insert(embedding_model()?),
            };
            write_fragments(
                &collection,
                model,
                &source_fragments,
                FragmentWrite::Upsert,
                cancellation,
            )?;
        }
    }
    check_cancellation(cancellation)?;
    collection.flush()?;
    check_cancellation(cancellation)?;
    let fragment_count = usize::try_from(collection.stats()?.doc_count)
        .context("semantic index fragment count exceeds usize")?;
    drop(collection);
    let expected = IndexManifest::expected(spec, catalog, fragment_count);
    write_manifest(&spec.directory, &expected)?;
    if cancellation.is_cancelled() {
        let _ = fs::remove_file(&manifest_path);
        bail!("semantic indexing was cancelled before activation");
    }
    Ok(SyncOutcome::Current)
}

#[derive(Clone, Copy)]
enum FragmentWrite {
    Insert,
    Upsert,
}

fn write_fragments(
    collection: &Collection,
    model: &embedding::Model2VecEmbedding,
    fragments: &[Fragment],
    write: FragmentWrite,
    cancellation: &CancellationToken,
) -> Result<()> {
    for batch in fragments.chunks(WRITE_BATCH) {
        check_cancellation(cancellation)?;
        let texts = batch
            .iter()
            .map(|fragment| fragment.embedding_text.as_str())
            .collect::<Vec<_>>();
        let vectors = model.embed_batch(&texts)?;
        check_cancellation(cancellation)?;
        let mut docs = Vec::with_capacity(batch.len());
        for (fragment, vector) in batch.iter().zip(vectors) {
            let mut doc = Doc::new()?;
            doc.set_pk(&fragment.id);
            doc.add_string("group_id", &fragment.group_id)?;
            doc.add_string("catalog_source", &fragment.catalog_source)?;
            doc.add_string("source", &fragment.source)?;
            doc.add_string("anchor", &fragment.anchor)?;
            doc.add_string("label", &fragment.label)?;
            doc.add_string("text", &fragment.text)?;
            doc.add_string("record_type", &fragment.record_type)?;
            doc.add_u64("window_id", fragment.window_id)?;
            doc.add_vector_f32("embedding", &vector)?;
            docs.push(doc);
        }
        let references = docs.iter().collect::<Vec<_>>();
        let result = match write {
            FragmentWrite::Insert => collection.insert(&references),
            FragmentWrite::Upsert => collection.upsert(&references),
        }?;
        if result.error_count > 0 {
            let detail = result
                .results
                .iter()
                .find(|result| !result.success)
                .map(|result| result.message.as_str())
                .unwrap_or("unknown zvec write error");
            bail!(
                "zvec rejected {} of {} semantic fragments: {detail}",
                result.error_count,
                batch.len()
            );
        }
    }
    Ok(())
}

fn search_index_blocking(
    spec: &IndexSpec,
    catalog: &CorpusCatalog,
    query: &str,
    mode: SearchMode,
    limit: usize,
    filter: &SearchFilter,
    cancellation: &CancellationToken,
) -> Result<Vec<SearchHit>> {
    let _lock = IndexLock::shared(&spec.directory)?;
    check_cancellation(cancellation)?;
    let status = read_status(spec, catalog);
    match status.state {
        IndexState::Missing => bail!("{} semantic index is unavailable", spec.corpus),
        IndexState::Stale => bail!("{} semantic index changed before search", spec.corpus),
        IndexState::Current => {}
    }
    if status.indexed_fragments == 0 {
        return Ok(Vec::new());
    }
    initialize_zvec()?;
    check_cancellation(cancellation)?;
    let vector = embedding_model()?.embed(query)?;
    ensure!(
        vector.iter().any(|value| *value != 0.0),
        "semantic search query produced no known model tokens"
    );
    check_cancellation(cancellation)?;
    let collection_path = spec.directory.join(COLLECTION_DIRECTORY);
    let collection = Collection::open(path_text(&collection_path)?, None)
        .context("cannot open zvec semantic collection")?;
    let hits = adaptive_search(
        &collection,
        query,
        &vector,
        mode,
        limit.clamp(1, MAX_RECALL),
        filter.expression().as_deref(),
        cancellation,
    )?;
    drop(collection);
    Ok(hits)
}

fn adaptive_search(
    collection: &Collection,
    text: &str,
    vector: &[f32],
    mode: SearchMode,
    requested: usize,
    filter: Option<&str>,
    cancellation: &CancellationToken,
) -> Result<Vec<SearchHit>> {
    let fusion_target = requested.saturating_mul(5).clamp(50, MAX_RECALL);
    let mut depth = INITIAL_RECALL
        .min(MAX_RECALL)
        .max(requested.min(INITIAL_RECALL));
    loop {
        check_cancellation(cancellation)?;
        let vector_docs = vector_query(collection, vector, depth, filter)?;
        let vector_exhausted = vector_docs.len() < depth;
        let hits = match mode {
            SearchMode::Semantic => collapse_semantic(vector_docs),
            SearchMode::Hybrid => {
                check_cancellation(cancellation)?;
                let keyword_docs = keyword_query(collection, text, depth, filter)?;
                let keyword_exhausted = keyword_docs.len() < depth;
                let hits = reciprocal_rank_fusion(&vector_docs, &keyword_docs);
                if hits.len() >= fusion_target
                    || depth == MAX_RECALL
                    || (vector_exhausted && keyword_exhausted)
                {
                    return Ok(hits.into_iter().take(requested).collect());
                }
                depth = (depth * 2).min(MAX_RECALL);
                continue;
            }
        };
        if hits.len() >= requested || depth == MAX_RECALL || vector_exhausted {
            return Ok(hits.into_iter().take(requested).collect());
        }
        depth = (depth * 2).min(MAX_RECALL);
    }
}

#[derive(Clone, Debug)]
struct RankedDoc {
    group_id: String,
    source: String,
    label: String,
    text: String,
    record_type: String,
}

impl RankedDoc {
    fn from_doc(doc: &Doc) -> Result<Self> {
        Ok(Self {
            group_id: required_string(doc, "group_id")?,
            source: required_string(doc, "source")?,
            label: required_string(doc, "label")?,
            text: required_string(doc, "text")?,
            record_type: required_string(doc, "record_type")?,
        })
    }

    fn hit(self) -> SearchHit {
        SearchHit {
            source: self.source,
            label: self.label,
            text: self.text,
            record_type: self.record_type,
        }
    }
}

fn vector_query(
    collection: &Collection,
    vector: &[f32],
    limit: usize,
    filter: Option<&str>,
) -> Result<Vec<RankedDoc>> {
    let mut query = SearchQuery::new("embedding", vector, limit as i32)?;
    query.set_include_vector(false)?;
    query.set_output_fields(&["group_id", "source", "label", "text", "record_type"])?;
    if let Some(filter) = filter {
        query.set_filter(filter)?;
    }
    collection
        .query(&query)?
        .iter()
        .map(RankedDoc::from_doc)
        .collect()
}

fn keyword_query(
    collection: &Collection,
    text: &str,
    limit: usize,
    filter: Option<&str>,
) -> Result<Vec<RankedDoc>> {
    let mut fts = Fts::new()?;
    fts.set_match_string(text)?;
    let mut query = SearchQuery::fts("text", &fts, limit as i32)?;
    query.set_include_vector(false)?;
    query.set_output_fields(&["group_id", "source", "label", "text", "record_type"])?;
    if let Some(filter) = filter {
        query.set_filter(filter)?;
    }
    collection
        .query(&query)?
        .iter()
        .map(RankedDoc::from_doc)
        .collect()
}

fn collapse_semantic(docs: Vec<RankedDoc>) -> Vec<SearchHit> {
    collapse_groups(docs)
        .into_iter()
        .map(RankedDoc::hit)
        .collect()
}

fn collapse_groups(docs: Vec<RankedDoc>) -> Vec<RankedDoc> {
    let mut seen = HashSet::new();
    docs.into_iter()
        .filter(|doc| seen.insert(doc.group_id.clone()))
        .collect()
}

fn reciprocal_rank_fusion(vector: &[RankedDoc], keyword: &[RankedDoc]) -> Vec<SearchHit> {
    #[derive(Clone)]
    struct Fused {
        representative: RankedDoc,
        score: f32,
        best_rank: usize,
    }

    let mut fused = HashMap::<String, Fused>::new();
    let vector = collapse_groups(vector.to_vec());
    let keyword = collapse_groups(keyword.to_vec());
    for docs in [&vector, &keyword] {
        for (index, doc) in docs.iter().enumerate() {
            let rank = index + 1;
            let entry = fused.entry(doc.group_id.clone()).or_insert_with(|| Fused {
                representative: doc.clone(),
                score: 0.0,
                best_rank: rank,
            });
            entry.score += 1.0 / (RRF_K + rank as f32);
            if rank < entry.best_rank {
                entry.representative = doc.clone();
                entry.best_rank = rank;
            }
        }
    }
    let mut hits = fused
        .into_iter()
        .map(|(group_id, fused)| (group_id, fused.clone(), fused.representative.hit()))
        .collect::<Vec<_>>();
    hits.sort_by(|left, right| {
        right
            .1
            .score
            .total_cmp(&left.1.score)
            .then_with(|| left.0.cmp(&right.0))
    });
    hits.into_iter().map(|(_, _, hit)| hit).collect()
}

fn required_string(doc: &Doc, field: &str) -> Result<String> {
    doc.get_string(field)?
        .ok_or_else(|| anyhow!("zvec result omitted required field {field}"))
}

fn collection_schema(jieba: &Path) -> Result<CollectionSchema> {
    let extra = serde_json::json!({ "jieba_dict_dir": jieba }).to_string();
    let text_index = IndexParams::fts(Some("jieba"), Some(&["lowercase"]), Some(&extra))?;
    Ok(CollectionSchema::builder("uri_agent_retrieval")
        .add_field(FieldSchema::new("group_id", DataType::String, false, 0)?)
        .add_indexed_field(
            "catalog_source",
            DataType::String,
            IndexParams::invert(false, false)?,
        )
        .add_field(FieldSchema::new("source", DataType::String, false, 0)?)
        .add_field(FieldSchema::new("anchor", DataType::String, false, 0)?)
        .add_field(FieldSchema::new("label", DataType::String, false, 0)?)
        .add_indexed_field(
            "record_type",
            DataType::String,
            IndexParams::invert(false, false)?,
        )
        .add_indexed_field(
            "window_id",
            DataType::Uint64,
            IndexParams::invert(true, false)?,
        )
        .add_indexed_field("text", DataType::String, text_index)
        .add_vector_field(
            "embedding",
            DataType::VectorFp32,
            EMBEDDING_DIMENSION as u32,
            IndexParams::hnsw(MetricType::Cosine, 16, 200)?,
        )
        .build()?)
}

fn runtime_directory() -> Result<PathBuf> {
    #[cfg(test)]
    if let Some(directory) = std::env::var_os("URI_AGENT_TEST_RETRIEVAL_ASSETS") {
        return Ok(PathBuf::from(directory));
    }
    std::env::current_exe()?
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| anyhow!("URI Agent executable has no parent directory"))
}

fn model_directory() -> Result<PathBuf> {
    Ok(runtime_directory()?
        .join("retrieval")
        .join("models")
        .join("potion-code-16M-v2"))
}

fn jieba_directory() -> Result<PathBuf> {
    let directory = runtime_directory()?.join("retrieval").join("jieba");
    for name in ["jieba.dict.utf8", "hmm_model.utf8"] {
        ensure!(
            directory.join(name).is_file(),
            "bundled Jieba asset is missing: {}",
            directory.join(name).display()
        );
    }
    Ok(directory)
}

fn embedding_model() -> Result<Arc<embedding::Model2VecEmbedding>> {
    static MODEL: OnceLock<Result<Arc<embedding::Model2VecEmbedding>, String>> = OnceLock::new();
    match MODEL.get_or_init(|| {
        embedding::Model2VecEmbedding::load(model_directory().map_err(|error| error.to_string())?)
            .map(Arc::new)
            .map_err(|error| format!("cannot load bundled embedding model: {error:#}"))
    }) {
        Ok(model) => Ok(model.clone()),
        Err(error) => Err(anyhow!(error.clone())),
    }
}

fn initialize_zvec() -> Result<()> {
    static INITIALIZED: OnceLock<Result<(), String>> = OnceLock::new();
    match INITIALIZED.get_or_init(|| zvec_rust::initialize(None).map_err(|error| error.to_string()))
    {
        Ok(()) => Ok(()),
        Err(error) => Err(anyhow!(error.clone())),
    }
}

fn path_text(path: &Path) -> Result<&str> {
    path.to_str()
        .ok_or_else(|| anyhow!("semantic index path is not valid UTF-8: {}", path.display()))
}

fn read_manifest(spec: &IndexSpec) -> Option<IndexManifest> {
    fs::read(spec.directory.join(MANIFEST_FILE))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<IndexManifest>(&bytes).ok())
}

fn collection_exists(spec: &IndexSpec) -> bool {
    spec.directory.join(COLLECTION_DIRECTORY).is_dir()
}

fn read_status(spec: &IndexSpec, catalog: &CorpusCatalog) -> IndexStatus {
    let manifest = read_manifest(spec);
    let Some(manifest) = manifest else {
        return IndexStatus {
            state: if spec.directory.exists() {
                IndexState::Stale
            } else {
                IndexState::Missing
            },
            indexed_fragments: 0,
            current_sources: catalog.sources.len(),
        };
    };
    let state = if manifest.compatible(spec)
        && manifest.source_digest == catalog.digest
        && manifest.sources == catalog.sources
        && collection_exists(spec)
    {
        IndexState::Current
    } else {
        IndexState::Stale
    };
    IndexStatus {
        state,
        indexed_fragments: manifest.fragments,
        current_sources: catalog.sources.len(),
    }
}

fn check_cancellation(cancellation: &CancellationToken) -> Result<()> {
    ensure!(
        !cancellation.is_cancelled(),
        "semantic indexing was cancelled"
    );
    Ok(())
}

fn filter_string(value: &str) -> String {
    format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"))
}

fn source_digest(sources: &BTreeMap<String, String>) -> String {
    let mut digest = Sha256::new();
    for (source, revision) in sources {
        for value in [source, revision] {
            digest.update(value.len().to_le_bytes());
            digest.update(value.as_bytes());
        }
    }
    format!("{:x}", digest.finalize())
}

fn write_manifest(directory: &Path, manifest: &IndexManifest) -> Result<()> {
    ensure_private_directory(directory)?;
    let path = directory.join(MANIFEST_FILE);
    let temporary = directory.join(format!(".{MANIFEST_FILE}.{}.tmp", Uuid::now_v7().simple()));
    let result = (|| -> Result<()> {
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(&temporary)?;
        file.write_all(&serde_json::to_vec_pretty(manifest)?)?;
        file.flush()?;
        fs::rename(&temporary, &path)?;
        set_private_file_permissions(&path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

fn ensure_private_directory(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn ensure_private_index_parent(directory: &Path) -> Result<()> {
    let parent = directory
        .parent()
        .ok_or_else(|| anyhow!("semantic index directory has no parent"))?;
    let mut descendants = Vec::new();
    let mut retrieval = None;
    for path in parent.ancestors() {
        if path.file_name().and_then(|name| name.to_str()) == Some("retrieval") {
            retrieval = Some(path);
            break;
        }
        descendants.push(path);
    }
    if let Some(retrieval) = retrieval {
        ensure_private_directory(retrieval)?;
        for path in descendants.into_iter().rev() {
            ensure_private_directory(path)?;
        }
    } else {
        ensure_private_directory(parent)?;
    }
    Ok(())
}

fn set_private_file_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

struct IndexLock {
    file: File,
}

impl IndexLock {
    fn exclusive(directory: &Path) -> Result<Self> {
        let file = Self::open(directory)?;
        file.lock_exclusive()?;
        Ok(Self { file })
    }

    fn shared(directory: &Path) -> Result<Self> {
        let file = Self::open(directory)?;
        file.lock_shared()?;
        Ok(Self { file })
    }

    fn open(directory: &Path) -> Result<File> {
        let parent = directory
            .parent()
            .ok_or_else(|| anyhow!("semantic index directory has no parent"))?;
        ensure_private_index_parent(directory)?;
        let name = directory
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("index");
        let mut options = OpenOptions::new();
        options.create(true).truncate(false).read(true).write(true);
        #[cfg(unix)]
        options.mode(0o600);
        let path = parent.join(format!("{name}.lock"));
        let file = options
            .open(&path)
            .context("cannot open semantic index lock")?;
        set_private_file_permissions(&path)?;
        Ok(file)
    }
}

impl Drop for IndexLock {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.file);
    }
}

async fn run_blocking<T, F>(operation: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    static PERMITS: OnceLock<Arc<Semaphore>> = OnceLock::new();
    let permits = PERMITS.get_or_init(|| Arc::new(Semaphore::new(2))).clone();
    let permit = permits
        .acquire_owned()
        .await
        .map_err(|_| anyhow!("semantic retrieval worker pool is closed"))?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        operation()
    })
    .await
    .context("semantic retrieval worker failed")?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ranked(group: &str, source: &str) -> RankedDoc {
        RankedDoc {
            group_id: group.to_string(),
            source: source.to_string(),
            label: String::new(),
            text: source.to_string(),
            record_type: String::new(),
        }
    }

    fn snapshot(fragments: Vec<Fragment>) -> (CorpusCatalog, CorpusSnapshot) {
        let catalog = CorpusCatalog::new(fragments.iter().map(|fragment| {
            (
                fragment.catalog_source.clone(),
                Fragment::stable_id(&[&fragment.embedding_text]),
            )
        }));
        let mut by_source = BTreeMap::<String, Vec<Fragment>>::new();
        for fragment in fragments {
            by_source
                .entry(fragment.catalog_source.clone())
                .or_default()
                .push(fragment);
        }
        let snapshot = CorpusSnapshot::new(catalog.clone(), by_source).unwrap();
        (catalog, snapshot)
    }

    fn fragment(id: &str, text: &str, record_type: &str) -> Fragment {
        Fragment {
            id: id.to_string(),
            group_id: id.to_string(),
            catalog_source: format!("{id}.rs"),
            source: format!("{id}.rs"),
            anchor: "1-3".to_string(),
            label: format!("{id}.rs:1-3"),
            text: text.to_string(),
            embedding_text: format!("path: {id}.rs\n{text}"),
            record_type: record_type.to_string(),
            window_id: 0,
        }
    }

    #[test]
    fn rrf_collapses_fragments_and_breaks_ties_by_group_id() {
        let vector = vec![ranked("b", "b1"), ranked("b", "b2"), ranked("a", "a")];
        let keyword = vec![ranked("a", "a"), ranked("b", "b1")];
        let hits = reciprocal_rank_fusion(&vector, &keyword);
        assert_eq!(
            hits.iter()
                .map(|hit| hit.source.as_str())
                .collect::<Vec<_>>(),
            ["a", "b1"]
        );
    }

    #[test]
    fn manifest_requires_exact_runtime_model_extractor_and_source_digest() {
        let directory = tempfile::tempdir().unwrap();
        let spec = IndexSpec::at(
            directory.path().join("index"),
            "test",
            "source",
            "extractor-v1",
        );
        let (catalog, _) = snapshot(vec![Fragment {
            id: "id".to_string(),
            group_id: "group".to_string(),
            catalog_source: "source".to_string(),
            source: "source".to_string(),
            anchor: "anchor".to_string(),
            label: "label".to_string(),
            text: "text".to_string(),
            embedding_text: "embed".to_string(),
            record_type: String::new(),
            window_id: 0,
        }]);
        let expected = IndexManifest::expected(&spec, &catalog, 1);
        let mut incoherent = expected.clone();
        incoherent.source_digest = "wrong".to_string();
        assert!(!incoherent.usable_checkpoint(&spec));
        fs::create_dir_all(spec.directory.join(COLLECTION_DIRECTORY)).unwrap();
        fs::write(
            spec.directory.join(MANIFEST_FILE),
            serde_json::to_vec(&expected).unwrap(),
        )
        .unwrap();
        assert_eq!(read_status(&spec, &catalog).state, IndexState::Current);

        let changed = CorpusCatalog::new(Vec::new());
        assert_eq!(read_status(&spec, &changed).state, IndexState::Stale);

        fs::write(spec.directory.join(MANIFEST_FILE), b"not json").unwrap();
        assert_eq!(read_status(&spec, &catalog).state, IndexState::Stale);
        fs::remove_dir_all(&spec.directory).unwrap();
        assert_eq!(read_status(&spec, &catalog).state, IndexState::Missing);
    }

    #[test]
    fn conversation_snapshots_keep_distinct_matching_fragments() {
        let text = format!("{}matching tail{}", "a".repeat(1_050), "b".repeat(200));
        let document = ConversationDocument {
            session_id: "session".to_string(),
            cwd: "/project".to_string(),
            anchor: "r42".to_string(),
            header: "[assistant id=r42 window=3]".to_string(),
            text,
            record_type: "assistant".to_string(),
            window_id: 3,
        };
        let catalog = conversation_catalog(std::slice::from_ref(&document));
        let sources = catalog.all_sources();
        let snapshot = conversation_snapshot(catalog, sources, vec![document]).unwrap();
        let fragments = &snapshot.fragments["session/r42"];

        assert_eq!(fragments.len(), 2);
        assert_ne!(fragments[0].text, fragments[1].text);
        assert!(fragments[1].text.contains("matching tail"));
        assert_eq!(fragments[1].label, "[assistant id=r42 window=3]");
    }

    #[test]
    fn search_filters_escape_scalar_values_and_combine_constraints() {
        assert_eq!(filter_string(r"a\b'c"), r"'a\\b\'c'");
        assert_eq!(
            SearchFilter::conversation([r"a\b'c".to_string(), "assistant".to_string()], Some(7))
                .expression()
                .as_deref(),
            Some(r"record_type IN ('a\\b\'c', 'assistant') AND window_id = 7")
        );
        assert!(SearchFilter::default().expression().is_none());
    }

    #[tokio::test]
    async fn cancellation_before_rebuild_never_activates_an_index() {
        let directory = tempfile::tempdir().unwrap();
        let spec = IndexSpec::at(
            directory.path().join("retrieval/v2/test/index"),
            "test",
            "source",
            "extractor-v1",
        );
        let catalog = CorpusCatalog::new(Vec::<(String, String)>::new());
        let snapshot = CorpusSnapshot::new(catalog, BTreeMap::new()).unwrap();
        let cancellation = CancellationToken::new();
        cancellation.cancel();

        let error = rebuild_index(&spec, snapshot, cancellation)
            .await
            .unwrap_err();

        assert!(error.to_string().contains("cancelled"));
        assert!(!spec.directory.exists());
        assert!(
            fs::read_dir(spec.directory.parent().unwrap())
                .unwrap()
                .all(|entry| !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".tmp"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn cache_directories_and_metadata_are_owner_only() {
        let directory = tempfile::tempdir().unwrap();
        let namespace = directory.path().join("retrieval/v2/context");
        let index = namespace.join("index");
        let temporary = namespace.join(".index.build.tmp");
        let spec = IndexSpec::at(index.clone(), "test", "source", "extractor-v1");
        let catalog = CorpusCatalog::new(Vec::<(String, String)>::new());
        let manifest = IndexManifest::expected(&spec, &catalog, 0);

        ensure_private_index_parent(&index).unwrap();
        ensure_private_directory(&index).unwrap();
        ensure_private_directory(&temporary).unwrap();
        write_manifest(&index, &manifest).unwrap();
        drop(IndexLock::open(&index).unwrap());

        for path in [
            directory.path().join("retrieval"),
            directory.path().join("retrieval/v2"),
            namespace,
            index.clone(),
            temporary,
        ] {
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        assert_eq!(
            fs::metadata(index.join(MANIFEST_FILE))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(index.parent().unwrap().join("index.lock"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[tokio::test]
    #[ignore = "requires fixed release retrieval assets"]
    async fn bundled_assets_rebuild_and_search() {
        assert!(
            std::env::var_os("URI_AGENT_TEST_RETRIEVAL_ASSETS").is_some(),
            "set URI_AGENT_TEST_RETRIEVAL_ASSETS to the prepared asset directory"
        );
        let directory = tempfile::tempdir().unwrap();
        let spec = IndexSpec::at(
            directory.path().join("index"),
            "integration",
            "fixture-v1",
            "integration-v1",
        );
        let (catalog, initial_snapshot) = snapshot(vec![
            fragment(
                "credentials",
                "Rotate refresh tokens and renew expired credentials safely.",
                "code",
            ),
            fragment(
                "terminal",
                "Render terminal colors and preserve the cursor position.",
                "code",
            ),
            fragment("sessions", "为历史会话建立语义索引并搜索相关内容。", "code"),
        ]);

        let status = rebuild_index(&spec, initial_snapshot, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(status.state, IndexState::Current);
        assert_eq!(
            index_status(&spec, &catalog).await.unwrap().state,
            IndexState::Current
        );

        let semantic = search_index(
            &spec,
            &catalog,
            "credential renewal",
            SearchMode::Semantic,
            3,
            SearchFilter::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(!semantic.is_empty());
        assert_eq!(semantic[0].source, "credentials.rs");

        let hybrid = search_index(
            &spec,
            &catalog,
            "refresh token rotation",
            SearchMode::Hybrid,
            3,
            SearchFilter::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(hybrid[0].source, "credentials.rs");

        let chinese = search_index(
            &spec,
            &catalog,
            "搜索历史对话",
            SearchMode::Hybrid,
            3,
            SearchFilter::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(chinese.iter().any(|hit| hit.source == "sessions.rs"));

        let refreshed = vec![
            fragment(
                "credentials",
                "Renew an expired login by rotating its refresh credential.",
                "code",
            ),
            fragment(
                "terminal",
                "Render terminal colors and preserve the cursor position.",
                "code",
            ),
        ];
        let (refreshed_catalog, _) = snapshot(refreshed.clone());
        let checkpoint = index_checkpoint(&spec).await.unwrap();
        assert_eq!(
            refreshed_catalog.changed_sources(&checkpoint),
            ["credentials.rs".to_string()].into_iter().collect()
        );
        let refreshed_snapshot = CorpusSnapshot::new(
            refreshed_catalog.clone(),
            [("credentials.rs".to_string(), vec![refreshed[0].clone()])]
                .into_iter()
                .collect(),
        )
        .unwrap();
        assert!(
            sync_index(
                &spec,
                &refreshed_catalog,
                refreshed_snapshot,
                CancellationToken::new(),
            )
            .await
            .unwrap()
        );
        let refreshed_hits = search_index(
            &spec,
            &refreshed_catalog,
            "expired login credential",
            SearchMode::Hybrid,
            3,
            SearchFilter::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(refreshed_hits[0].source, "credentials.rs");
        assert!(refreshed_hits.iter().all(|hit| hit.source != "sessions.rs"));
        assert_eq!(
            index_status(&spec, &refreshed_catalog)
                .await
                .unwrap()
                .indexed_fragments,
            2
        );

        let filtered_spec = IndexSpec::at(
            directory.path().join("filtered-index"),
            "filtered",
            "fixture-v1",
            "integration-v1",
        );
        let mut filtered_fragments = (0..250)
            .map(|index| {
                fragment(
                    &format!("assistant-{index:03}"),
                    "special migration needle with exact matching vocabulary",
                    "assistant",
                )
            })
            .collect::<Vec<_>>();
        filtered_fragments.push(fragment(
            "user-target",
            "special migration needle from the requested user record",
            "user",
        ));
        let (filtered_catalog, filtered_snapshot) = snapshot(filtered_fragments);
        rebuild_index(&filtered_spec, filtered_snapshot, CancellationToken::new())
            .await
            .unwrap();
        let filtered_hits = search_index(
            &filtered_spec,
            &filtered_catalog,
            "special migration needle",
            SearchMode::Hybrid,
            1,
            SearchFilter::conversation(["user".to_string()], None),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(filtered_hits.len(), 1);
        assert_eq!(filtered_hits[0].source, "user-target.rs");
    }
}
