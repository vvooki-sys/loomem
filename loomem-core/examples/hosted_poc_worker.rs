//! Synthetic, private-IPC experiment; Hosted is responsible for authorization,
//! process isolation, acceptance, publication, and recovery. No production use.
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
    fs,
    io::{self, BufRead, Write},
    path::{Path, PathBuf},
};
use tantivy::{
    collector::TopDocs,
    query::TermQuery,
    schema::{IndexRecordOption, Value as TantivyValue},
    TantivyDocument, Term,
};

#[derive(Deserialize)]
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
struct Worker {
    root: PathBuf,
    config: Config,
    model: LocalEmbedder,
}

fn safe_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
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
fn validate_generation(path: &Path, config: &Config, records: &[Record]) -> Result<()> {
    let store = RocksDbStore::open(path.join("rocks"), &config.storage.rocksdb)?;
    let original_index = TantivyIndex::open(path.join("index"), &config.storage.tantivy)?;
    if store.get_all_chunks()?.len() != records.len()
        || store.get_all_embeddings()?.len() != records.len()
        || usize::try_from(original_index.count()?)? != records.len()
    {
        bail!("generation count mismatch")
    }
    drop(original_index);
    let index = tantivy::Index::open_in_dir(path.join("index"))?;
    let schema = index.schema();
    let id_field = schema.get_field("id")?;
    let content_field = schema.get_field("content")?;
    let reader = index.reader()?;
    let searcher = reader.searcher();
    for record in records {
        let chunk = store.get_chunk(&record.id)?.context("missing chunk")?;
        if chunk.content != record.content
            || store.get_embedding(&record.id)?.as_ref() != Some(&record.embedding)
        {
            bail!("stored record mismatch")
        }
        let query = TermQuery::new(
            Term::from_field_text(id_field, &record.id),
            IndexRecordOption::Basic,
        );
        let docs = searcher.search(&query, &TopDocs::with_limit(2))?;
        if docs.len() != 1 {
            bail!("indexed record mismatch")
        }
        let doc: TantivyDocument = searcher.doc(docs[0].1)?;
        if doc.get_first(content_field).and_then(|v| v.as_str()) != Some(record.content.as_str()) {
            bail!("indexed content mismatch")
        }
    }
    Ok(())
}
impl Worker {
    fn path(&self, generation: &str) -> Result<PathBuf> {
        if !safe_name(generation) {
            bail!("invalid generation")
        }
        Ok(self.root.join(generation))
    }
    fn ready(&self, generation: &str) -> Result<PathBuf> {
        let p = self.path(generation)?;
        if !fs::symlink_metadata(&p)?.file_type().is_dir()
            || !fs::symlink_metadata(p.join("READY"))?.file_type().is_file()
        {
            bail!("generation unavailable")
        }
        Ok(p)
    }
    fn build(
        &self,
        generation: &str,
        records: &[Record],
        failpoint: Option<&str>,
    ) -> Result<Value> {
        let p = self.path(generation)?;
        let mut ids = std::collections::HashSet::new();
        let mut operations = std::collections::HashSet::new();
        for record in records {
            if !ids.insert(&record.id) || !operations.insert(&record.op_id) {
                bail!("duplicate record");
            }
            if !safe_name(&record.id)
                || !safe_name(&record.op_id)
                || record.embedding.len() != 384
                || record.embedding.iter().any(|v| !v.is_finite())
            {
                bail!("invalid record")
            }
        }
        fs::create_dir(&p).context("generation already exists or cannot be created")?;
        {
            let store = RocksDbStore::open(p.join("rocks"), &self.config.storage.rocksdb)?;
            let mut index = TantivyIndex::open(p.join("index"), &self.config.storage.tantivy)?;
            for record in records {
                let c = chunk(record)?;
                store.store_chunk(&c)?;
                store.store_embedding(&c.id, record.embedding.clone())?;
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
                index.upsert_chunk_document(document(&c), &c)?;
            }
            index.commit()?;
        }
        // Validate every stored chunk, vector and indexed document after reopening.
        validate_generation(&p, &self.config, records)?;
        let marker = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(p.join("READY"))?;
        marker.sync_all()?;
        fs::File::open(&p)?.sync_all()?;
        Ok(json!({"ok":true,"generation":generation,"count":records.len()}))
    }
    fn search(&self, generation: &str, query: &str, embedding: &[f32]) -> Result<Value> {
        if embedding.len() != 384 || embedding.iter().any(|v| !v.is_finite()) {
            bail!("invalid query embedding")
        }
        let p = self.ready(generation)?;
        let store = RocksDbStore::open(p.join("rocks"), &self.config.storage.rocksdb)?;
        let index = TantivyIndex::open(p.join("index"), &self.config.storage.tantivy)?;
        let embeddings = store.get_all_embeddings()?;
        let lexical = index.search(query, 20)?;
        let hybrid = HybridSearchEngine::new(self.config.clone()).search_with_vector(
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
            .map(|r| Score {
                id: r.id,
                score: r.score,
            })
            .collect();
        Ok(json!({"ok":true,"generation":generation,"vector":vector,"bm25":bm25,"hybrid":hybrid}))
    }
    fn read(&self, generation: &str, id: &str) -> Result<Value> {
        if !safe_name(id) {
            bail!("invalid id")
        }
        let p = self.ready(generation)?;
        let store = RocksDbStore::open(p.join("rocks"), &self.config.storage.rocksdb)?;
        Ok(json!({"ok":true,"generation":generation,"chunk":store.get_chunk(id)?}))
    }
    fn execute(&self, req: Value) -> Result<Value> {
        let s = |k: &str| {
            req.get(k)
                .and_then(Value::as_str)
                .context("missing string field")
        };
        match s("cmd")? {
            "prepare" => {
                let op_id = s("op_id")?;
                let generation = s("generation")?;
                if !safe_name(op_id) || !safe_name(generation) {
                    bail!("invalid operation")
                }
                let embedding = self.model.embed(s("content")?)?;
                Ok(json!({"ok":true,"op_id":op_id,"generation":generation,"embedding":embedding}))
            }
            "build" => {
                let records: Vec<Record> =
                    serde_json::from_value(req.get("records").context("missing records")?.clone())?;
                self.build(
                    s("generation")?,
                    &records,
                    req.get("failpoint").and_then(Value::as_str),
                )
            }
            "search" => {
                let embedding: Vec<f32> = serde_json::from_value(
                    req.get("embedding").context("missing embedding")?.clone(),
                )?;
                self.search(s("generation")?, s("query")?, &embedding)
            }
            "read" => self.read(s("generation")?, s("id")?),
            _ => bail!("unknown command"),
        }
    }
}
fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 4 {
        bail!("usage: hosted_poc_worker ROOT CONFIG MODEL_DIR")
    }
    let root = Path::new(&args[1]);
    if !fs::symlink_metadata(root)?.file_type().is_dir() {
        bail!("root must be a directory")
    }
    let worker = Worker {
        root: root.canonicalize()?,
        config: Config::load(&args[2])?,
        model: LocalEmbedder::load(Path::new(&args[3]), 384)?,
    };
    let stdin = io::stdin();
    let mut out = io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line?;
        let request: Result<Value> = serde_json::from_str(&line).context("invalid JSON request");
        if request
            .as_ref()
            .ok()
            .and_then(|v| v.get("cmd"))
            .and_then(Value::as_str)
            == Some("shutdown")
        {
            writeln!(out, "{{\"ok\":true}}")?;
            out.flush()?;
            break;
        }
        let response = request
            .and_then(|r| worker.execute(r))
            .unwrap_or_else(|_| json!({"ok":false,"error":"operation failed"}));
        writeln!(out, "{}", serde_json::to_string(&response)?)?;
        out.flush()?;
    }
    Ok(())
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
        let cfg_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../config.toml");
        let config = Config::load(cfg_path).expect("load config");
        let record = Record {
            op_id: "op1".into(),
            id: "memory1".into(),
            content: "synthetic fact".into(),
            embedding: vec![1.0; 384],
        };
        let c = chunk(&record).expect("chunk");
        {
            let store = RocksDbStore::open(tmp.path().join("rocks"), &config.storage.rocksdb)
                .expect("open rocks");
            let mut index = TantivyIndex::open(tmp.path().join("index"), &config.storage.tantivy)
                .expect("open index");
            store.store_chunk(&c).expect("store chunk");
            store
                .store_embedding(&c.id, record.embedding.clone())
                .expect("store vector");
            index
                .upsert_chunk_document(document(&c), &c)
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
        let wrong_vector = Record {
            embedding: vec![0.5; 384],
            content: "synthetic fact".into(),
            ..wrong_content
        };
        assert!(
            validate_generation(tmp.path(), &config, std::slice::from_ref(&wrong_vector)).is_err()
        );
        {
            let mut index = TantivyIndex::open(tmp.path().join("index"), &config.storage.tantivy)
                .expect("open index");
            let mut wrong_document = document(&c);
            wrong_document.content = "different indexed content".into();
            index
                .upsert_chunk_document(wrong_document, &c)
                .expect("replace doc");
            index.commit().expect("commit index");
        }
        let proper = Record {
            embedding: vec![1.0; 384],
            ..wrong_vector
        };
        assert!(validate_generation(tmp.path(), &config, std::slice::from_ref(&proper)).is_err());
        {
            let mut index = TantivyIndex::open(tmp.path().join("index"), &config.storage.tantivy)
                .expect("open index");
            index.delete_document("memory1").expect("delete doc");
            index.commit().expect("commit index");
        }
        assert!(validate_generation(tmp.path(), &config, &[proper]).is_err());
    }
}
