use super::*;
use crate::hybrid_search::tests::{base_chunk, test_config};

fn engine() -> HybridSearchEngine {
    let mut config = test_config();
    config.search.decay.l0_lambda = 0.0;
    config.search.decay.l1_lambda = 0.0;
    HybridSearchEngine::new(config)
}

fn lexical(count: u16) -> Vec<SearchResult> {
    (0..count)
        .map(|i| SearchResult {
            id: format!("id{i:03}"),
            content: format!("distinct fixture {i}"),
            user_id: "default".into(),
            app_id: "default".into(),
            stream: "target".into(),
            level: 0,
            timestamp: 0,
            score: f32::from(count - i),
        })
        .collect()
}

fn ids(results: &[HybridSearchResult]) -> Vec<&str> {
    results.iter().map(|r| r.id.as_str()).collect()
}

#[test]
fn budgets_preserve_small_requests_expand_large_and_reject_overflow() {
    let default = SearchCandidateBudget::new(10, 10).unwrap();
    for request in [0, 1, 10] {
        assert_eq!(SearchCandidateBudget::new(10, request).unwrap(), default);
    }
    assert_eq!(default.vector_fetch(false), 20);
    assert_eq!(default.vector_fetch(true), 40);
    assert_eq!(default.pool_size(), 30);
    let expanded = SearchCandidateBudget::new(10, 60).unwrap();
    assert_eq!(expanded.vector_fetch(false), 120);
    assert_eq!(expanded.vector_fetch(true), 240);
    assert_eq!(expanded.pool_size(), 180);
    assert!(SearchCandidateBudget::new(10, usize::MAX).is_err());
    assert!(SearchCandidateBudget::new(usize::MAX, 0).is_err());
    assert_eq!(SearchCandidateBudget::new(0, 0).unwrap().pool_size(), 0);
}

#[test]
fn old_config_deserializes_with_request_ceiling() {
    let mut old = serde_json::to_value(test_config().search).unwrap();
    old.as_object_mut().unwrap().remove("max_request_top_k");
    let search: crate::hybrid_search::SearchConfig = serde_json::from_value(old).unwrap();
    assert_eq!(search.max_request_top_k, 1000);
}

#[test]
fn lexical_pool_expands_and_legacy_wrapper_keeps_identical_membership_and_scores() {
    let engine = engine();
    let original = engine.bm25_only_guaranteed(lexical(100), None).unwrap();
    let same = engine
        .bm25_only_with_budget(
            lexical(100),
            None,
            SearchCandidateBudget::new(10, 10).unwrap(),
        )
        .unwrap();
    assert_eq!(original.len(), 30);
    assert_eq!(ids(&original), ids(&same));
    assert_eq!(
        original.iter().map(|r| r.score).collect::<Vec<_>>(),
        same.iter().map(|r| r.score).collect::<Vec<_>>()
    );
    let expanded = engine
        .bm25_only_with_budget(
            lexical(100),
            None,
            SearchCandidateBudget::new(10, 60).unwrap(),
        )
        .unwrap();
    assert_eq!(expanded.len(), 100);
    assert!(ids(&expanded).contains(&"id059"));
}

#[test]
fn fusion_expands_and_guaranteed_lexical_tail_survives_dense_competition() {
    let engine = engine();
    let scores: Vec<_> = (0..200_u16).map(|i| (format!("id{i:03}"), 1.0)).collect();
    let guaranteed = HashSet::from(["id199".to_string()]);
    let legacy = engine
        .fuse_with_vector_guaranteed(lexical(200), scores.clone(), None, Some(&guaranteed))
        .unwrap();
    let same = engine
        .fuse_with_vector_budget(
            lexical(200),
            scores.clone(),
            None,
            Some(&guaranteed),
            SearchCandidateBudget::new(10, 10).unwrap(),
        )
        .unwrap();
    assert_eq!(ids(&legacy), ids(&same));
    assert_eq!(legacy.len(), 31);
    let expanded = engine
        .fuse_with_vector_budget(
            lexical(200),
            scores,
            None,
            Some(&guaranteed),
            SearchCandidateBudget::new(10, 60).unwrap(),
        )
        .unwrap();
    assert_eq!(expanded.len(), 181);
    assert_eq!(expanded.last().unwrap().id, "id199");
}

#[test]
fn dense_tail_beyond_old_twenty_is_retrievable_and_stream_filter_remains_closed() {
    let engine = engine();
    let dir = tempfile::tempdir().unwrap();
    let store = RocksDbStore::open(dir.path(), &engine.config.storage.rocksdb).unwrap();
    let mut embeddings = Vec::new();
    for i in 0..100_u16 {
        let mut chunk = base_chunk();
        chunk.id = format!("id{i:03}");
        chunk.stream = if i < 20 { "other" } else { "target" }.into();
        store.store_chunk(&chunk).unwrap();
        embeddings.push((chunk.id, vec![1.0, f32::from(i) / 100.0]));
    }
    let old = engine
        .search_with_vector_guaranteed(vec![], &embeddings, &[1.0, 0.0], Some(&store), None, None)
        .unwrap();
    assert_eq!(old.len(), 20);
    let same = engine
        .search_with_vector_budget(
            vec![],
            &embeddings,
            &[1.0, 0.0],
            VectorSearchOptions {
                store: Some(&store),
                stream_filter: None,
                guaranteed: None,
                budget: SearchCandidateBudget::new(10, 10).unwrap(),
            },
        )
        .unwrap();
    assert_eq!(ids(&old), ids(&same));
    let expanded = engine
        .search_with_vector_budget(
            vec![],
            &embeddings,
            &[1.0, 0.0],
            VectorSearchOptions {
                store: Some(&store),
                stream_filter: None,
                guaranteed: None,
                budget: SearchCandidateBudget::new(10, 60).unwrap(),
            },
        )
        .unwrap();
    assert_eq!(expanded.len(), 100);
    assert!(ids(&expanded).contains(&"id059"));
    let streams = vec!["target".into()];
    let scoped = engine
        .search_with_vector_budget(
            vec![],
            &embeddings,
            &[1.0, 0.0],
            VectorSearchOptions {
                store: Some(&store),
                stream_filter: Some(&streams),
                guaranteed: None,
                budget: SearchCandidateBudget::new(10, 60).unwrap(),
            },
        )
        .unwrap();
    assert_eq!(scoped.len(), 80);
    assert!(scoped
        .iter()
        .all(|r| store.get_chunk(&r.id).unwrap().unwrap().stream == "target"));
}

#[test]
fn explicit_stream_scope_without_storage_is_rejected() {
    let engine = engine();
    let streams = vec!["target".into()];
    let result = engine.search_with_vector_budget(
        vec![],
        &[],
        &[1.0, 0.0],
        VectorSearchOptions {
            store: None,
            stream_filter: Some(&streams),
            guaranteed: None,
            budget: SearchCandidateBudget::new(10, 60).unwrap(),
        },
    );
    assert!(result.is_err());
}
