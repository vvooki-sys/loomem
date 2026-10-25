//! Real store/index regressions: conversation time is not event time.
use super::*;
use crate::auth::{AuthContext, KeyScope, UserRole};
use loomem_core::fragments::{fragment_chunk, SourceFragment};

const SOURCE: &str = "00000000-0000-4000-8000-000000000024";

fn fixture() -> Vec<Chunk> {
    let source = chunk(SOURCE, None, true);
    let mut template = chunk("template", None, false);
    template.consolidated = true;
    template.extraction_meta = None;
    template.metadata = Some(json!({"_loomem_ingest":{"version":1,"kind":"fragment"}}));
    template.source = Some(SourceTag::from_agent("mcp-ingest"));
    let fragment = fragment_chunk(
        &SourceFragment {
            id: format!("{SOURCE}:f0"),
            source_id: SOURCE.into(),
            index: 0,
            byte_start: 0,
            byte_end: source.content.len(),
            turn_index: 0,
            role: Some("user".into()),
            // Intentionally inside the requested range, as is recorded time.
            conversation_date: Some("2024-01-01".into()),
            text: source.content.clone(),
        },
        &template,
    );
    let mut fact = chunk("known-fact", Some("2024-02-03"), true);
    fact.metadata = Some(json!({"_loomem_ingest":{"version":1,"kind":"fact"}}));
    fact.provenance_role = ProvenanceRole::Claim;
    fact.source = Some(SourceTag::from_agent("mcp-ingest"));
    fact.source_ids = Some(vec![SOURCE.into()]);
    fact.extraction_meta.as_mut().unwrap().extracted_from = Some(SOURCE.into());
    assert!(is_ingest_fragment(&fragment));
    assert!(is_ingest_fact_shape(&fact));
    vec![source, fragment, fact, chunk("legacy", None, false)]
}

async fn seed_fixture(state: &Arc<AppState>) -> Vec<Chunk> {
    let rows = fixture();
    for row in &rows {
        // Stale lexical index deliberately advertises recorded time for all rows.
        seed(state, row).await;
        state.store.store_embedding(&row.id, vec![1.0]).unwrap();
    }
    rows
}

fn context(state: &Arc<AppState>) -> (SearchRequest, QueryContext) {
    let payload = serde_json::from_value(json!({
        "query":"orchard","streams":["streamA"],"top_k":20,
        "date_from":"2024-01-01","date_to":"2024-12-31"
    }))
    .unwrap();
    let auth = AuthContext::single_stream("streamA", UserRole::Admin, KeyScope::Shared, None, true);
    let ctx = super::super::super::prepare_query_context(state, &auth, &payload).unwrap();
    (payload, ctx)
}

fn results(rows: &[Chunk]) -> Vec<HybridSearchResult> {
    rows.iter()
        .map(|row| HybridSearchResult {
            id: row.id.clone(),
            content: row.content.clone(),
            user_id: String::new(),
            app_id: String::new(),
            level: row.level,
            timestamp: i64::try_from(row.timestamp).unwrap(),
            score: 1.0,
            bm25_score: 1.0,
            vector_score: 1.0,
            time_decay_factor: 1.0,
        })
        .collect()
}

#[tokio::test]
async fn fragment_unknown_event_is_excluded_before_vector_rank_and_final_filter() {
    let (_, state) = make_test_app();
    let rows = seed_fixture(&state).await;
    let (payload, ctx) = context(&state);
    let mut admitted: Vec<_> = multi_query_embeddings(&state, &ctx, None)
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    admitted.sort();
    assert_eq!(admitted, ["known-fact", "legacy"]);
    let all = state.store.get_all_embeddings().unwrap();
    assert_eq!(embeddings(all.clone(), None, &state), all);

    let mut final_results = results(&rows);
    let mut log = super::super::super::rejections::RejectionLog::new(false);
    apply(&mut final_results, &ctx, &payload, &state, &mut log);
    assert_eq!(
        final_results
            .iter()
            .map(|row| row.id.as_str())
            .collect::<Vec<_>>(),
        ["known-fact", "legacy"]
    );
}

#[tokio::test]
async fn final_lane_guard_cannot_reintroduce_fragment_from_stale_candidates() {
    let (_, state) = make_test_app();
    let rows = seed_fixture(&state).await;
    let (payload, ctx) = context(&state);
    let lane =
        super::super::super::operation_lane::finalize(results(&rows), 0, &ctx, &payload, &state)
            .await;
    let metrics = super::super::super::telemetry::ResponseMetrics {
        start: ctx.start,
        complexity: ctx.complexity,
        dedup_removed: 0,
        total_results_before_topk: rows.len(),
        final_top_k: 20,
        evidence: None,
        operation: None,
        retrieval_v2: None,
    };
    let (returned, _) = finish(lane, metrics, &ctx, &state);
    assert_eq!(
        returned
            .iter()
            .map(|row| row.id.as_str())
            .collect::<Vec<_>>(),
        ["known-fact", "legacy"]
    );
}

#[tokio::test]
async fn stale_lexical_fragment_is_excluded_on_cold_path_but_allowed_without_range() {
    let (app, state) = make_test_app();
    let rows = seed_fixture(&state).await;
    assert_eq!(ids(&http(&app, true).await), ["known-fact", "legacy"]);
    let unrestricted = ids(&http(&app, false).await);
    assert_eq!(unrestricted.len(), rows.len());
    assert!(unrestricted.contains(&rows[1].id));
}

#[tokio::test]
async fn authoritative_lexical_index_also_excludes_undated_fragment() {
    let (app, state) = make_test_app();
    seed_fixture(&state).await;
    // Use the real startup repair writer to replace deliberately stale dates.
    assert_eq!(
        state
            .tantivy
            .lock()
            .await
            .repair_ingest_event_dates(&state.store)
            .unwrap(),
        3
    );
    assert_eq!(ids(&http(&app, true).await), ["known-fact", "legacy"]);
    assert_eq!(ids(&http(&app, false).await).len(), 4);
}

#[tokio::test]
async fn late_cache_publish_cannot_restore_undated_fragment() {
    let (_, original) = make_test_app();
    let mut config = original.config.clone();
    config.search.cache.enabled = true;
    let (app, state) = make_test_app_cfg(config);
    let rows = seed_fixture(&state).await;
    let (_, ctx) = context(&state);
    state
        .query_cache
        .lock()
        .await
        .insert(ctx.cache_key.unwrap(), results(&rows));
    let response = http(&app, true).await;
    assert_eq!(ids(&response), ["known-fact", "legacy"]);
    assert_eq!(response["results"][0]["metadata"]["cached"], true);
}

#[test]
fn only_recognized_fragment_shape_gets_strict_event_semantics() {
    let mut fragment = fixture().remove(1);
    let filter = DateFilter::Range(1_704_067_200, 1_735_689_599);
    assert!(!allows(&fragment, Some(&filter)));
    assert!(allows(&fragment, None));
    // A fragment-like legacy row must not acquire new filtering semantics.
    fragment.metadata = None;
    assert!(!is_ingest_fragment(&fragment));
    assert!(allows(&fragment, Some(&filter)));
}
