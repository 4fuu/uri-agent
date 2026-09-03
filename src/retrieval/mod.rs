mod code;
mod embedding;

pub(crate) use code::code_corpus;
pub(crate) use embedding::{EMBEDDING_DIMENSION, MODEL_ID, MODEL_REVISION};

use anyhow::{Context, Result, anyhow, bail, ensure};
use fs2::FileExt as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use tokio::sync::Semaphore;
use uuid::Uuid;
use zvec_rust::{
    Collection, CollectionSchema, DataType, Doc, FieldSchema, Fts, IndexParams, MetricType,
    SearchQuery,
};

const SCHEMA_VERSION: u32 = 1;
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
    pub(crate) source: String,
    pub(crate) anchor: String,
    pub(crate) label: String,
    pub(crate) text: String,
    pub(crate) embedding_text: String,
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
        self.source = self.source.replace('\0', " ");
        self.anchor = self.anchor.replace('\0', " ");
        self.label = self.label.replace('\0', " ");
        self.text = self.text.replace('\0', " ");
        self.embedding_text = self.embedding_text.replace('\0', " ");
        self
    }
}

#[derive(Clone, Debug)]
pub(crate) struct CorpusSnapshot {
    fragments: Vec<Fragment>,
    digest: String,
}

impl CorpusSnapshot {
    pub(crate) fn new(mut fragments: Vec<Fragment>) -> Self {
        fragments = fragments.into_iter().map(Fragment::sanitize).collect();
        fragments.sort_by(|left, right| left.id.cmp(&right.id));
        let mut digest = Sha256::new();
        for fragment in &fragments {
            for value in [
                &fragment.id,
                &fragment.group_id,
                &fragment.source,
                &fragment.anchor,
                &fragment.label,
                &fragment.text,
                &fragment.embedding_text,
            ] {
                digest.update(value.len().to_le_bytes());
                digest.update(value.as_bytes());
            }
        }
        Self {
            fragments,
            digest: format!("{:x}", digest.finalize()),
        }
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.fragments.len()
    }
}

pub(crate) struct ConversationDocument {
    pub(crate) session_id: String,
    pub(crate) cwd: String,
    pub(crate) anchor: String,
    pub(crate) header: String,
    pub(crate) text: String,
}

pub(crate) struct ConversationCorpus {
    pub(crate) spec: IndexSpec,
    pub(crate) snapshot: CorpusSnapshot,
}

pub(crate) fn conversation_corpus(
    namespace: &str,
    identity: &str,
    corpus: &str,
    source: String,
    documents: Vec<ConversationDocument>,
) -> Result<ConversationCorpus> {
    let spec = IndexSpec::new(
        namespace,
        identity,
        corpus,
        source,
        CONVERSATION_EXTRACTOR_VERSION,
    )?;
    let mut fragments = Vec::new();
    for document in documents {
        let group_id = Fragment::stable_id(&[&document.session_id, &document.anchor]);
        for (index, text) in conversation_chunks(&document.text).into_iter().enumerate() {
            fragments.push(Fragment {
                id: Fragment::stable_id(&[&group_id, &index.to_string(), &text]),
                group_id: group_id.clone(),
                source: document.session_id.clone(),
                anchor: document.anchor.clone(),
                label: document.header.clone(),
                embedding_text: format!(
                    "session: {}\nworking directory: {}\n{}\n{}",
                    document.session_id, document.cwd, document.header, text
                ),
                text,
            });
        }
    }
    Ok(ConversationCorpus {
        spec,
        snapshot: CorpusSnapshot::new(fragments),
    })
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
    pub(crate) current_fragments: usize,
}

impl IndexStatus {
    pub(crate) fn format(&self, corpus: &str) -> String {
        match self.state {
            IndexState::Missing => format!(
                "{corpus} semantic index is missing · current_fragments={}",
                self.current_fragments
            ),
            IndexState::Stale => format!(
                "{corpus} semantic index is stale · indexed_fragments={} · current_fragments={}",
                self.indexed_fragments, self.current_fragments
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
    pub(crate) anchor: String,
    pub(crate) label: String,
    pub(crate) text: String,
    pub(crate) score: f32,
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
    fragments: usize,
}

impl IndexManifest {
    fn expected(spec: &IndexSpec, snapshot: &CorpusSnapshot) -> Self {
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
            source_digest: snapshot.digest.clone(),
            fragments: snapshot.fragments.len(),
        }
    }
}

pub(crate) async fn index_status(
    spec: &IndexSpec,
    snapshot: &CorpusSnapshot,
) -> Result<IndexStatus> {
    let spec = spec.clone();
    let expected = IndexManifest::expected(&spec, snapshot);
    run_blocking(move || {
        let _lock = IndexLock::shared(&spec.directory)?;
        Ok(read_status(&spec, &expected))
    })
    .await
}

pub(crate) async fn rebuild_index(
    spec: &IndexSpec,
    snapshot: CorpusSnapshot,
) -> Result<IndexStatus> {
    let spec = spec.clone();
    run_blocking(move || rebuild_index_blocking(&spec, snapshot)).await
}

pub(crate) async fn search_index(
    spec: &IndexSpec,
    snapshot: &CorpusSnapshot,
    query: &str,
    mode: SearchMode,
    limit: usize,
) -> Result<Vec<SearchHit>> {
    ensure!(
        !query.trim().is_empty(),
        "semantic search query must not be empty"
    );
    let spec = spec.clone();
    let expected = IndexManifest::expected(&spec, snapshot);
    let query = query.to_string();
    run_blocking(move || search_index_blocking(&spec, &expected, &query, mode, limit)).await
}

fn rebuild_index_blocking(spec: &IndexSpec, snapshot: CorpusSnapshot) -> Result<IndexStatus> {
    let _lock = IndexLock::exclusive(&spec.directory)?;
    initialize_zvec()?;
    let model = embedding_model()?;
    let expected = IndexManifest::expected(spec, &snapshot);
    let parent = spec
        .directory
        .parent()
        .ok_or_else(|| anyhow!("semantic index directory has no parent"))?;
    fs::create_dir_all(parent).with_context(|| {
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
    fs::create_dir_all(&temporary)?;

    let build_result = (|| -> Result<()> {
        let collection_path = temporary.join(COLLECTION_DIRECTORY);
        let schema = collection_schema(&jieba_directory()?)?;
        let collection = Collection::create_and_open(path_text(&collection_path)?, &schema, None)
            .context("cannot create zvec semantic collection")?;
        for batch in snapshot.fragments.chunks(WRITE_BATCH) {
            let texts = batch
                .iter()
                .map(|fragment| fragment.embedding_text.as_str())
                .collect::<Vec<_>>();
            let vectors = model.embed_batch(&texts)?;
            let mut docs = Vec::with_capacity(batch.len());
            for (fragment, vector) in batch.iter().zip(vectors) {
                let mut doc = Doc::new()?;
                doc.set_pk(&fragment.id);
                doc.add_string("group_id", &fragment.group_id)?;
                doc.add_string("source", &fragment.source)?;
                doc.add_string("anchor", &fragment.anchor)?;
                doc.add_string("label", &fragment.label)?;
                doc.add_string("text", &fragment.text)?;
                doc.add_vector_f32("embedding", &vector)?;
                docs.push(doc);
            }
            let references = docs.iter().collect::<Vec<_>>();
            let result = collection.insert(&references)?;
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
        collection.optimize()?;
        collection.flush()?;
        drop(collection);
        fs::write(
            temporary.join(MANIFEST_FILE),
            serde_json::to_vec_pretty(&expected)?,
        )?;
        Ok(())
    })();
    if let Err(error) = build_result {
        let _ = fs::remove_dir_all(&temporary);
        return Err(error).context("cannot rebuild semantic index");
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
        current_fragments: expected.fragments,
    })
}

fn search_index_blocking(
    spec: &IndexSpec,
    expected: &IndexManifest,
    query: &str,
    mode: SearchMode,
    limit: usize,
) -> Result<Vec<SearchHit>> {
    let _lock = IndexLock::shared(&spec.directory)?;
    let status = read_status(spec, expected);
    match status.state {
        IndexState::Missing => bail!(
            "{} semantic index is missing; build it with the protocol's index exec route",
            spec.corpus
        ),
        IndexState::Stale => bail!(
            "{} semantic index is stale; rebuild it with the protocol's index exec route",
            spec.corpus
        ),
        IndexState::Current => {}
    }
    if expected.fragments == 0 {
        return Ok(Vec::new());
    }
    initialize_zvec()?;
    let vector = embedding_model()?.embed(query)?;
    ensure!(
        vector.iter().any(|value| *value != 0.0),
        "semantic search query produced no known model tokens"
    );
    let collection_path = spec.directory.join(COLLECTION_DIRECTORY);
    let collection = Collection::open(path_text(&collection_path)?, None)
        .context("cannot open zvec semantic collection")?;
    let hits = adaptive_search(
        &collection,
        query,
        &vector,
        mode,
        limit.clamp(1, MAX_RECALL),
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
) -> Result<Vec<SearchHit>> {
    let fusion_target = requested.saturating_mul(5).clamp(50, MAX_RECALL);
    let mut depth = INITIAL_RECALL
        .min(MAX_RECALL)
        .max(requested.min(INITIAL_RECALL));
    loop {
        let vector_docs = vector_query(collection, vector, depth)?;
        let vector_exhausted = vector_docs.len() < depth;
        let hits = match mode {
            SearchMode::Semantic => collapse_semantic(vector_docs),
            SearchMode::Hybrid => {
                let keyword_docs = keyword_query(collection, text, depth)?;
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
    anchor: String,
    label: String,
    text: String,
    score: f32,
}

impl RankedDoc {
    fn from_doc(doc: &Doc) -> Result<Self> {
        Ok(Self {
            group_id: required_string(doc, "group_id")?,
            source: required_string(doc, "source")?,
            anchor: required_string(doc, "anchor")?,
            label: required_string(doc, "label")?,
            text: required_string(doc, "text")?,
            score: doc.get_score(),
        })
    }

    fn hit(self, score: f32) -> SearchHit {
        SearchHit {
            source: self.source,
            anchor: self.anchor,
            label: self.label,
            text: self.text,
            score,
        }
    }
}

fn vector_query(collection: &Collection, vector: &[f32], limit: usize) -> Result<Vec<RankedDoc>> {
    let mut query = SearchQuery::new("embedding", vector, limit as i32)?;
    query.set_include_vector(false)?;
    query.set_output_fields(&["group_id", "source", "anchor", "label", "text"])?;
    collection
        .query(&query)?
        .iter()
        .map(RankedDoc::from_doc)
        .collect()
}

fn keyword_query(collection: &Collection, text: &str, limit: usize) -> Result<Vec<RankedDoc>> {
    let mut fts = Fts::new()?;
    fts.set_match_string(text)?;
    let mut query = SearchQuery::fts("text", &fts, limit as i32)?;
    query.set_include_vector(false)?;
    query.set_output_fields(&["group_id", "source", "anchor", "label", "text"])?;
    collection
        .query(&query)?
        .iter()
        .map(RankedDoc::from_doc)
        .collect()
}

fn collapse_semantic(docs: Vec<RankedDoc>) -> Vec<SearchHit> {
    collapse_groups(docs)
        .into_iter()
        .map(|doc| {
            let score = doc.score;
            doc.hit(score)
        })
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
        .map(|(group_id, fused)| {
            (
                group_id,
                fused.clone(),
                fused.representative.hit(fused.score),
            )
        })
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
        .add_field(FieldSchema::new("source", DataType::String, false, 0)?)
        .add_field(FieldSchema::new("anchor", DataType::String, false, 0)?)
        .add_field(FieldSchema::new("label", DataType::String, false, 0)?)
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

fn read_status(spec: &IndexSpec, expected: &IndexManifest) -> IndexStatus {
    let manifest = fs::read(spec.directory.join(MANIFEST_FILE))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<IndexManifest>(&bytes).ok());
    let Some(manifest) = manifest else {
        return IndexStatus {
            state: if spec.directory.exists() {
                IndexState::Stale
            } else {
                IndexState::Missing
            },
            indexed_fragments: 0,
            current_fragments: expected.fragments,
        };
    };
    let state = if manifest == *expected && spec.directory.join(COLLECTION_DIRECTORY).is_dir() {
        IndexState::Current
    } else {
        IndexState::Stale
    };
    IndexStatus {
        state,
        indexed_fragments: manifest.fragments,
        current_fragments: expected.fragments,
    }
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
        fs::create_dir_all(parent)?;
        let name = directory
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("index");
        OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(parent.join(format!("{name}.lock")))
            .context("cannot open semantic index lock")
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
            anchor: "1-2".to_string(),
            label: String::new(),
            text: source.to_string(),
            score: 1.0,
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
        assert_eq!(hits[0].score, hits[1].score);
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
        let snapshot = CorpusSnapshot::new(vec![Fragment {
            id: "id".to_string(),
            group_id: "group".to_string(),
            source: "source".to_string(),
            anchor: "anchor".to_string(),
            label: "label".to_string(),
            text: "text".to_string(),
            embedding_text: "embed".to_string(),
        }]);
        let expected = IndexManifest::expected(&spec, &snapshot);
        fs::create_dir_all(spec.directory.join(COLLECTION_DIRECTORY)).unwrap();
        fs::write(
            spec.directory.join(MANIFEST_FILE),
            serde_json::to_vec(&expected).unwrap(),
        )
        .unwrap();
        assert_eq!(read_status(&spec, &expected).state, IndexState::Current);

        let changed = CorpusSnapshot::new(Vec::new());
        assert_eq!(
            read_status(&spec, &IndexManifest::expected(&spec, &changed)).state,
            IndexState::Stale
        );

        fs::write(spec.directory.join(MANIFEST_FILE), b"not json").unwrap();
        assert_eq!(read_status(&spec, &expected).state, IndexState::Stale);
        fs::remove_dir_all(&spec.directory).unwrap();
        assert_eq!(read_status(&spec, &expected).state, IndexState::Missing);
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
        let fragment = |id: &str, text: &str| Fragment {
            id: id.to_string(),
            group_id: id.to_string(),
            source: format!("{id}.rs"),
            anchor: "1-3".to_string(),
            label: format!("{id}.rs:1-3"),
            text: text.to_string(),
            embedding_text: format!("path: {id}.rs\n{text}"),
        };
        let snapshot = CorpusSnapshot::new(vec![
            fragment(
                "credentials",
                "Rotate refresh tokens and renew expired credentials safely.",
            ),
            fragment(
                "terminal",
                "Render terminal colors and preserve the cursor position.",
            ),
            fragment("sessions", "为历史会话建立语义索引并搜索相关内容。"),
        ]);

        let status = rebuild_index(&spec, snapshot.clone()).await.unwrap();
        assert_eq!(status.state, IndexState::Current);
        assert_eq!(
            index_status(&spec, &snapshot).await.unwrap().state,
            IndexState::Current
        );

        let semantic = search_index(
            &spec,
            &snapshot,
            "credential renewal",
            SearchMode::Semantic,
            3,
        )
        .await
        .unwrap();
        assert!(!semantic.is_empty());
        assert!(semantic.iter().any(|hit| hit.source == "credentials.rs"));

        let hybrid = search_index(
            &spec,
            &snapshot,
            "refresh token rotation",
            SearchMode::Hybrid,
            3,
        )
        .await
        .unwrap();
        assert!(hybrid.iter().any(|hit| hit.source == "credentials.rs"));

        let chinese = search_index(&spec, &snapshot, "搜索历史对话", SearchMode::Hybrid, 3)
            .await
            .unwrap();
        assert!(chinese.iter().any(|hit| hit.source == "sessions.rs"));
    }
}
