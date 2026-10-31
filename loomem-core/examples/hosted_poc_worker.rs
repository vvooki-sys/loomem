//! Synthetic shared-engine experiment; Hosted owns authorization and recovery.
//! One process holds multiple explicitly addressed scopes. No production use.
use anyhow::{bail, Context, Result};
use loomem_core::{
    config::Config,
    hybrid_search::HybridSearchEngine,
    local_embeddings::LocalEmbedder,
    storage::{Chunk, RocksDbStore},
    tantivy_index::{TantivyIndex, TextDocument},
    vector_search,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    fs,
    io::{self, BufRead, Write},
    path::{Path, PathBuf},
    sync::Arc,
    thread::JoinHandle,
};
use tantivy::{
    collector::TopDocs,
    query::TermQuery,
    schema::{IndexRecordOption, Value as TantivyValue},
    TantivyDocument, Term,
};

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ScopeContext {
    actor: String,
    scope: String,
    epoch: i64,
}

impl ScopeContext {
    fn from_request(request: &Value) -> Result<Self> {
        let context: Self = serde_json::from_value(
            request
                .get("context")
                .context("missing scope context")?
                .clone(),
        )?;
        if !safe_name(&context.actor) || !safe_name(&context.scope) || context.epoch < 0 {
            bail!("invalid scope context")
        }
        Ok(context)
    }

    fn json(&self) -> Result<Value> {
        Ok(serde_json::to_value(self)?)
    }
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    op_id: String,
    id: String,
    content: String,
    embedding: Vec<f32>,
}

#[derive(Serialize)]
struct Score {
    id: String,
    score: f32,
}

struct ScopeState {
    root: PathBuf,
    jobs: HashMap<String, JoinHandle<Result<Value>>>,
}

struct SharedEngine {
    root: PathBuf,
    config: Config,
    model: Arc<LocalEmbedder>,
    contexts: HashMap<ScopeContext, ScopeState>,
}

fn safe_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

fn string_field<'a>(request: &'a Value, key: &str) -> Result<&'a str> {
    request
        .get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("missing string field {key}"))
}

fn chunk(record: &Record) -> Result<Chunk> {
    Ok(serde_json::from_value(json!({
        "id": record.id, "content": record.content, "stream": "__user_default__", "level": 0,
        "score": 1.0, "timestamp": 1, "consolidated": false, "dormant": false,
        "in_progress": false, "prompt_version": null, "source_ids": null, "last_decay": null,
        "metadata": null
    }))?)
}

fn document(chunk: &Chunk) -> TextDocument {
    TextDocument {
        id: chunk.id.clone(),
        content: chunk.content.clone(),
        user_id: String::new(),
        app_id: String::new(),
        level: chunk.level,
        timestamp: 1,
        stream: chunk.stream.clone(),
        entities: None,
        relations: None,
        event_date: None,
        source_agent: None,
    }
}

fn validate_records(records: &[Record]) -> Result<()> {
    let mut ids = std::collections::HashSet::new();
    let mut operations = std::collections::HashSet::new();
    for record in records {
        if !ids.insert(&record.id) || !operations.insert(&record.op_id) {
            bail!("duplicate record")
        }
        if !safe_name(&record.id)
            || !safe_name(&record.op_id)
            || record.embedding.len() != 384
            || record.embedding.iter().any(|value| !value.is_finite())
        {
            bail!("invalid record")
        }
    }
    Ok(())
}

fn validate_index_record(
    searcher: &tantivy::Searcher,
    id_field: tantivy::schema::Field,
    content_field: tantivy::schema::Field,
    record: &Record,
) -> Result<()> {
    let query = TermQuery::new(
        Term::from_field_text(id_field, &record.id),
        IndexRecordOption::Basic,
    );
    let docs = searcher.search(&query, &TopDocs::with_limit(2))?;
    if docs.len() != 1 {
        bail!("indexed record mismatch");
    }
    let doc: TantivyDocument = searcher.doc(docs[0].1)?;
    if doc
        .get_first(content_field)
        .and_then(|value| value.as_str())
        != Some(record.content.as_str())
    {
        bail!("indexed content mismatch");
    }
    Ok(())
}

fn validate_counts(store: &RocksDbStore, index: &TantivyIndex, expected: usize) -> Result<()> {
    if store.get_all_chunks()?.len() != expected
        || store.get_all_embeddings()?.len() != expected
        || usize::try_from(index.count()?)? != expected
    {
        bail!("generation count mismatch")
    }
    Ok(())
}

fn validate_storage_record(store: &RocksDbStore, record: &Record) -> Result<()> {
    let stored = store.get_chunk(&record.id)?.context("missing chunk")?;
    if stored.content != record.content
        || store.get_embedding(&record.id)?.as_ref() != Some(&record.embedding)
    {
        bail!("stored record mismatch")
    }
    Ok(())
}

fn open_validated_store(path: &Path, config: &Config, expected: usize) -> Result<RocksDbStore> {
    let store = RocksDbStore::open(path.join("rocks"), &config.storage.rocksdb)?;
    let index = TantivyIndex::open(path.join("index"), &config.storage.tantivy)?;
    validate_counts(&store, &index, expected)?;
    Ok(store)
}

fn validate_generation(path: &Path, config: &Config, records: &[Record]) -> Result<()> {
    let store = open_validated_store(path, config, records.len())?;
    let index = tantivy::Index::open_in_dir(path.join("index"))?;
    let schema = index.schema();
    let id_field = schema.get_field("id")?;
    let content_field = schema.get_field("content")?;
    let reader = index.reader()?;
    let searcher = reader.searcher();
    for record in records {
        validate_storage_record(&store, record)?;
        validate_index_record(&searcher, id_field, content_field, record)?;
    }
    Ok(())
}

fn generation_path(root: &Path, generation: &str) -> Result<PathBuf> {
    if !safe_name(generation) {
        bail!("invalid generation")
    }
    Ok(root.join(generation))
}

fn ready_path(root: &Path, generation: &str) -> Result<PathBuf> {
    let path = generation_path(root, generation)?;
    if !fs::symlink_metadata(&path)?.file_type().is_dir()
        || !fs::symlink_metadata(path.join("READY"))?
            .file_type()
            .is_file()
    {
        bail!("generation unavailable")
    }
    Ok(path)
}

fn inject_failpoint(failpoint: Option<&str>) -> Result<()> {
    if failpoint == Some("after_rocksdb_wait") {
        eprintln!("LOOMEM_POC_BARRIER_AFTER_ROCKSDB");
        loop {
            std::thread::park();
        }
    }
    if failpoint == Some("after_rocksdb_abort") {
        std::process::abort();
    }
    if failpoint == Some("after_rocksdb") {
        bail!("injected failure after RocksDB")
    }
    Ok(())
}

fn write_generation(
    path: &Path,
    config: &Config,
    records: &[Record],
    failpoint: Option<&str>,
) -> Result<()> {
    let store = RocksDbStore::open(path.join("rocks"), &config.storage.rocksdb)?;
    let mut index = TantivyIndex::open(path.join("index"), &config.storage.tantivy)?;
    for record in records {
        let stored = chunk(record)?;
        store.store_chunk(&stored)?;
        store.store_embedding(&stored.id, record.embedding.clone())?;
        inject_failpoint(failpoint)?;
        index.upsert_chunk_document(document(&stored), &stored)?;
    }
    index.commit()?;
    Ok(())
}

fn build_generation(
    root: &Path,
    config: &Config,
    generation: &str,
    records: &[Record],
    failpoint: Option<&str>,
) -> Result<Value> {
    validate_records(records)?;
    let path = generation_path(root, generation)?;
    fs::create_dir(&path).context("generation already exists or cannot be created")?;
    write_generation(&path, config, records, failpoint)?;
    validate_generation(&path, config, records)?;
    let marker = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path.join("READY"))?;
    marker.sync_all()?;
    fs::File::open(&path)?.sync_all()?;
    Ok(json!({"generation":generation,"count":records.len()}))
}

fn search_generation(
    root: &Path,
    config: &Config,
    generation: &str,
    query: &str,
    embedding: &[f32],
) -> Result<Value> {
    if embedding.len() != 384 || embedding.iter().any(|value| !value.is_finite()) {
        bail!("invalid query embedding")
    }
    let path = ready_path(root, generation)?;
    let store = RocksDbStore::open(path.join("rocks"), &config.storage.rocksdb)?;
    let index = TantivyIndex::open(path.join("index"), &config.storage.tantivy)?;
    let embeddings = store.get_all_embeddings()?;
    let lexical = index.search(query, 20)?;
    let hybrid = HybridSearchEngine::new(config.clone()).search_with_vector(
        lexical.clone(),
        &embeddings,
        embedding,
        Some(&store),
        None,
    )?;
    let vector: Vec<Score> = vector_search::vector_search(&embeddings, embedding, 20)
        .into_iter()
        .map(|(id, score)| Score { id, score })
        .collect();
    let bm25: Vec<Score> = lexical
        .into_iter()
        .map(|result| Score {
            id: result.id,
            score: result.score,
        })
        .collect();
    Ok(json!({"generation":generation,"vector":vector,"bm25":bm25,"hybrid":hybrid}))
}

fn read_generation(root: &Path, config: &Config, generation: &str, id: &str) -> Result<Value> {
    if !safe_name(id) {
        bail!("invalid id")
    }
    let path = ready_path(root, generation)?;
    let store = RocksDbStore::open(path.join("rocks"), &config.storage.rocksdb)?;
    Ok(json!({"generation":generation,"chunk":store.get_chunk(id)?}))
}

fn prepare_embedding(model: &LocalEmbedder, request: &Value) -> Result<Value> {
    let embedding = model.embed(string_field(request, "content")?)?;
    Ok(json!({"op_id":string_field(request,"op_id")?,
        "generation":string_field(request,"generation")?,"embedding":embedding}))
}

fn search_request(root: &Path, config: &Config, request: &Value) -> Result<Value> {
    let embedding: Vec<f32> = serde_json::from_value(
        request
            .get("embedding")
            .context("missing embedding")?
            .clone(),
    )?;
    search_generation(
        root,
        config,
        string_field(request, "generation")?,
        string_field(request, "query")?,
        &embedding,
    )
}

fn read_request(root: &Path, config: &Config, request: &Value) -> Result<Value> {
    read_generation(
        root,
        config,
        string_field(request, "generation")?,
        string_field(request, "id")?,
    )
}

fn run_scoped_build(
    root: PathBuf,
    config: Config,
    context: ScopeContext,
    generation: String,
    records: Vec<Record>,
    failpoint: Option<String>,
) -> Result<Value> {
    let mut result = build_generation(&root, &config, &generation, &records, failpoint.as_deref())?;
    let object = result.as_object_mut().context("build result type")?;
    object.insert("context".into(), context.json()?);
    object.insert("engine_pid".into(), json!(std::process::id()));
    Ok(result)
}

impl SharedEngine {
    fn open_scope(&mut self, context: ScopeContext, request: &Value) -> Result<Value> {
        let scoped_root = self.root.join(&context.scope);
        let root = request
            .get("root")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .unwrap_or(scoped_root);
        if !fs::symlink_metadata(&root)?.file_type().is_dir() {
            bail!("scope root must be a directory")
        }
        let context_root = root.canonicalize()?;
        if request.get("root").is_none() && context_root.parent() != Some(self.root.as_path()) {
            bail!("scope root escapes shared engine")
        }
        let state = self
            .contexts
            .entry(context.clone())
            .or_insert_with(|| ScopeState {
                root: context_root.clone(),
                jobs: HashMap::new(),
            });
        if !state.jobs.is_empty() {
            bail!("cannot retarget a scope with active jobs")
        }
        state.root = context_root;
        Ok(json!({"opened":true,"contexts":self.contexts.len(),
            "context":context.json()?,"engine_pid":std::process::id()}))
    }

    fn scope_root(&self, context: &ScopeContext) -> Result<PathBuf> {
        Ok(self
            .contexts
            .get(context)
            .context("scope is not open")?
            .root
            .clone())
    }

    fn spawn_build(&mut self, context: ScopeContext, request: &Value) -> Result<Value> {
        let generation = string_field(request, "generation")?.to_owned();
        let records: Vec<Record> =
            serde_json::from_value(request.get("records").context("missing records")?.clone())?;
        validate_records(&records)?;
        let failpoint = request
            .get("failpoint")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let root = self.scope_root(&context)?;
        let config = self.config.clone();
        let job_context = context.clone();
        let job_generation = generation.clone();
        let state = self
            .contexts
            .get_mut(&context)
            .context("scope is not open")?;
        if state.jobs.contains_key(&generation) {
            bail!("generation job already exists")
        }
        let handle = std::thread::spawn(move || {
            run_scoped_build(
                root,
                config,
                job_context,
                job_generation,
                records,
                failpoint,
            )
        });
        state.jobs.insert(generation.clone(), handle);
        Ok(
            json!({"pending":true,"job":generation,"context":context.json()?,
            "engine_pid":std::process::id()}),
        )
    }

    fn await_job(&mut self, context: &ScopeContext, request: &Value) -> Result<Value> {
        let job = string_field(request, "job")?;
        let handle = self
            .contexts
            .get_mut(context)
            .context("scope is not open")?
            .jobs
            .remove(job)
            .context("job is absent from this scope")?;
        handle
            .join()
            .map_err(|_| anyhow::anyhow!("scoped job panicked"))?
    }

    fn execute(&mut self, request: Value) -> Result<Value> {
        let context = ScopeContext::from_request(&request)?;
        let command = string_field(&request, "cmd")?;
        if command == "open_scope" {
            return self.open_scope(context, &request);
        }
        let root = self.scope_root(&context)?;
        let result = match command {
            "prepare" => prepare_embedding(&self.model, &request)?,
            "build_async" => return self.spawn_build(context, &request),
            "await" => return self.await_job(&context, &request),
            "search" => search_request(&root, &self.config, &request)?,
            "read" => read_request(&root, &self.config, &request)?,
            "status" => json!({"contexts":self.contexts.len()}),
            _ => bail!("unknown command"),
        };
        bind_result(result, &context)
    }
}

fn bind_result(mut result: Value, context: &ScopeContext) -> Result<Value> {
    let object = result.as_object_mut().context("result type")?;
    object.insert("context".into(), context.json()?);
    object.insert("engine_pid".into(), json!(std::process::id()));
    Ok(result)
}

fn is_shutdown(request: &Result<Value>) -> bool {
    request
        .as_ref()
        .ok()
        .and_then(|value| value.get("cmd"))
        .and_then(Value::as_str)
        == Some("shutdown")
}

fn failed_response(context: Option<ScopeContext>, error: &anyhow::Error) -> Value {
    let mut response = json!({"ok":false,"error":format!("{error:#}")});
    if let Some(context) = context {
        if let (Some(object), Ok(context)) = (response.as_object_mut(), context.json()) {
            object.insert("context".into(), context);
            object.insert("engine_pid".into(), json!(std::process::id()));
        }
    }
    response
}

fn execute_request(engine: &mut SharedEngine, request: Result<Value>) -> Value {
    let context = request
        .as_ref()
        .ok()
        .and_then(|value| ScopeContext::from_request(value).ok());
    request
        .and_then(|value| engine.execute(value))
        .map(|mut value| {
            if let Some(object) = value.as_object_mut() {
                object.insert("ok".into(), json!(true));
            }
            value
        })
        .unwrap_or_else(|error| failed_response(context, &error))
}

fn load_engine(args: &[String]) -> Result<SharedEngine> {
    if args.len() != 4 {
        bail!("usage: hosted_poc_worker SHARED_ROOT CONFIG MODEL_DIR")
    }
    let root = Path::new(&args[1]);
    if !fs::symlink_metadata(root)?.file_type().is_dir() {
        bail!("shared root must be a directory")
    }
    Ok(SharedEngine {
        root: root.canonicalize()?,
        config: Config::load(&args[2])?,
        model: Arc::new(LocalEmbedder::load(Path::new(&args[3]), 384)?),
        contexts: HashMap::new(),
    })
}

fn serve(mut engine: SharedEngine) -> Result<()> {
    let stdin = io::stdin();
    let mut out = io::stdout().lock();
    for line in stdin.lock().lines() {
        let request: Result<Value> = serde_json::from_str(&line?).context("invalid JSON request");
        if is_shutdown(&request) {
            writeln!(out, "{{\"ok\":true}}")?;
            out.flush()?;
            break;
        }
        let response = execute_request(&mut engine, request);
        writeln!(out, "{}", serde_json::to_string(&response)?)?;
        out.flush()?;
    }
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    serve(load_engine(&args)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_generation_paths() {
        assert!(safe_name("generation_12-ok"));
        for name in ["", ".", "..", "../other", "/tmp/other", "a/b", "a\\b"] {
            assert!(!safe_name(name));
        }
    }

    #[test]
    fn every_operation_requires_explicit_valid_context() {
        let valid = json!({"actor":"actor-a","scope":"scope-a","epoch":3});
        assert_eq!(
            ScopeContext::from_request(&json!({"context":valid})).expect("valid context"),
            ScopeContext {
                actor: "actor-a".into(),
                scope: "scope-a".into(),
                epoch: 3,
            }
        );
        for invalid in [
            json!({}),
            json!({"context":{}}),
            json!({"context":{"actor":"actor-a","scope":"../b","epoch":3}}),
            json!({"context":{"actor":"actor-a","scope":"scope-a","epoch":-1}}),
        ] {
            assert!(ScopeContext::from_request(&invalid).is_err());
        }
    }

    #[test]
    fn synthetic_record_becomes_real_loomem_chunk() {
        let record = Record {
            op_id: "op1".into(),
            id: "memory1".into(),
            content: "synthetic fact".into(),
            embedding: vec![1.0; 384],
        };
        let actual = chunk(&record).expect("synthetic chunk must deserialize");
        assert_eq!(actual.id, "memory1");
        assert_eq!(actual.content, "synthetic fact");
        assert_eq!(actual.stream, "__user_default__");
        assert_eq!(document(&actual).id, "memory1");
    }

    #[test]
    fn generation_validation_checks_real_chunks_vectors_and_index_content() {
        let tmp = tempfile::tempdir().expect("temporary generation");
        let config = Config::load(Path::new(env!("CARGO_MANIFEST_DIR")).join("../config.toml"))
            .expect("load config");
        let record = Record {
            op_id: "op1".into(),
            id: "memory1".into(),
            content: "synthetic fact".into(),
            embedding: vec![1.0; 384],
        };
        let stored = chunk(&record).expect("chunk");
        {
            let store = RocksDbStore::open(tmp.path().join("rocks"), &config.storage.rocksdb)
                .expect("open rocks");
            let mut index = TantivyIndex::open(tmp.path().join("index"), &config.storage.tantivy)
                .expect("open index");
            store.store_chunk(&stored).expect("store chunk");
            store
                .store_embedding(&stored.id, record.embedding.clone())
                .expect("store vector");
            index
                .upsert_chunk_document(document(&stored), &stored)
                .expect("index chunk");
            index.commit().expect("commit index");
        }
        validate_generation(tmp.path(), &config, std::slice::from_ref(&record))
            .expect("complete generation");
        let wrong_content = Record {
            content: "different".into(),
            ..record
        };
        assert!(
            validate_generation(tmp.path(), &config, std::slice::from_ref(&wrong_content)).is_err()
        );
    }
}
