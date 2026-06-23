use super::*;
use crate::config::RocksDbConfig;
use crate::crypto::provider::MasterKeyEnvProvider;
use crate::graph::{EntityNode, GraphStore};
use crate::storage::{Chunk, RocksDbStore};
use std::sync::Arc;
use tempfile::TempDir;

// ── Test helpers ─────────────────────────────────────────────────────────────

fn db_cfg() -> RocksDbConfig {
    RocksDbConfig {
        max_open_files: 100,
        compression: "none".to_string(),
        write_buffer_size: 4 * 1024 * 1024,
        max_write_buffer_number: 2,
    }
}

fn open_noop(tmp: &TempDir) -> RocksDbStore {
    RocksDbStore::open(tmp.path(), &db_cfg()).expect("open noop")
}

fn open_encrypted(tmp: &TempDir) -> Arc<RocksDbStore> {
    let store = RocksDbStore::open(tmp.path(), &db_cfg()).expect("open encrypted");
    let provider = Arc::new(MasterKeyEnvProvider::new([42u8; 32], store.db_arc()));
    Arc::new(store.with_encryption_provider(provider))
}

/// Write a bare `EntityNode` JSON directly to `graph:entity:{id}` bypassing
/// `store_entity`, so the row has `stream_id == ""` (legacy format).
fn put_legacy_entity(store: &RocksDbStore, entity: &EntityNode) {
    let key = format!("graph:entity:{}", entity.id);
    let val = serde_json::to_vec(entity).expect("serialize entity");
    store.put(key.as_bytes(), &val).expect("put legacy entity");
}

fn make_chunk(id: &str, stream: &str) -> Chunk {
    Chunk {
        id: id.to_string(),
        content: format!("content-{id}"),
        stream: stream.to_string(),
        level: 0,
        score: 1.0,
        timestamp: 1_000_000,
        consolidated: false,
        dormant: false,
        in_progress: false,
        prompt_version: None,
        source_ids: None,
        last_decay: None,
        metadata: None,
        importance: None,
        persistent: false,
        last_implicit_boost: None,
        access_count: 0,
        source: None,
        created_by: None,
        updated_at: None,
        valid_from: None,
        valid_until: None,
        is_latest: true,
        superseded_by: None,
        supersedes_id: None,
        root_memory_id: None,
        version: 1,
        memory_type: None,
        extraction_meta: None,
        deleted_at: None,
        trust_level: None,
        ingester_user_id: None,
        alpha: 1.0,
        beta: 1.0,
        harmful_count: 0,
        n_ratings: 0,
        last_rated_at: None,
        provenance_role: crate::storage::ProvenanceRole::Claim,
    }
}

fn legacy_entity(id: &str, name: &str, chunk_ids: Vec<String>) -> EntityNode {
    EntityNode {
        id: id.to_string(),
        canonical_name: name.to_string(),
        entity_type: "Person".to_string(),
        aliases: Vec::new(),
        chunk_ids,
        stream_id: String::new(), // legacy: empty
        created_at: 1000,
        updated_at: 1000,
    }
}

// ── T1: dry-run default — zero mutations ─────────────────────────────────────

/// T1: dry_run=true classifies correctly and leaves raw row bytes unchanged.
#[test]
fn t1_dry_run_zero_mutations() {
    let tmp = TempDir::new().expect("tempdir");

    // Write legacy entity and a live chunk in the same tempdir via noop store.
    // Scoped drop releases DB lock before reopening with encrypted provider.
    let raw_before: Vec<u8>;
    {
        let store = open_noop(&tmp);
        let entity = legacy_entity("ent-t1", "Alice", vec!["chunk-t1".to_string()]);
        put_legacy_entity(&store, &entity);
        store
            .store_chunk(&make_chunk("chunk-t1", "stream-a"))
            .expect("store chunk");

        // Capture raw bytes before any repair.
        let key = b"graph:entity:ent-t1";
        raw_before = store.get(key).expect("get").expect("present").to_vec();
    } // noop lock released.

    let store_enc = open_encrypted(&tmp);
    let graph = GraphStore::new(store_enc.clone());

    let report = repair_entity_streams(&store_enc, &graph, true).expect("dry run ok");

    assert!(report.dry_run);
    assert_eq!(report.repaired, 1, "classified as repaired");
    assert_eq!(report.scanned, 1);
    assert!(
        report.repaired_by_stream.is_empty(),
        "dry_run must not populate by_stream"
    );

    // Raw bytes must be byte-identical — no write happened.
    let raw_after = store_enc
        .get(b"graph:entity:ent-t1")
        .expect("get")
        .expect("present")
        .to_vec();
    assert_eq!(raw_before, raw_after, "T1: dry_run must not mutate the row");
}

// ── T2: happy path — encrypted + indexed ─────────────────────────────────────

/// T2: dry_run=false under MasterKeyEnvProvider: entity encrypted, stream
/// resolved, name index written so get_entity_by_name finds it.
#[test]
fn t2_happy_path_encrypted_and_indexed() {
    let tmp = TempDir::new().expect("tempdir");
    {
        let store = open_noop(&tmp);
        let entity = legacy_entity("ent-t2", "Bob", vec!["chunk-t2".to_string()]);
        put_legacy_entity(&store, &entity);
        store
            .store_chunk(&make_chunk("chunk-t2", "stream-b"))
            .expect("store chunk");
    }

    let store_enc = open_encrypted(&tmp);
    let graph = GraphStore::new(store_enc.clone());

    let report = repair_entity_streams(&store_enc, &graph, false).expect("repair ok");

    assert_eq!(report.repaired, 1, "T2: repaired==1");
    assert_eq!(report.scanned, 1);
    assert_eq!(report.already_scoped, 0);
    assert_eq!(report.repaired_by_stream.get("stream-b"), Some(&1u64));

    // Row now has non-empty encrypted_payload.
    let raw = store_enc
        .get(b"graph:entity:ent-t2")
        .expect("get")
        .expect("present");
    let stored: crate::graph::StoredEntityRead =
        serde_json::from_slice(&raw).expect("parse envelope");
    assert!(
        !stored.encrypted_payload.is_empty(),
        "T2: encrypted_payload must be present after repair"
    );
    assert_eq!(
        stored.entity.stream_id, "stream-b",
        "T2: stream_id set in envelope"
    );

    // decode_entity round-trips canonical_name.
    let decoded = graph.decode_entity(&raw).expect("decode ok");
    assert_eq!(
        decoded.canonical_name, "Bob",
        "T2: canonical_name survives round-trip"
    );

    // get_entity_by_name finds it in stream-b (index rows written).
    let found = graph
        .get_entity_by_name("Bob", "stream-b")
        .expect("lookup ok");
    assert!(
        found.is_some(),
        "T2: get_entity_by_name must find repaired entity"
    );
    assert_eq!(found.unwrap().id, "ent-t2");
}

// ── T3: conflicting chunk streams ─────────────────────────────────────────────

/// T3: entity with chunks in two different streams → conflicting_chunk_streams,
/// row untouched.
#[test]
fn t3_conflicting_chunk_streams() {
    let tmp = TempDir::new().expect("tempdir");
    {
        let store = open_noop(&tmp);
        let entity = legacy_entity(
            "ent-t3",
            "Charlie",
            vec!["chunk-t3a".to_string(), "chunk-t3b".to_string()],
        );
        put_legacy_entity(&store, &entity);
        store
            .store_chunk(&make_chunk("chunk-t3a", "stream-x"))
            .expect("chunk a");
        store
            .store_chunk(&make_chunk("chunk-t3b", "stream-y"))
            .expect("chunk b");

        // Capture raw bytes before any repair.
        let raw = store
            .get(b"graph:entity:ent-t3")
            .expect("get")
            .expect("present")
            .to_vec();
        drop(store);

        let store_enc = open_encrypted(&tmp);
        let graph = GraphStore::new(store_enc.clone());
        let report = repair_entity_streams(&store_enc, &graph, false).expect("repair ok");

        assert_eq!(report.conflicting_chunk_streams, 1, "T3: one conflict");
        assert_eq!(report.repaired, 0);

        // Row byte-identical.
        let raw_after = store_enc
            .get(b"graph:entity:ent-t3")
            .expect("get")
            .expect("present")
            .to_vec();
        assert_eq!(
            raw, raw_after,
            "T3: conflicting entity row must not be mutated"
        );
    }
}

// ── T4: unresolvable — no live chunks ─────────────────────────────────────────

/// T4: entity with no live chunks → unresolvable_no_chunks, row untouched.
#[test]
fn t4_unresolvable_no_live_chunks() {
    let tmp = TempDir::new().expect("tempdir");
    {
        let store = open_noop(&tmp);
        // chunk_ids points at a chunk that does not exist in storage.
        let entity = legacy_entity("ent-t4", "Dana", vec!["ghost-chunk".to_string()]);
        put_legacy_entity(&store, &entity);
        let raw = store
            .get(b"graph:entity:ent-t4")
            .expect("get")
            .expect("present")
            .to_vec();
        drop(store);

        let store_enc = open_encrypted(&tmp);
        let graph = GraphStore::new(store_enc.clone());
        let report = repair_entity_streams(&store_enc, &graph, false).expect("repair ok");

        assert_eq!(report.unresolvable_no_chunks, 1, "T4: unresolvable count");
        assert_eq!(report.repaired, 0);

        let raw_after = store_enc
            .get(b"graph:entity:ent-t4")
            .expect("get")
            .expect("present")
            .to_vec();
        assert_eq!(
            raw, raw_after,
            "T4: unresolvable entity row must not be mutated"
        );
    }
}

// ── T5: name conflict ─────────────────────────────────────────────────────────

/// T5: incumbent with same canonical name exists in stream → repaired_name_conflict.
/// The row is encrypted but get_entity_by_name still returns the incumbent.
#[test]
fn t5_name_conflict_incumbent_preserved() {
    let tmp = TempDir::new().expect("tempdir");
    {
        let store = open_noop(&tmp);
        let entity = legacy_entity("ent-t5-legacy", "Eve", vec!["chunk-t5".to_string()]);
        put_legacy_entity(&store, &entity);
        store
            .store_chunk(&make_chunk("chunk-t5", "stream-c"))
            .expect("chunk");
    }

    // Open encrypted, create incumbent "Eve" via get_or_create_entity.
    let store_enc = open_encrypted(&tmp);
    let graph = GraphStore::new(store_enc.clone());
    let incumbent = graph
        .get_or_create_entity("Eve", "Person", &[], "stream-c")
        .expect("create incumbent");

    let report = repair_entity_streams(&store_enc, &graph, false).expect("repair ok");

    // Legacy entity processed → name conflict (incumbent.id != ent-t5-legacy).
    assert_eq!(report.repaired_name_conflict, 1, "T5: name_conflict count");
    assert_eq!(report.repaired, 0);

    // get_entity_by_name still returns the incumbent.
    let found = graph
        .get_entity_by_name("Eve", "stream-c")
        .expect("lookup ok")
        .expect("must find");
    assert_eq!(
        found.id, incumbent.id,
        "T5: incumbent id must be preserved in name index"
    );
}

// ── T6: idempotence — second run skips already-scoped ────────────────────────

/// T6: after T2 repair, a second run increments already_scoped and repaired==0.
#[test]
fn t6_idempotent_second_run() {
    let tmp = TempDir::new().expect("tempdir");
    {
        let store = open_noop(&tmp);
        let entity = legacy_entity("ent-t6", "Frank", vec!["chunk-t6".to_string()]);
        put_legacy_entity(&store, &entity);
        store
            .store_chunk(&make_chunk("chunk-t6", "stream-d"))
            .expect("chunk");
    }

    let store_enc = open_encrypted(&tmp);
    let graph = GraphStore::new(store_enc.clone());

    // First run.
    let r1 = repair_entity_streams(&store_enc, &graph, false).expect("run 1");
    assert_eq!(r1.repaired, 1, "T6: first run must repair");

    // Second run.
    let r2 = repair_entity_streams(&store_enc, &graph, false).expect("run 2");
    assert_eq!(r2.repaired, 0, "T6: second run must not repair again");
    assert_eq!(r2.already_scoped, 1, "T6: already_scoped must increment");
}

// ── T7: noop provider rejected ───────────────────────────────────────────────

/// T7: NoopProvider → repair_entity_streams returns Err (HTTP handler maps → 400).
#[test]
fn t7_noop_provider_returns_err() {
    let tmp = TempDir::new().expect("tempdir");
    let store = Arc::new(open_noop(&tmp));
    let graph = GraphStore::new(store.clone());

    let result = repair_entity_streams(&store, &graph, true);
    assert!(result.is_err(), "T7: NoopProvider must return Err");
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("NoopProvider") || msg.contains("disabled"),
        "T7: error must mention disabled provider: {msg}"
    );
}

// ── T10: tombstoned chunks do not vote (critic MED-1) ────────────────────────

/// T10: a soft-deleted chunk's stream is excluded from resolution. Case A: a
/// tombstoned chunk in a different stream must NOT inflate to Conflicting.
/// Case B: an entity whose only chunk is tombstoned is unresolvable.
#[test]
fn t10_tombstoned_chunks_do_not_vote() {
    let tmp = TempDir::new().expect("tempdir");
    {
        let store = open_noop(&tmp);
        // Case A: live chunk in stream-a + tombstoned chunk in stream-b.
        put_legacy_entity(
            &store,
            &legacy_entity(
                "ent-t10a",
                "Tomb-A",
                vec!["c-t10-live".to_string(), "c-t10-dead".to_string()],
            ),
        );
        store
            .store_chunk(&make_chunk("c-t10-live", "stream-a"))
            .expect("store live chunk");
        let mut dead = make_chunk("c-t10-dead", "stream-b");
        dead.deleted_at = Some(2_000_000);
        store.store_chunk(&dead).expect("store tombstoned chunk");

        // Case B: only chunk is tombstoned.
        put_legacy_entity(
            &store,
            &legacy_entity("ent-t10b", "Tomb-B", vec!["c-t10-only-dead".to_string()]),
        );
        let mut only_dead = make_chunk("c-t10-only-dead", "stream-c");
        only_dead.deleted_at = Some(2_000_000);
        store
            .store_chunk(&only_dead)
            .expect("store tombstoned chunk");
    } // noop lock released.

    let store_enc = open_encrypted(&tmp);
    let graph = GraphStore::new(store_enc.clone());
    let report = repair_entity_streams(&store_enc, &graph, false).expect("repair ok");

    // Case A resolved to the LIVE stream only (no Conflicting from the tombstone).
    assert_eq!(report.repaired, 1, "case A repaired despite dead chunk");
    assert_eq!(report.conflicting_chunk_streams, 0);
    assert_eq!(report.repaired_by_stream.get("stream-a"), Some(&1));
    let raw = store_enc
        .get(b"graph:entity:ent-t10a")
        .expect("get")
        .expect("present");
    let staged: StoredEntityRead = serde_json::from_slice(&raw).expect("parse envelope");
    assert_eq!(staged.entity.stream_id, "stream-a");

    // Case B untouched.
    assert_eq!(report.unresolvable_no_chunks, 1, "case B unresolvable");
    let raw_b = store_enc
        .get(b"graph:entity:ent-t10b")
        .expect("get")
        .expect("present");
    let staged_b: StoredEntityRead = serde_json::from_slice(&raw_b).expect("parse envelope");
    assert!(staged_b.entity.stream_id.is_empty(), "case B not scoped");
    assert!(
        staged_b.encrypted_payload.is_empty(),
        "case B not encrypted"
    );
}

// ── T11: alias-carrying repair writes resolvable alias rows ──────────────────

/// T11: repaired entity with aliases is findable by each alias in the stream;
/// no collisions counted.
#[test]
fn t11_alias_rows_written_and_resolvable() {
    let tmp = TempDir::new().expect("tempdir");
    {
        let store = open_noop(&tmp);
        let mut ent = legacy_entity("ent-t11", "Aleksandra", vec!["c-t11".to_string()]);
        ent.aliases = vec!["Ala".to_string(), "Ola".to_string()];
        put_legacy_entity(&store, &ent);
        store
            .store_chunk(&make_chunk("c-t11", "stream-a"))
            .expect("store chunk");
    }

    let store_enc = open_encrypted(&tmp);
    let graph = GraphStore::new(store_enc.clone());
    let report = repair_entity_streams(&store_enc, &graph, false).expect("repair ok");

    assert_eq!(report.repaired, 1);
    assert_eq!(report.alias_collisions_skipped, 0);
    for q in ["Aleksandra", "Ala", "Ola"] {
        let found = graph
            .get_entity_by_name(q, "stream-a")
            .expect("lookup ok")
            .unwrap_or_else(|| panic!("'{q}' must resolve after repair"));
        assert_eq!(found.id, "ent-t11", "'{q}' must resolve to repaired entity");
    }
}

// ── T12: alias collision never overwrites the incumbent (critic MED-2) ───────

/// T12: when a repaired entity's alias token already maps to a different
/// incumbent in the stream, the alias row is SKIPPED (incumbent preserved,
/// collision counted); canonical-name row is still written.
#[test]
fn t12_alias_collision_preserves_incumbent() {
    let tmp = TempDir::new().expect("tempdir");
    {
        let store = open_noop(&tmp);
        let mut ent = legacy_entity("ent-t12", "Robert", vec!["c-t12".to_string()]);
        ent.aliases = vec!["Bobby".to_string()];
        put_legacy_entity(&store, &ent);
        store
            .store_chunk(&make_chunk("c-t12", "stream-a"))
            .expect("store chunk");
    }

    let store_enc = open_encrypted(&tmp);
    let graph = GraphStore::new(store_enc.clone());

    // Incumbent holding the alias "Bobby" in stream-a, created through the
    // normal path BEFORE the repair runs.
    let incumbent = graph
        .get_or_create_entity("Bob", "Person", &["Bobby".to_string()], "stream-a")
        .expect("create incumbent");

    let report = repair_entity_streams(&store_enc, &graph, false).expect("repair ok");

    assert_eq!(report.repaired, 1, "name 'Robert' is free — repaired");
    assert_eq!(
        report.alias_collisions_skipped, 1,
        "alias 'Bobby' collides with incumbent"
    );

    // Incumbent's alias mapping is preserved.
    let bobby = graph
        .get_entity_by_name("Bobby", "stream-a")
        .expect("lookup ok")
        .expect("'Bobby' still resolves");
    assert_eq!(
        bobby.id, incumbent.id,
        "'Bobby' must still point at incumbent"
    );

    // Repaired entity reachable by its canonical name.
    let robert = graph
        .get_entity_by_name("Robert", "stream-a")
        .expect("lookup ok")
        .expect("'Robert' resolves");
    assert_eq!(robert.id, "ent-t12");
}
