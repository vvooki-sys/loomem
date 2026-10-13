use super::*;
use crate::storage::RocksDbConfig;
use std::sync::{Arc, Barrier};

fn store() -> (tempfile::TempDir, Arc<RocksDbStore>) {
    let dir = tempfile::tempdir().unwrap();
    let store = RocksDbStore::open(
        dir.path(),
        &RocksDbConfig {
            max_open_files: 100,
            compression: "lz4".into(),
            write_buffer_size: 4 * 1024 * 1024,
            max_write_buffer_number: 2,
        },
    )
    .unwrap();
    (dir, Arc::new(store))
}

fn chunk(id: &str) -> Chunk {
    serde_json::from_value(serde_json::json!({
        "id": id, "content": "synthetic fact", "stream": "test", "level": 0,
        "score": 1.0, "timestamp": 1, "consolidated": false,
        "dormant": false, "in_progress": false
    }))
    .unwrap()
}

#[test]
fn current_target_publishes_and_metadata_updates_preserve_it() {
    let (_dir, store) = store();
    let mut chunk = chunk("one");
    store.store_chunk(&chunk).unwrap();
    let target = store
        .embedding_target(&chunk.id, &chunk.content)
        .unwrap()
        .unwrap();
    chunk.score = 0.5;
    store.store_chunk(&chunk).unwrap();
    assert!(store
        .store_embedding_if_current(&target, vec![1.0])
        .unwrap());
    assert_eq!(store.get_embedding(&chunk.id).unwrap(), Some(vec![1.0]));
    assert!(store
        .embedding_target(&chunk.id, "outdated")
        .unwrap()
        .is_none());
    assert!(store
        .embedding_target("absent", "synthetic fact")
        .unwrap()
        .is_none());
}

#[test]
fn delayed_publication_cannot_cross_delete_purge_update_or_aba() {
    for operation in ["soft", "hard", "purge", "update", "version", "aba"] {
        let (_dir, store) = store();
        let mut chunk = chunk("one");
        store.store_chunk(&chunk).unwrap();
        let target = store
            .embedding_target(&chunk.id, &chunk.content)
            .unwrap()
            .unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let worker_store = Arc::clone(&store);
        let worker_barrier = Arc::clone(&barrier);
        let writer = std::thread::spawn(move || {
            worker_barrier.wait();
            worker_store
                .store_embedding_if_current(&target, vec![1.0])
                .unwrap()
        });
        match operation {
            "soft" => {
                store.delete_by_id(&chunk.id).unwrap();
            }
            "hard" => {
                store.hard_delete_by_id(&chunk.id).unwrap();
            }
            "purge" => {
                store.purge_namespace("test", false).unwrap();
            }
            "update" => {
                chunk.content = "updated fact".into();
                store.store_chunk(&chunk).unwrap();
            }
            "version" => {
                chunk.version += 1;
                store.store_chunk(&chunk).unwrap();
            }
            "aba" => {
                store.hard_delete_by_id(&chunk.id).unwrap();
                store.store_chunk(&chunk).unwrap();
            }
            _ => unreachable!(),
        }
        barrier.wait();
        assert!(!writer.join().unwrap(), "{operation}");
        assert!(
            store.get_embedding(&chunk.id).unwrap().is_none(),
            "{operation}"
        );
    }
}

#[test]
fn publication_before_delete_is_removed_and_updates_invalidate_vectors() {
    let (_dir, store) = store();
    let mut chunk = chunk("one");
    store.store_chunk(&chunk).unwrap();
    let target = store
        .embedding_target(&chunk.id, &chunk.content)
        .unwrap()
        .unwrap();
    assert!(store
        .store_embedding_if_current(&target, vec![1.0])
        .unwrap());
    chunk.content = "updated fact".into();
    store.store_chunk(&chunk).unwrap();
    assert!(store.get_embedding(&chunk.id).unwrap().is_none());
    let target = store
        .embedding_target(&chunk.id, &chunk.content)
        .unwrap()
        .unwrap();
    assert!(store
        .store_embedding_if_current(&target, vec![2.0])
        .unwrap());
    store.hard_delete_by_id(&chunk.id).unwrap();
    assert!(store.get_embedding(&chunk.id).unwrap().is_none());
}

#[test]
fn purge_is_scoped_dry_run_safe_and_cleans_derivatives() {
    let (_dir, store) = store();
    let chunk = chunk("one");
    let mut other = chunk.clone();
    other.id = "two".into();
    other.stream = "other".into();
    store.store_chunk(&chunk).unwrap();
    store.store_chunk(&other).unwrap();
    store.store_embedding(&chunk.id, vec![1.0]).unwrap();
    store.put(b"entity:one", b"synthetic").unwrap();
    store.put(b"rel:one", b"synthetic").unwrap();
    assert_eq!(store.purge_namespace("test", true).unwrap(), vec!["one"]);
    assert!(store.get_chunk("one").unwrap().is_some());
    assert_eq!(store.purge_namespace("test", false).unwrap(), vec!["one"]);
    assert!(store.get_embedding("one").unwrap().is_none());
    assert!(store.get(b"entity:one").unwrap().is_none());
    assert!(store.get(b"rel:one").unwrap().is_none());
    assert!(store.get_chunk("two").unwrap().is_some());
    assert!(store.purge_namespace("test", false).unwrap().is_empty());
}

#[test]
fn purge_decode_failure_does_not_partially_delete_valid_rows() {
    let (_dir, store) = store();
    store.store_chunk(&chunk("one")).unwrap();
    store.put(b"chunk:L0:broken", b"invalid envelope").unwrap();
    assert!(store.purge_namespace("test", false).is_err());
    assert!(store.get_chunk("one").unwrap().is_some());
}

#[test]
fn legacy_row_target_is_invalidated_by_identical_delete_recreate() {
    let (_dir, store) = store();
    let chunk = chunk("legacy");
    store
        .put(b"chunk:L0:legacy", &serde_json::to_vec(&chunk).unwrap())
        .unwrap();
    let target = store
        .embedding_target(&chunk.id, &chunk.content)
        .unwrap()
        .unwrap();
    store.hard_delete_by_id(&chunk.id).unwrap();
    store.store_chunk(&chunk).unwrap();
    assert!(!store
        .store_embedding_if_current(&target, vec![1.0])
        .unwrap());
}
