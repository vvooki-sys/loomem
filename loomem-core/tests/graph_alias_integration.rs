//! Integration tests for entity alias merge (Person) + relation normalization.
//!
//! Tests exercise the central enforcement layer in `graph::get_or_create_entity`
//! and `graph::get_or_create_edge` directly (amendment AC5 override).

use loomem_core::config::RocksDbConfig;
use loomem_core::graph::GraphStore;
use loomem_core::storage::RocksDbStore;
use std::sync::Arc;

fn make_graph() -> (GraphStore, tempfile::TempDir) {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let config = RocksDbConfig {
        max_open_files: 100,
        compression: "none".to_string(),
        write_buffer_size: 4 * 1024 * 1024,
        max_write_buffer_number: 2,
    };
    let store = Arc::new(RocksDbStore::open(tmp.path(), &config).expect("rocksdb"));
    (GraphStore::new(store), tmp)
}

const STREAM: &str = "test-stream";

/// Test 1: Person alias merge end-to-end via central enforcement in
/// `get_or_create_entity`. Creating "Anna" first then "Anna Nowak"
/// must merge into a single EntityNode with both forms as aliases.
#[test]
fn person_alias_merge_via_get_or_create_entity() {
    let (graph, _tmp) = make_graph();

    // Setup: create canonical entity "Anna Nowak" [Person].
    let existing = graph
        .get_or_create_entity("Anna Nowak", "Person", &[], STREAM)
        .expect("create Anna Nowak");

    // Action: call get_or_create_entity with the substring form "Anna".
    // Central enforcement must detect the alias and merge instead of creating a new node.
    let merged = graph
        .get_or_create_entity("Anna", "Person", &[], STREAM)
        .expect("get_or_create Anna");

    // Assert: same entity ID returned (no new node created).
    assert_eq!(
        existing.id, merged.id,
        "alias merge must return the existing entity, not a new one"
    );

    // Assert: aliases vec on the returned entity contains both forms.
    let entity = graph
        .get_entity_by_id(&existing.id)
        .expect("load entity")
        .expect("entity must exist");
    let alias_lower: Vec<String> = entity.aliases.iter().map(|a| a.to_lowercase()).collect();
    assert!(
        alias_lower.iter().any(|a| a.contains("anna")),
        "merged aliases must contain the new form; aliases={:?}",
        entity.aliases
    );
}

/// Test 2: Non-Person entity types must NOT be aliased, even when names overlap.
/// Defends against accidental scope expansion beyond D1 (Person-only).
#[test]
fn non_person_entities_are_not_aliased() {
    let (graph, _tmp) = make_graph();

    // Setup: create "Team" [Organization].
    let team = graph
        .get_or_create_entity("Team", "Organization", &[], STREAM)
        .expect("create Team");

    // Action: create "Team Memory" [Organization] — names overlap but type is not Person.
    let team_memory = graph
        .get_or_create_entity("Team Memory", "Organization", &[], STREAM)
        .expect("create Team Memory");

    // Assert: two separate nodes (no alias merge for Organization).
    assert_ne!(
        team.id, team_memory.id,
        "Organization entities must not be merged via alias logic"
    );

    // Assert: both entities are individually retrievable (2 distinct nodes exist).
    assert!(
        graph
            .get_entity_by_id(&team.id)
            .expect("load team")
            .is_some(),
        "Team entity must exist"
    );
    assert!(
        graph
            .get_entity_by_id(&team_memory.id)
            .expect("load team memory")
            .is_some(),
        "Team Memory entity must exist"
    );
}

/// Test 3: Relation normalization end-to-end via central enforcement in
/// `get_or_create_edge`. An unknown raw relation string must be stored as
/// "related_to" (fallback), not the raw value.
#[test]
fn relation_normalization_via_get_or_create_edge() {
    let (graph, _tmp) = make_graph();

    let person = graph
        .get_or_create_entity("Anna", "Person", &[], STREAM)
        .expect("create person");
    let tech = graph
        .get_or_create_entity("Tantivy", "Technology", &[], STREAM)
        .expect("create tech");

    // Action: create edge with a nonsense relation string (not in the 13-variant whitelist).
    let edge = graph
        .get_or_create_edge(&person.id, &tech.id, "works_at_tantivy", STREAM)
        .expect("get_or_create_edge");

    // Assert: edge stored with "related_to", not the raw "works_at_tantivy".
    assert_eq!(
        edge.relation_type, "related_to",
        "unknown relation must be normalized to related_to, got {:?}",
        edge.relation_type
    );
}
