use super::*;
use crate::{config::RocksDbConfig, source_tag::SourceTag};
use std::sync::{Arc, Barrier};

fn setup() -> (tempfile::TempDir, Arc<RocksDbStore>) {
    let temp = tempfile::tempdir().unwrap();
    let store = RocksDbStore::open(
        temp.path(),
        &RocksDbConfig {
            max_open_files: 100,
            compression: "lz4".into(),
            write_buffer_size: 4 * 1024 * 1024,
            max_write_buffer_number: 2,
        },
    )
    .unwrap();
    (temp, Arc::new(store))
}
fn chunk(id: &str) -> Chunk {
    serde_json::from_value(serde_json::json!({
        "id":id,"content":"synthetic","stream":"test","level":0,
        "score":1.0,"timestamp":1,"consolidated":false,"dormant":false,"in_progress":false
    }))
    .unwrap()
}
fn source() -> Chunk {
    let mut c = chunk(&uuid::Uuid::new_v4().to_string());
    c.provenance_role = ProvenanceRole::Evidence;
    c.persistent = true;
    c.consolidated = true;
    c.trust_level = Some("a2".into());
    c.source = Some(SourceTag::from_agent("mcp-ingest-source"));
    c.metadata = Some(serde_json::json!({"_loomem_ingest":{"version":1,"kind":"source"}}));
    c
}
fn fact(parent: &Chunk) -> Chunk {
    let mut c = chunk(&uuid::Uuid::new_v4().to_string());
    c.source = Some(SourceTag::from_agent("mcp-ingest"));
    c.trust_level = Some("a2".into());
    c.metadata = Some(serde_json::json!({"_loomem_ingest":{"version":1,"kind":"fact"}}));
    c.source_ids = Some(vec![parent.id.clone()]);
    c.extraction_meta=Some(serde_json::from_value(serde_json::json!({"fact_type":"fact","subject":null,"event_date":null,"event_date_context":null,"supersedes":null,"superseded_by":null,"confidence":1.0,"extracted_from":parent.id,"extraction_model":null})).unwrap());
    c
}
#[test]
fn admission_checks_sources_stream_alive_and_matching_metadata() {
    let (_temp, store) = setup();
    let root = source();
    let f = fact(&root);
    assert!(store.store_chunk(&f).is_err());
    store.store_chunk(&root).unwrap();
    let mut bad = f.clone();
    bad.stream = "other".into();
    assert!(store.store_chunk(&bad).is_err());
    bad = f.clone();
    bad.source_ids = Some(vec![uuid::Uuid::new_v4().to_string()]);
    assert!(store.store_chunk(&bad).is_err());
    store.store_chunk(&f).unwrap();
    store.tombstone_ingest_source(&root.id, 42).unwrap();
    assert!(store.store_chunk(&fact(&root)).is_err());
    assert_eq!(
        store.get_chunk(&f.id).unwrap().unwrap().deleted_at,
        Some(42)
    );
}
#[test]
fn immutable_owned_content_provenance_and_resurrection() {
    let (_temp, store) = setup();
    let root = source();
    store.store_chunk(&root).unwrap();
    for field in ["content", "stream", "metadata", "role", "source"] {
        let mut c = root.clone();
        match field {
            "content" => c.content = "changed".into(),
            "stream" => c.stream = "other".into(),
            "metadata" => c.metadata = None,
            "role" => c.provenance_role = ProvenanceRole::Claim,
            _ => c.source = None,
        }
        assert!(store.store_chunk(&c).is_err());
    }
    let mut changed = root.clone();
    changed.score = 0.7;
    store.store_chunk(&changed).unwrap();
    store.tombstone_ingest_source(&root.id, 7).unwrap();
    assert!(store.store_chunk(&root).is_err());
}
#[test]
fn descendants_cycles_shared_parents_cross_stream_and_retry() {
    let (_temp, store) = setup();
    let root = source();
    let other = source();
    store.store_chunk(&root).unwrap();
    store.store_chunk(&other).unwrap();
    let first = fact(&root);
    store.store_chunk(&first).unwrap();
    let mut second = chunk("legacy-child");
    second.source_ids = Some(vec![first.id.clone(), other.id.clone()]);
    store.store_chunk(&second).unwrap();
    let mut cycle = chunk("cycle");
    cycle.source_ids = Some(vec![second.id.clone()]);
    store.store_chunk(&cycle).unwrap();
    second.extraction_meta = None;
    second.source_ids.as_mut().unwrap().push(cycle.id.clone());
    store
        .db
        .put(
            format!("chunk:L0:{}", second.id),
            store.encode_chunk(&second).unwrap(),
        )
        .unwrap();
    let mut foreign = chunk("foreign");
    foreign.stream = "other".into();
    foreign.source_ids = Some(vec![first.id.clone()]);
    store
        .db
        .put(
            format!("chunk:L0:{}", foreign.id),
            store.encode_chunk(&foreign).unwrap(),
        )
        .unwrap();
    let ids = store.tombstone_ingest_source(&root.id, 9).unwrap().unwrap();
    assert_eq!(ids.len(), 4);
    assert!(store
        .get_chunk(&other.id)
        .unwrap()
        .unwrap()
        .deleted_at
        .is_none());
    assert!(store
        .get_chunk("foreign")
        .unwrap()
        .unwrap()
        .deleted_at
        .is_none());
    assert_eq!(
        ids,
        store
            .tombstone_ingest_source(&root.id, 10)
            .unwrap()
            .unwrap()
    );
    assert_eq!(
        store.get_chunk("cycle").unwrap().unwrap().deleted_at,
        Some(9)
    );
}
#[test]
fn corrupt_scan_aborts_entire_tombstone_batch() {
    let (_temp, store) = setup();
    let root = source();
    store.store_chunk(&root).unwrap();
    store.db.put(b"chunk:L0:corrupt", b"bad").unwrap();
    assert!(store.tombstone_ingest_source(&root.id, 5).is_err());
    assert!(store
        .get_chunk(&root.id)
        .unwrap()
        .unwrap()
        .deleted_at
        .is_none());
}
#[test]
fn delayed_writer_and_delete_are_serialized_without_sleep() {
    let (_temp, store) = setup();
    let root = source();
    store.store_chunk(&root).unwrap();
    let f = fact(&root);
    let gate = Arc::new(Barrier::new(2));
    let worker = Arc::clone(&store);
    let ready = Arc::clone(&gate);
    let thread = std::thread::spawn(move || {
        ready.wait();
        worker.store_chunk(&f)
    });
    store.tombstone_ingest_source(&root.id, 11).unwrap();
    gate.wait();
    assert!(thread.join().unwrap().is_err());
}
#[test]
fn legacy_claim_cannot_opt_into_source_with_marker_alone() {
    let (_temp, store) = setup();
    let mut legacy = chunk("legacy");
    legacy.metadata = source().metadata;
    assert!(!is_ingest_source(&legacy));
    store.store_chunk(&legacy).unwrap();
    assert!(store
        .tombstone_ingest_source(&legacy.id, 2)
        .unwrap()
        .is_none());
}

#[test]
fn delayed_unmarked_descendant_cannot_cross_tombstoned_ancestry() {
    let (_temp, store) = setup();
    let root = source();
    store.store_chunk(&root).unwrap();
    let f = fact(&root);
    store.store_chunk(&f).unwrap();
    let mut child = chunk("derived");
    child.source_ids = Some(vec![f.id.clone()]);
    store.store_chunk(&child).unwrap();
    let mut forged = child.clone();
    forged.source_ids = None;
    assert!(store.store_chunk(&forged).is_err());
    store.tombstone_ingest_source(&root.id, 15).unwrap();
    let mut late = chunk("late-derived");
    late.source_ids = Some(vec![child.id.clone()]);
    assert!(store.store_chunk(&late).is_err());
    let mut legacy = chunk("legacy-dangling");
    legacy.source_ids = Some(vec!["unknown-legacy".into()]);
    assert!(store.store_chunk(&legacy).is_err());
}

#[test]
fn hard_purged_parent_blocks_new_writes_but_legacy_reads_survive() {
    let (_temp, store) = setup();
    let root = source();
    store.store_chunk(&root).unwrap();
    let f = fact(&root);
    store.hard_delete_by_id(&root.id).unwrap();
    assert!(store.store_chunk(&f).is_err());
    let mut generic = chunk("new-derived");
    generic.source_ids = Some(vec![root.id]);
    assert!(store.store_chunk(&generic).is_err());
    let mut old = chunk("old-legacy");
    old.source_ids = Some(vec!["missing-legacy".into()]);
    store
        .db
        .put(
            format!("chunk:L0:{}", old.id),
            store.encode_chunk(&old).unwrap(),
        )
        .unwrap();
    old.score = 0.3;
    store.store_chunk(&old).unwrap();
    assert!(store.get_chunk(&old.id).unwrap().is_some());
    old.deleted_at = Some(8);
    store
        .db
        .put(
            format!("chunk:L0:{}", old.id),
            store.encode_chunk(&old).unwrap(),
        )
        .unwrap();
    old.deleted_at = None;
    assert!(store.store_chunk(&old).is_err());
}

#[test]
fn fact_display_shape_requires_canonical_links_and_owned_authority() {
    let parent = source();
    let c = fact(&parent);
    assert!(is_ingest_fact_shape(&c));
    let mut forged = c.clone();
    forged.source_ids = None;
    assert!(!is_ingest_fact_shape(&forged));
    forged = c.clone();
    forged.trust_level = None;
    assert!(!is_ingest_fact_shape(&forged));
    forged = c.clone();
    forged.extraction_meta.as_mut().unwrap().extracted_from =
        Some(uuid::Uuid::new_v4().to_string());
    assert!(!is_ingest_fact_shape(&forged));
}

#[test]
fn racing_admission_is_either_included_or_rejected() {
    let (_temp, store) = setup();
    let root = source();
    store.store_chunk(&root).unwrap();
    let child = fact(&root);
    let child_id = child.id.clone();
    let gate = Arc::new(Barrier::new(3));
    let writer_store = Arc::clone(&store);
    let writer_gate = Arc::clone(&gate);
    let writer = std::thread::spawn(move || {
        writer_gate.wait();
        writer_store.store_chunk(&child)
    });
    let delete_store = Arc::clone(&store);
    let delete_gate = Arc::clone(&gate);
    let deleter = std::thread::spawn(move || {
        delete_gate.wait();
        delete_store.tombstone_ingest_source(&root.id, 99).unwrap()
    });
    gate.wait();
    let admitted = writer.join().unwrap().is_ok();
    let ids = deleter.join().unwrap().unwrap();
    let child = store.get_chunk(&child_id).unwrap();
    if admitted {
        assert!(ids.contains(&child_id));
        assert_eq!(child.unwrap().deleted_at, Some(99));
    } else {
        assert!(child.is_none());
    }
}

#[test]
fn fact_provenance_updates_reject_and_family_delete_invalidates_embedding() {
    let (_temp, store) = setup();
    let root = source();
    store.store_chunk(&root).unwrap();
    let child = fact(&root);
    store.store_chunk(&child).unwrap();
    let target = store
        .embedding_target(&child.id, &child.content)
        .unwrap()
        .unwrap();
    let mut changed = child.clone();
    changed.content = "changed fact".into();
    assert!(store.store_chunk(&changed).is_err());
    changed = child.clone();
    changed.metadata = None;
    assert!(store.store_chunk(&changed).is_err());
    store.tombstone_ingest_source(&root.id, 55).unwrap();
    assert!(!store
        .store_embedding_if_current(&target, vec![1.0])
        .unwrap());
    store.hard_delete_by_id(&root.id).unwrap();
    assert!(store.store_chunk(&child).is_err());
}

#[test]
fn legacy_self_audit_reference_and_composite_audit_string_are_compatible() {
    let (_temp, store) = setup();
    let mut legacy = chunk("self-audit");
    legacy.extraction_meta = fact(&source()).extraction_meta;
    legacy.extraction_meta.as_mut().unwrap().extracted_from = Some(legacy.id.clone());
    store.store_chunk(&legacy).unwrap();
    let second = chunk("second-parent");
    store.store_chunk(&second).unwrap();
    let mut merged = chunk("merged-legacy");
    merged.source_ids = Some(vec![legacy.id.clone(), second.id.clone()]);
    merged.extraction_meta = legacy.extraction_meta.clone();
    merged.extraction_meta.as_mut().unwrap().extracted_from =
        Some(format!("{},{}", legacy.id, second.id));
    store.store_chunk(&merged).unwrap();
    assert_eq!(
        parents(&merged).collect::<Vec<_>>(),
        vec![legacy.id.as_str(), second.id.as_str()]
    );
    let mut marked = fact(&source());
    marked.extraction_meta = merged.extraction_meta;
    assert!(!is_ingest_fact_shape(&marked));
    assert!(store.store_chunk(&marked).is_err());
}

#[test]
fn explicit_other_parent_cannot_hide_singular_extracted_owner() {
    let (_temp, store) = setup();
    let root = source();
    let other = source();
    store.store_chunk(&root).unwrap();
    store.store_chunk(&other).unwrap();
    let mut child = chunk("singular-owner-child");
    child.source_ids = Some(vec![other.id.clone()]);
    child.extraction_meta = fact(&root).extraction_meta;
    store.store_chunk(&child).unwrap();
    let ids = store
        .tombstone_ingest_source(&root.id, 21)
        .unwrap()
        .unwrap();
    assert!(ids.contains(&child.id));
    let mut late = chunk("late-singular-child");
    late.source_ids = Some(vec![other.id.clone()]);
    late.extraction_meta = fact(&root).extraction_meta;
    assert!(store.store_chunk(&late).is_err());
}
