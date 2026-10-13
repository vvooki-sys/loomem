use super::*;

#[tokio::test]
async fn rerank_pool_excludes_superseded_before_selection_and_preserves_tail() {
    let (_app, state) = make_test_app();
    let mut payload = req(None, None);
    payload.top_k = Some(1);
    let ctx = prepare_query_context(&state, &admin_auth(), &payload).unwrap();
    let candidates = ["old", "current", "other"]
        .into_iter()
        .map(|id| {
            let mut c = chunk(id, id, None);
            c.is_latest = id != "old";
            state.store.store_chunk(&c).unwrap();
            loomem_core::HybridSearchResult {
                id: c.id,
                content: c.content,
                user_id: String::new(),
                app_id: String::new(),
                level: 0,
                timestamp: 1_700_000_000,
                score: 1.0,
                bm25_score: 1.0,
                vector_score: 0.0,
                time_decay_factor: 1.0,
            }
        })
        .collect();
    let filtered = filter_candidates(candidates, &ctx, &payload, &state);
    assert_eq!(filtered.results.len(), 2, "eligible tail feeds reranker");
    assert!(filtered.results.iter().all(|r| r.id != "old"));
    let ranked =
        cache_policy::reranked_results(vec![1, 1, 999], &filtered.results, &filtered.results, 2);
    assert_eq!(
        ranked.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
        vec!["other", "current"]
    );
    let bounded = cache_policy::reranked_results(
        vec![1],
        &filtered.results,
        &filtered.results,
        filtered.final_top_k,
    );
    assert_eq!(bounded.len(), 1);
    assert_eq!(bounded[0].id, "other");
    let filled =
        cache_policy::reranked_results(vec![0], &filtered.results[..1], &filtered.results, 2);
    assert_eq!(
        filled.len(),
        2,
        "short rerank pool must not shrink cached response"
    );
}
