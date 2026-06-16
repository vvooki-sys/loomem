//! Person alias detection for entity dedup (cycle/131 + cycle/133).
//!
//! Implements the D1/D2/D3 decisions: Person-only, token-boundary aligned
//! contiguous-subsequence match, existing-first. See brief §3 decisions D1–D9
//! (cycle/133 replaced substring-anywhere with token-boundary match, D1-D5).

use anyhow::Result;

use super::{EntityNode, GraphStore};

/// Find an existing canonical Person entity whose token sequence is a
/// contiguous subsequence of `name`'s tokens, or vice-versa (token-boundary
/// aligned, case-insensitive, whitespace-trimmed via `split_whitespace`).
///
/// Returns `None` for non-Person entity types, empty/whitespace names, or
/// when no token-boundary contiguous-subsequence match exists among the
/// stream's Person entities.
///
/// Iteration order is `created_at` ascending so the first-observed form wins
/// (D7 existing-first rule from cycle/131 D3).
pub fn find_person_alias(
    name: &str,
    entity_type: &str,
    stream_id: &str,
    graph: &GraphStore,
) -> Result<Option<EntityNode>> {
    if entity_type != "Person" {
        return Ok(None);
    }
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let lower_incoming = trimmed.to_lowercase();
    let incoming_tokens: Vec<&str> = lower_incoming.split_whitespace().collect();
    if incoming_tokens.is_empty() {
        return Ok(None);
    }
    for entity in graph.scan_entities_in_stream(stream_id)? {
        if entity.entity_type != "Person" {
            continue;
        }
        let lower_existing = entity.canonical_name.trim().to_lowercase();
        let existing_tokens: Vec<&str> = lower_existing.split_whitespace().collect();
        if existing_tokens.is_empty() {
            continue;
        }
        let (short, long) = if incoming_tokens.len() <= existing_tokens.len() {
            (incoming_tokens.as_slice(), existing_tokens.as_slice())
        } else {
            (existing_tokens.as_slice(), incoming_tokens.as_slice())
        };
        if long.windows(short.len()).any(|win| win == short) {
            return Ok(Some(entity));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RocksDbConfig;
    use crate::storage::RocksDbStore;
    use std::sync::Arc;

    fn make_graph() -> (GraphStore, tempfile::TempDir) {
        let tmp = tempfile::TempDir::new().unwrap();
        let config = RocksDbConfig {
            max_open_files: 100,
            compression: "none".to_string(),
            write_buffer_size: 4 * 1024 * 1024,
            max_write_buffer_number: 2,
        };
        let store = Arc::new(RocksDbStore::open(tmp.path(), &config).unwrap());
        (GraphStore::new(store), tmp)
    }

    const STREAM: &str = "test-stream";

    #[test]
    fn alias_substring_short_in_long() {
        // "Anna" exists; ingest "Anna Nowak" → match
        let (graph, _tmp) = make_graph();
        graph
            .get_or_create_entity("Anna", "Person", &[], STREAM)
            .unwrap();
        let result = find_person_alias("Anna Nowak", "Person", STREAM, &graph).unwrap();
        assert!(result.is_some(), "should match: short in long");
        assert_eq!(result.unwrap().canonical_name, "Anna");
    }

    #[test]
    fn alias_substring_long_in_short() {
        // "Anna Nowak" exists; ingest "Anna" → match
        let (graph, _tmp) = make_graph();
        graph
            .get_or_create_entity("Anna Nowak", "Person", &[], STREAM)
            .unwrap();
        let result = find_person_alias("Anna", "Person", STREAM, &graph).unwrap();
        assert!(result.is_some(), "should match: long in short");
        assert_eq!(result.unwrap().canonical_name, "Anna Nowak");
    }

    #[test]
    fn alias_case_insensitive() {
        // "ANNA" should match "anna nowak"
        let (graph, _tmp) = make_graph();
        graph
            .get_or_create_entity("anna nowak", "Person", &[], STREAM)
            .unwrap();
        let result = find_person_alias("ANNA", "Person", STREAM, &graph).unwrap();
        assert!(result.is_some(), "case-insensitive match must work");
    }

    #[test]
    fn alias_no_match_unrelated() {
        // "Krzysztof" + existing "Adam" → None
        let (graph, _tmp) = make_graph();
        graph
            .get_or_create_entity("Adam", "Person", &[], STREAM)
            .unwrap();
        let result = find_person_alias("Krzysztof", "Person", STREAM, &graph).unwrap();
        assert!(result.is_none(), "unrelated names must not match");
    }

    #[test]
    fn alias_skips_non_person() {
        // entity_type = "Organization" → None even if names overlap
        let (graph, _tmp) = make_graph();
        graph
            .get_or_create_entity("Acme", "Organization", &[], STREAM)
            .unwrap();
        let result = find_person_alias("Acme Memory", "Organization", STREAM, &graph).unwrap();
        assert!(result.is_none(), "non-Person entity_type must return None");
    }

    #[test]
    fn alias_empty_name() {
        let (graph, _tmp) = make_graph();
        let result = find_person_alias("", "Person", STREAM, &graph).unwrap();
        assert!(result.is_none(), "empty name must return None");
    }

    #[test]
    fn alias_whitespace_only() {
        let (graph, _tmp) = make_graph();
        let result = find_person_alias("   ", "Person", STREAM, &graph).unwrap();
        assert!(result.is_none(), "whitespace-only name must return None");
    }

    // cycle/133 — token-boundary match: false-positive regression tests
    // (substring-anywhere would have matched all three no-match cases below).

    #[test]
    fn alias_no_match_mid_token_substring() {
        // existing "An" + ingest "Janet" → None (mid-token substring, not a token-boundary match)
        let (graph, _tmp) = make_graph();
        graph
            .get_or_create_entity("An", "Person", &[], STREAM)
            .unwrap();
        let result = find_person_alias("Janet", "Person", STREAM, &graph).unwrap();
        assert!(result.is_none(), "mid-token substring must not alias-merge");
    }

    #[test]
    fn alias_no_match_intra_token_prefix() {
        // existing "Anna" + ingest "Annaewski" → None (different single tokens)
        let (graph, _tmp) = make_graph();
        graph
            .get_or_create_entity("Anna", "Person", &[], STREAM)
            .unwrap();
        let result = find_person_alias("Annaewski", "Person", STREAM, &graph).unwrap();
        assert!(result.is_none(), "intra-token prefix must not alias-merge");
    }

    #[test]
    fn alias_no_match_different_single_tokens() {
        // existing "Andre" + ingest "Andrew" → None
        let (graph, _tmp) = make_graph();
        graph
            .get_or_create_entity("Andre", "Person", &[], STREAM)
            .unwrap();
        let result = find_person_alias("Andrew", "Person", STREAM, &graph).unwrap();
        assert!(
            result.is_none(),
            "different single tokens must not alias-merge"
        );
    }

    #[test]
    fn alias_match_polish_short_first_name() {
        // existing "Ola" + ingest "Ola Kowalska" → MATCH (token-prefix)
        let (graph, _tmp) = make_graph();
        graph
            .get_or_create_entity("Ola", "Person", &[], STREAM)
            .unwrap();
        let result = find_person_alias("Ola Kowalska", "Person", STREAM, &graph).unwrap();
        assert!(
            result.is_some(),
            "Polish 3-char first name must still alias-merge"
        );
        assert_eq!(result.unwrap().canonical_name, "Ola");
    }

    #[test]
    fn alias_match_token_suffix() {
        // existing "Anna Nowak" + ingest "Nowak" → MATCH (single token suffix)
        let (graph, _tmp) = make_graph();
        graph
            .get_or_create_entity("Anna Nowak", "Person", &[], STREAM)
            .unwrap();
        let result = find_person_alias("Nowak", "Person", STREAM, &graph).unwrap();
        assert!(result.is_some(), "token-suffix match must alias-merge");
        assert_eq!(result.unwrap().canonical_name, "Anna Nowak");
    }
}
