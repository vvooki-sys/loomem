//! `[search] state_followup_*` against real tempdir RocksDB + Tantivy: real
//! BM25 retrieval, the stored embeddings, hybrid fusion with the request
//! budget, the server step (`apply`, which reuses the seeds' stored
//! vectors), then the handler's filter + truncate. The test app has no
//! embedder, so the query vector is given directly (as in
//! `vector_fill_tests`); a seed without a stored vector is skipped.
//!
//! Fixture: 20 "dense" chunks without the query word and `stale` (a
//! project_state fact that mentions it, cosine 0.83) fill the vector lane
//! (2 x top_k 10; `dense-19` falls out); `stale` ranks first. `update` (an
//! event) carries no query word, sits below the lane's cut (cosine 0.6) and
//! is the stale fact's closest neighbour (0.944).
use super::state_followup::{apply, runs};
use super::VectorInputs;
use crate::auth::{AuthContext, KeyScope};
use crate::handlers::search::{
    bm25_retrieve, cache_policy, filter_candidates, prepare_query_context,
};
use crate::handlers::types::{ChannelDiagnostics, SearchRequest, StateFollowupDiag};
use crate::AppState;
use loomem_core::hybrid_search::{SearchCandidateBudget, VectorSearchOptions};
use loomem_core::storage::{Chunk, UserRole, DEFAULT_STREAM_ID};
use loomem_core::{Config, HybridSearchResult, TextDocument};
use serde_json::{json, Value};
use std::sync::Arc;

const QUERY_VECTOR: [f32; 3] = [1.0, 0.0, 0.0];
const STALE: [f32; 3] = [0.83, 0.0, 0.5578];
const UPDATE: [f32; 3] = [0.6, 0.0, 0.8];

fn auth() -> AuthContext {
    AuthContext::single_stream(
        DEFAULT_STREAM_ID,
        UserRole::Admin,
        KeyScope::Shared,
        Some("test".into()),
        true,
    )
}

fn request(extra: Value) -> SearchRequest {
    let mut payload = json!({"query":"apple", "top_k":10, "dry_run":true});
    for (key, value) in extra.as_object().expect("object") {
        payload[key] = value.clone();
    }
    serde_json::from_value(payload).expect("request fixture")
}

async fn seed(
    state: &Arc<AppState>,
    id: &str,
    content: &str,
    vector: Option<&[f32]>,
    meta: Option<&str>,
) {
    let mut chunk = json!({
        "id":id, "content":content, "stream":DEFAULT_STREAM_ID, "level":0,
        "score":1.0, "timestamp":1_700_000_000, "consolidated":false,
        "dormant":false, "in_progress":false,
    });
    if let Some(fact_type) = meta {
        chunk["extraction_meta"] = json!({"fact_type": fact_type, "confidence": 1.0});
    }
    let chunk: Chunk = serde_json::from_value(chunk).expect("chunk fixture");
    state.store.store_chunk(&chunk).expect("seed chunk");
    if let Some(vector) = vector {
        state
            .store
            .store_embedding(id, vector.to_vec())
            .expect("seed vector");
    }
    let mut index = state.tantivy.lock().await;
    index
        .upsert_document(TextDocument {
            id: id.into(),
            content: content.into(),
            user_id: "test".into(),
            app_id: "test".into(),
            level: 0,
            timestamp: 1_700_000_000,
            stream: DEFAULT_STREAM_ID.into(),
            entities: None,
            relations: None,
            event_date: None,
            source_agent: None,
        })
        .expect("seed index");
    index.commit().expect("index commit");
}

/// Time decay off so the wall clock cannot reorder the fixture; no API key
/// (and no key env var) so the gate sees no embedder and nothing can reach
/// a network embedder.
fn test_config() -> Config {
    let (_, original) = crate::tests::make_test_app();
    let mut config = original.config.clone();
    config.search.decay.l0_lambda = 0.0;
    config.search.decay.l1_lambda = 0.0;
    config.llm.api_key = None;
    config.llm.api_key_env = "LOOMEM_TEST_NO_SUCH_EMBEDDING_KEY".into();
    config
}

async fn app(config: Config) -> Arc<AppState> {
    let (_, state) = crate::tests::make_test_app_cfg(config);
    for i in 0_u16..20 {
        let id = format!("dense-{i:02}");
        let content = format!("orchard ledger entry {i}");
        let vector = [1.0, 0.5 + f32::from(i) / 40.0, 0.0];
        seed(&state, &id, &content, Some(&vector), None).await;
    }
    seed(
        &state,
        "stale",
        "ashdown is on the apple ottoman waiting list",
        Some(&STALE),
        Some("project_state"),
    )
    .await;
    seed(
        &state,
        "update",
        "ashdown's ottoman was finished and collected",
        Some(&UPDATE),
        Some("event"),
    )
    .await;
    state
}

fn enabled() -> Config {
    let mut config = test_config();
    config.search.state_followup.enabled = true;
    config
}

struct Run {
    ctx: crate::handlers::search::QueryContext,
    payload: SearchRequest,
    inputs: VectorInputs,
    budget: SearchCandidateBudget,
    pool: Vec<HybridSearchResult>,
}

/// Fusion of `apple`, top_k 10, exactly as `retrieve_primary` builds it.
async fn fuse(state: &Arc<AppState>, payload: SearchRequest) -> Run {
    let ctx = prepare_query_context(state, &auth(), &payload).expect("query context");
    let bm25 = bm25_retrieve(&ctx, &[], &payload, state)
        .await
        .expect("BM25");
    assert_eq!(bm25.len(), 1, "only the stale fact mentions the query word");
    let embeddings = state.store.get_all_embeddings().expect("vectors");
    let budget = SearchCandidateBudget::new(state.config.search.top_k, ctx.limit).expect("budget");
    let pool = state
        .hybrid_search
        .search_with_vector_budget(
            bm25,
            &embeddings,
            &QUERY_VECTOR,
            VectorSearchOptions {
                store: Some(&state.store),
                stream_filter: None,
                guaranteed: None,
                budget,
            },
        )
        .expect("fusion");
    Run {
        ctx,
        payload,
        inputs: VectorInputs {
            embeddings,
            query_embedding: QUERY_VECTOR.to_vec(),
        },
        budget,
        pool,
    }
}

fn response_ids(state: &Arc<AppState>, run: &Run) -> Vec<String> {
    let outcome = cache_policy::truncate(filter_candidates(
        run.pool.clone(),
        &run.ctx,
        &run.payload,
        state,
    ));
    outcome.results.iter().map(|r| r.id.clone()).collect()
}

fn bits(pool: &[HybridSearchResult]) -> Vec<(String, u64, u32)> {
    pool.iter()
        .map(|r| (r.id.clone(), r.score.to_bits(), r.vector_score.to_bits()))
        .collect()
}

#[tokio::test]
async fn off_is_bit_identical_to_a_run_with_the_keys_absent() {
    let absent = app(test_config()).await;
    let mut explicit_cfg = test_config();
    explicit_cfg.search.state_followup.enabled = false;
    let explicit = app(explicit_cfg).await;
    let absent_run = fuse(&absent, request(json!({}))).await;
    let mut explicit_run = fuse(&explicit, request(json!({}))).await;
    assert_eq!(bits(&explicit_run.pool), bits(&absent_run.pool));
    // The stale fact is a lane hit itself (cosine 0.83 displaces dense-19).
    assert_eq!(explicit_run.pool.len(), 20);
    let before = bits(&explicit_run.pool);
    let diag = apply(
        &explicit,
        &explicit_run.ctx,
        &explicit_run.inputs,
        &mut explicit_run.pool,
        explicit_run.budget,
        None,
    )
    .await;
    assert_eq!(diag, None, "off: the step does not run");
    assert_eq!(bits(&explicit_run.pool), before);
    let ids = response_ids(&explicit, &explicit_run);
    assert_eq!(ids.first().map(String::as_str), Some("stale"));
    assert!(!ids.iter().any(|id| id == "update"), "{ids:?}");
}

#[tokio::test]
async fn on_pulls_the_update_into_the_response_from_the_stored_seed_vector() {
    let state = app(enabled()).await;
    let mut run = fuse(&state, request(json!({}))).await;
    let before = bits(&run.pool);
    assert!(
        !run.pool.iter().any(|r| r.id == "update"),
        "out of every lane"
    );
    assert!(
        state.local_embedder.is_none(),
        "no embedder: stored vectors only"
    );
    let diag = apply(
        &state,
        &run.ctx,
        &run.inputs,
        &mut run.pool,
        run.budget,
        None,
    )
    .await
    .expect("the step ran");
    assert_eq!(diag.seeds, ["stale"]);
    assert_eq!(diag.seeds_from_store, diag.seeds.len());
    assert_eq!(diag.embedded, 0);
    assert!(
        diag.injected_ids.iter().any(|id| id == "update"),
        "{diag:?}"
    );
    // The update is the seed's best neighbour: full vector weight, so it
    // follows only the stale fact (BM25 + vector) and dense-00 (cosine 1).
    let ids = response_ids(&state, &run);
    assert_eq!(&ids[..3], ["stale", "dense-00", "update"], "{ids:?}");
    let update = run.pool.iter().find(|r| r.id == "update").expect("merged");
    assert_eq!(update.score.to_bits(), (0.6_f64 * 1.0 * 1.0).to_bits());
    assert_eq!(update.bm25_score, 0.0);
    let cosine = loomem_core::vector_search::cosine_similarity(&STALE, &UPDATE);
    assert_eq!(update.vector_score, cosine, "raw cosine to the seed");
    assert_eq!(
        update.content,
        "ashdown's ottoman was finished and collected"
    );
    for (id, score, vector) in before {
        let after = run.pool.iter().find(|r| r.id == id).expect("kept");
        assert_eq!(after.score.to_bits(), score, "{id}");
        assert_eq!(after.vector_score.to_bits(), vector, "{id}");
    }
}

/// A seed without a stored vector needs a fresh embedding; with no embedder
/// it is skipped (warned) and the pool is untouched. Nothing is sent
/// anywhere: the test config points the key env var at nothing.
#[tokio::test]
async fn seed_without_a_stored_vector_is_skipped_when_no_embedder_exists() {
    let mut config = enabled();
    config.search.state_followup.fact_types = vec![loomem_core::storage::FactType::ProjectState];
    let state = app(config).await;
    seed(
        &state,
        "orphan",
        "apple ottoman delivered to the orphanage",
        None,
        Some("project_state"),
    )
    .await;
    // top_k 25 so the vector-less orphan (BM25 only) is inside the scanned
    // top-k; the wider budget also puts every embedded chunk in the pool,
    // so nothing is left to inject. `fuse` expects one BM25 hit, so build
    // the run by hand.
    let payload = request(json!({"top_k":25}));
    let ctx = prepare_query_context(&state, &auth(), &payload).expect("query context");
    let bm25 = bm25_retrieve(&ctx, &[], &payload, &state)
        .await
        .expect("BM25");
    assert_eq!(bm25.len(), 2);
    let embeddings = state.store.get_all_embeddings().expect("vectors");
    assert!(embeddings.iter().all(|(id, _)| id != "orphan"));
    let budget = SearchCandidateBudget::new(state.config.search.top_k, ctx.limit).expect("budget");
    let mut pool = state
        .hybrid_search
        .search_with_vector_budget(
            bm25,
            &embeddings,
            &QUERY_VECTOR,
            VectorSearchOptions {
                store: Some(&state.store),
                stream_filter: None,
                guaranteed: None,
                budget,
            },
        )
        .expect("fusion");
    let inputs = VectorInputs {
        embeddings,
        query_embedding: QUERY_VECTOR.to_vec(),
    };
    let before = bits(&pool);
    let diag = apply(&state, &ctx, &inputs, &mut pool, budget, None)
        .await
        .expect("the step ran");
    assert_eq!(diag.seeds, ["stale", "orphan"]);
    assert_eq!(diag.seeds_from_store, 1, "stale: stored vector reused");
    assert_eq!(diag.embedded, 0, "orphan: no vector, no embedder, skipped");
    assert!(diag.injected_ids.is_empty());
    assert_eq!(bits(&pool), before);
}

#[tokio::test]
async fn gate_skips_the_step_under_a_date_filter_unless_allowed() {
    let mut config = enabled();
    let state = app(config.clone()).await;
    let plain = fuse(&state, request(json!({}))).await;
    assert!(
        runs(&state, &plain.ctx),
        "no embedder needed: stored vectors"
    );
    let dated_payload = json!({"date_from":"2023-01-01","date_to":"2025-01-01"});
    let mut dated = fuse(&state, request(dated_payload.clone())).await;
    assert!(dated.ctx.date_filter.is_some());
    assert!(!runs(&state, &dated.ctx));
    let before = bits(&dated.pool);
    let diag = apply(
        &state,
        &dated.ctx,
        &dated.inputs,
        &mut dated.pool,
        dated.budget,
        None,
    )
    .await;
    assert_eq!(diag, None);
    assert_eq!(bits(&dated.pool), before);

    config.search.state_followup.with_date_filter = true;
    let state = app(config.clone()).await;
    let dated = fuse(&state, request(dated_payload)).await;
    assert!(runs(&state, &dated.ctx));

    config.search.state_followup.enabled = false;
    let state = app(config).await;
    let plain = fuse(&state, request(json!({}))).await;
    assert!(!runs(&state, &plain.ctx), "disabled wins over everything");
}

/// Enabled, but no state fact of a configured type in the requested top-k:
/// the step ran (empty trail), nothing was embedded, the pool is untouched.
#[tokio::test]
async fn no_seed_in_the_top_k_leaves_the_pool_untouched_with_an_empty_trail() {
    let off = app(test_config()).await;
    let off_run = fuse(&off, request(json!({}))).await;
    let mut config = enabled();
    // `stale` is a project_state fact; nothing in the fixture is a preference.
    config.search.state_followup.fact_types =
        vec![loomem_core::storage::FactType::PreferenceOrDecision];
    let state = app(config).await;
    let mut run = fuse(&state, request(json!({}))).await;
    assert_eq!(bits(&run.pool), bits(&off_run.pool));
    let diag = apply(
        &state,
        &run.ctx,
        &run.inputs,
        &mut run.pool,
        run.budget,
        None,
    )
    .await;
    assert_eq!(
        diag,
        Some(StateFollowupDiag {
            seeds: Vec::new(),
            injected_ids: Vec::new(),
            seeds_from_store: 0,
            embedded: 0,
        })
    );
    assert_eq!(bits(&run.pool), bits(&off_run.pool));
}

/// The diagnostics key appears only when the step ran: the default
/// `channel_diagnostics` shape is unchanged.
#[test]
fn diagnostics_key_is_absent_unless_the_step_ran() {
    let absent = serde_json::to_value(ChannelDiagnostics::default()).unwrap();
    assert!(absent.get("state_followup").is_none());
    assert_eq!(absent.as_object().unwrap().len(), 5);
    let ran = ChannelDiagnostics {
        state_followup: Some(StateFollowupDiag {
            seeds: vec!["stale".into()],
            injected_ids: vec!["update".into()],
            seeds_from_store: 1,
            embedded: 0,
        }),
        ..Default::default()
    };
    assert_eq!(
        serde_json::to_value(ran).unwrap()["state_followup"],
        json!({"seeds":["stale"],"injected_ids":["update"],"seeds_from_store":1,"embedded":0})
    );
}
