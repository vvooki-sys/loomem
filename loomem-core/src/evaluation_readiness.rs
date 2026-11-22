//! Evidence for a caller-owned fresh, immutable evaluation database only.
//! Caller must pin the model/config, capture expectations at successful writes,
//! and exclude concurrent mutations. Vector shape does not prove model provenance.
use crate::storage::RocksDbStore;
use crate::tantivy_index::readiness::{DocumentReadiness, ExpectedDocument};
use crate::tantivy_index::TantivyIndex;
use anyhow::{ensure, Result};

#[derive(Debug, PartialEq, Eq)]
pub enum ChunkReadiness {
    Active,
    Unavailable,
    ContentMismatch,
    MetadataMismatch,
}
#[derive(Debug, PartialEq, Eq)]
pub enum VectorReadiness {
    Valid,
    Missing,
    Invalid,
    Unavailable,
}
#[derive(Debug)]
pub struct RecordReadiness {
    pub id: String,
    pub chunk: ChunkReadiness,
    pub vector: VectorReadiness,
    pub document: DocumentReadiness,
}
#[derive(Debug)]
pub struct ReadinessReport {
    pub ready: bool,
    pub records: Vec<RecordReadiness>,
}

/// Observe exactly the receipt-provided set, without writes, commits or reloads.
/// Index observations share one snapshot of the index's existing search reader.
/// This is not a general concurrent-production readiness guarantee.
pub fn probe_expected_records(
    store: &RocksDbStore,
    index: &TantivyIndex,
    expected: &[ExpectedDocument<'_>],
    dimension: usize,
) -> Result<ReadinessReport> {
    probe_expected_records_with_metadata(store, index, expected, dimension, |_, _| true)
}

/// Same observation, additionally checking independently reconstructed provenance.
/// The callback must be pure; it cannot change the underlying records.
pub fn probe_expected_records_with_metadata(
    store: &RocksDbStore,
    index: &TantivyIndex,
    expected: &[ExpectedDocument<'_>],
    dimension: usize,
    metadata_matches: impl Fn(&ExpectedDocument<'_>, &crate::storage::Chunk) -> bool,
) -> Result<ReadinessReport> {
    ensure!(!expected.is_empty(), "empty expected records");
    ensure!(dimension > 0, "invalid embedding dimension");
    let documents = index.probe_expected_documents(expected)?;
    let mut records = Vec::with_capacity(expected.len());
    for (item, document) in expected.iter().zip(documents) {
        let chunk = match store.get_chunk_scoped(item.id, item.stream)? {
            Some(chunk) if chunk.deleted_at.is_none() => {
                if !metadata_matches(item, &chunk) {
                    ChunkReadiness::MetadataMismatch
                } else if chunk.content == item.content {
                    ChunkReadiness::Active
                } else {
                    ChunkReadiness::ContentMismatch
                }
            }
            _ => ChunkReadiness::Unavailable,
        };
        let vector = if chunk == ChunkReadiness::Active {
            match store.get_embedding(item.id)? {
                None => VectorReadiness::Missing,
                Some(v) if v.len() == dimension && v.iter().all(|x| x.is_finite()) => {
                    VectorReadiness::Valid
                }
                Some(_) => VectorReadiness::Invalid,
            }
        } else {
            VectorReadiness::Unavailable
        };
        records.push(RecordReadiness {
            id: item.id.into(),
            chunk,
            vector,
            document,
        });
    }
    let ready = records.iter().all(|r| {
        r.chunk == ChunkReadiness::Active
            && r.vector == VectorReadiness::Valid
            && r.document == DocumentReadiness::Current
    });
    Ok(ReadinessReport { ready, records })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RocksDbConfig;
    use crate::storage::Chunk;
    use crate::tantivy_index::{TantivyConfig, TextDocument};

    fn chunk() -> Chunk {
        serde_json::from_value(serde_json::json!({
            "id":"a", "content":"synthetic", "stream":"test", "level":0,
            "score":1.0,"timestamp":1,"consolidated":false,"dormant":false,"in_progress":false
        }))
        .unwrap()
    }
    fn doc(content: &str) -> TextDocument {
        TextDocument {
            id: "a".into(),
            content: content.into(),
            user_id: "".into(),
            app_id: "".into(),
            level: 0,
            timestamp: 1,
            stream: "test".into(),
            entities: None,
            relations: None,
            event_date: None,
            source_agent: None,
        }
    }
    fn fixture() -> (tempfile::TempDir, RocksDbStore, TantivyIndex) {
        let dir = tempfile::tempdir().unwrap();
        let store = RocksDbStore::open(
            dir.path().join("db"),
            &RocksDbConfig {
                max_open_files: 100,
                compression: "lz4".into(),
                write_buffer_size: 4 * 1024 * 1024,
                max_write_buffer_number: 2,
            },
        )
        .unwrap();
        let index = TantivyIndex::open(
            dir.path().join("index"),
            &TantivyConfig {
                enabled: true,
                heap_size_mb: 16,
                drift_warn_pct: 5.0,
                auto_rebuild_on_drift: false,
            },
        )
        .unwrap();
        store.store_chunk(&chunk()).unwrap();
        (dir, store, index)
    }
    fn probe(store: &RocksDbStore, index: &TantivyIndex) -> ReadinessReport {
        probe_expected_records(
            store,
            index,
            &[ExpectedDocument {
                id: "a",
                stream: "test",
                content: "synthetic",
            }],
            384,
        )
        .unwrap()
    }
    #[test]
    fn visibility_and_duplicate_documents() {
        let (_dir, store, mut index) = fixture();
        store.store_embedding("a", vec![0.1; 384]).unwrap();
        index.index_document(doc("synthetic")).unwrap();
        assert!(!probe(&store, &index).ready);
        index.commit().unwrap();
        assert!(probe(&store, &index).ready);
        index.index_document(doc("synthetic")).unwrap();
        index.commit().unwrap();
        assert_eq!(
            probe(&store, &index).records[0].document,
            DocumentReadiness::Duplicate
        );
    }
    #[test]
    fn orphan_and_invalid_vectors_cannot_satisfy_expectations() {
        let (_dir, store, mut index) = fixture();
        index.index_document(doc("synthetic")).unwrap();
        index.commit().unwrap();
        store.store_embedding("orphan", vec![0.1; 384]).unwrap();
        assert_eq!(
            probe(&store, &index).records[0].vector,
            VectorReadiness::Missing
        );
        for vector in [
            vec![0.1; 383],
            vec![f32::NAN; 384],
            vec![f32::INFINITY; 384],
        ] {
            store.store_embedding("a", vector).unwrap();
            assert_eq!(
                probe(&store, &index).records[0].vector,
                VectorReadiness::Invalid
            );
        }
    }
    #[test]
    fn inactive_foreign_and_mismatched_storage_fail() {
        let (_dir, store, mut index) = fixture();
        index.index_document(doc("synthetic")).unwrap();
        index.commit().unwrap();
        for variant in 0..3 {
            let mut c = chunk();
            match variant {
                0 => c.deleted_at = Some(1),
                1 => c.stream = "foreign".into(),
                _ => c.content = "changed".into(),
            }
            store.store_chunk(&c).unwrap();
            store.store_embedding("a", vec![0.1; 384]).unwrap();
            assert!(!probe(&store, &index).ready);
        }
    }
    #[test]
    fn wrong_index_text_and_invalid_requests_fail() {
        let (_dir, store, mut index) = fixture();
        store.store_embedding("a", vec![0.1; 384]).unwrap();
        index.index_document(doc("wrong")).unwrap();
        index.commit().unwrap();
        assert_eq!(
            probe(&store, &index).records[0].document,
            DocumentReadiness::ContentMismatch
        );
        let expected = || ExpectedDocument {
            id: "a",
            stream: "test",
            content: "synthetic",
        };
        assert!(probe_expected_records(&store, &index, &[expected(), expected()], 384).is_err());
        assert!(probe_expected_records(&store, &index, &[], 384).is_err());
        assert!(probe_expected_records(&store, &index, &[expected()], 0).is_err());
    }
}
