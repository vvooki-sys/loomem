//! Stage-1 acceptance tests for source fragments, end to end over real
//! storage (tempdir RocksDB + Tantivy): `memory_ingest` → persisted chunks →
//! `/v1/search` and `memory_search` → delete cascade. Each test pins one
//! contract of the stage-1 plan; none mocks storage.
//!
//! Harness limitation: the test app has no embedder (`local_embedder: None`,
//! `vector_enabled: false`), so every search here is served by the BM25 leg
//! alone. Queries therefore use rare tokens that BM25 resolves exactly. The
//! production motivation for fragments — the local embedder truncating a
//! long source at 512 tokens — cannot be reproduced here; what these tests
//! prove is the storage, evidence and lifecycle contract of the fragments.
use super::fixtures::*;

use loomem_core::fragments::{fragment_id, FragmentMeta};
use loomem_core::memory_extractor::ChatHttpReply;
use loomem_core::storage::{Chunk, ProvenanceRole};
use serde_json::{json, Value};
use std::sync::Arc;

use super::*;

const STREAM: &str = "stage1";
/// Appears only in the last user turn of the long conversation.
const NEEDLE: &str = "zygomorphic-kettle-7731";
/// Needle of the metamorphic pair, placed in turn 3 (index 2).
const EARLY_NEEDLE: &str = "amaranth-sextant-5519";
/// Present in every turn, so a small `top_k` must truncate candidates.
const SHARED_TERM: &str = "lighthouse";
/// `fixtures::args` stamps this conversation date on every ingest.
const CONVERSATION_DATE: &str = "2024-03-20";
const LONG_TURNS: usize = 13;

const TOPICS: [&str; 16] = [
    "harbour cranes",
    "glacier moraines",
    "copper boilers",
    "orchard ladders",
    "tidal charts",
    "loom shuttles",
    "basalt columns",
    "kite spars",
    "cider presses",
    "signal flags",
    "peat stoves",
    "ferry timetables",
    "bell foundries",
    "salt pans",
    "mill races",
    "canal locks",
];

/// One transcript turn: `user:` on even indexes, `assistant:` on odd ones.
/// Every turn opens with its own index and topic, so the first 150 chars
/// never share the >80% prefix that `filter_candidates` deduplicates on,
/// and is padded past ~280 chars so the conversation reaches several
/// thousand characters within the 400-token fragment budget.
fn turn(index: usize, extra: Option<&str>) -> String {
    let role = if index.is_multiple_of(2) {
        "user"
    } else {
        "assistant"
    };
    let topic = TOPICS[index % TOPICS.len()];
    let mut text =
        format!("{role}: Turn {index} is about {topic}; the {SHARED_TERM} keeper logged it. ");
    while text.len() < 280 {
        text.push_str(&format!("More notes on {topic} follow for turn {index}. "));
    }
    if let Some(extra) = extra {
        text.push_str(extra);
    }
    text
}

/// Deterministic conversation of `turns` turns; `needle` is appended to the
/// turn at the given index only. `conversation(n, x)` is a byte prefix of
/// `conversation(m, x)` for `n < m`.
fn conversation(turns: usize, needle: Option<(usize, &str)>) -> String {
    (0..turns)
        .map(|index| turn(index, needle.filter(|(at, _)| *at == index).map(|(_, n)| n)))
        .collect::<Vec<_>>()
        .join("\n")
}

fn long_conversation() -> String {
    conversation(LONG_TURNS, Some((LONG_TURNS - 1, NEEDLE)))
}

/// Test app with fragments on and time decay off (scores then depend only
/// on BM25 and the per-chunk multipliers, so they are reproducible).
/// `extraction` selects the extraction-enabled fixture base (injected chat,
/// no API key) or the default app (extraction disabled).
fn stage1_app(
    extraction: bool,
    configure: impl FnOnce(&mut loomem_core::Config),
) -> (axum::Router, Arc<AppState>) {
    let (_, base) = if extraction {
        fixture(20000)
    } else {
        crate::tests::make_test_app()
    };
    let mut config = base.config.clone();
    config.fragments.enabled = true;
    config.search.decay.l0_lambda = 0.0;
    config.search.decay.l1_lambda = 0.0;
    configure(&mut config);
    crate::tests::make_test_app_cfg(config)
}

/// Ingest through the production tool (extraction disabled in the app).
async fn ingest(state: &Arc<AppState>, content: &str) -> ToolResult {
    let result = tool_ingest(state, args(content), STREAM, None)
        .await
        .unwrap();
    assert_ne!(result.is_error, Some(true), "{}", result.content[0].text);
    result
}

fn by_kind(records: &[Chunk], kind_name: &str) -> Vec<Chunk> {
    records
        .iter()
        .filter(|chunk| kind(chunk) == kind_name)
        .cloned()
        .collect()
}

fn persisted(state: &AppState, id: &str) -> Chunk {
    state.store.get_chunk(id).unwrap().unwrap()
}

fn meta(chunk: &Chunk) -> &FragmentMeta {
    chunk
        .fragment
        .as_ref()
        .unwrap_or_else(|| panic!("{} is not a fragment", chunk.id))
}

/// The persisted fragments of `source`, in index order.
fn fragments_of(state: &AppState, source: &Chunk) -> Vec<Chunk> {
    let mut fragments: Vec<Chunk> = by_kind(&chunks(state, STREAM), "fragment")
        .into_iter()
        .filter(|chunk| meta(chunk).source_id == source.id)
        .collect();
    fragments.sort_by_key(|chunk| meta(chunk).byte_start);
    fragments
}

/// The one source persisted for `content`, found by its content.
fn source_for(state: &AppState, content: &str) -> Chunk {
    let sources: Vec<Chunk> = by_kind(&chunks(state, STREAM), "source")
        .into_iter()
        .filter(|chunk| chunk.content == content)
        .collect();
    assert_eq!(
        sources.len(),
        1,
        "expected exactly one source for the content"
    );
    sources[0].clone()
}

/// Asserts the fragment is an exact slice of its persisted source and
/// returns that slice.
fn exact_slice<'a>(source: &'a Chunk, fragment: &Chunk) -> &'a str {
    let meta = meta(fragment);
    assert_eq!(meta.source_id, source.id);
    let slice = &source.content[meta.byte_start..meta.byte_end];
    assert_eq!(
        fragment.content, slice,
        "fragment {} is not a slice",
        fragment.id
    );
    slice
}

fn is_fragment_of(id: &str, source: &Chunk) -> bool {
    id.starts_with(&format!("{}:f", source.id))
}

async fn search_json(app: &axum::Router, body: Value) -> Value {
    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/v1/search")
        .header("Authorization", "Bearer admin-token")
        .header("Content-Type", "application/json")
        .body(axum::body::Body::from(body.to_string()))
        .unwrap();
    let (status, bytes) = crate::tests::send(app.clone(), request).await;
    assert_eq!(
        status,
        axum::http::StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&bytes)
    );
    serde_json::from_slice(&bytes).unwrap()
}

async fn search_needle(app: &axum::Router, needle: &str) -> Value {
    search_json(
        app,
        json!({"query": needle, "streams": [STREAM], "top_k": 10}),
    )
    .await
}

fn hits(response: &Value) -> &[Value] {
    response["results"].as_array().unwrap()
}

fn hit_ids(response: &Value) -> Vec<String> {
    hits(response)
        .iter()
        .map(|hit| hit["id"].as_str().unwrap().to_string())
        .collect()
}

async fn mcp_search(state: &Arc<AppState>, query: &str) -> String {
    let result = super::super::tool_search(state, json!({"query": query, "top_k": 10}), STREAM)
        .await
        .unwrap();
    assert_ne!(result.is_error, Some(true), "{}", result.content[0].text);
    result.content[0].text.clone()
}

/// Chunk ids on the `memory_search` text surface (`… (id: <id>)` per hit).
fn mcp_ids(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| line.rsplit_once("(id: "))
        .map(|(_, id)| id.trim_end_matches(')').to_string())
        .collect()
}

/// The first fragment hit of `source` whose stored slice contains `needle`.
fn fragment_hit_containing(
    state: &AppState,
    source: &Chunk,
    ids: &[String],
    needle: &str,
) -> Chunk {
    let fragments: Vec<Chunk> = ids
        .iter()
        .filter(|id| is_fragment_of(id, source))
        .map(|id| persisted(state, id))
        .collect();
    assert!(
        !fragments.is_empty(),
        "no fragment of {} among {ids:?}",
        source.id
    );
    fragments
        .into_iter()
        .find(|fragment| exact_slice(source, fragment).contains(needle))
        .unwrap_or_else(|| panic!("no fragment hit of {} contains {needle}", source.id))
}

// ── Contract 1: buried needle, extraction disabled ───────────────────────

#[tokio::test]
async fn buried_needle_is_retrievable_through_an_exact_fragment_slice() {
    let (_, state) = stage1_app(false, |_| {});
    let content = long_conversation();
    assert!(
        content.len() > 3000,
        "conversation is {} chars",
        content.len()
    );
    let result = ingest(&state, &content).await;
    assert!(result.content[0]
        .text
        .contains("Knowledge extraction disabled."));
    let source = source_for(&state, &content);
    assert_eq!(source.content.matches(NEEDLE).count(), 1);
    let fragments = fragments_of(&state, &source);
    assert_eq!(fragments.len(), LONG_TURNS, "one fragment per turn");
    assert!(result.content[0]
        .text
        .ends_with(&format!(" Stored {LONG_TURNS} fragments.")));
    let ids = mcp_ids(&mcp_search(&state, NEEDLE).await);
    let hit = fragment_hit_containing(&state, &source, &ids, NEEDLE);
    assert_eq!(hit.id, fragment_id(&source.id, 12));
    assert_eq!(meta(&hit).role.as_deref(), Some("user"));
    assert_eq!(meta(&hit).turn_index, 12);
    assert_eq!(
        meta(&hit).conversation_date.as_deref(),
        Some(CONVERSATION_DATE)
    );
    assert_eq!(hit.level, 0);
    assert_eq!(
        hit.source_ids.as_deref(),
        Some([source.id.clone()].as_slice())
    );
    assert_eq!(hit.provenance_role, ProvenanceRole::Claim);
}

/// Control: the same conversation with `[fragments] enabled = false`. In this
/// harness BM25 still returns the whole-session source for the needle (an
/// exact rare-token match has nowhere else to go), so the test asserts that
/// observed behaviour. The fragment path is nonetheless the contract: the
/// source hit carries no quotable byte range, no author role and no
/// per-turn date, and in production the vector leg embeds only the first
/// ~500 tokens of a long source, so a needle in a late turn is invisible to
/// it. This control exists so a change in today's BM25 behaviour is noticed.
#[tokio::test]
async fn control_without_fragments_bm25_returns_the_whole_session_source() {
    let (_, state) = stage1_app(false, |config| config.fragments.enabled = false);
    let content = long_conversation();
    let result = ingest(&state, &content).await;
    assert!(!result.content[0].text.contains("fragments"));
    let source = source_for(&state, &content);
    assert!(by_kind(&chunks(&state, STREAM), "fragment").is_empty());
    let ids = mcp_ids(&mcp_search(&state, NEEDLE).await);
    assert_eq!(
        ids,
        std::slice::from_ref(&source.id),
        "only the whole session is retrievable"
    );
    assert!(persisted(&state, &source.id).fragment.is_none());
}

// ── Contract 2: appending turns never removes earlier evidence ───────────

const SHORT_TURNS: usize = 8;
const EXTENDED_TURNS: usize = SHORT_TURNS + 6;
const EARLY_NEEDLE_TURN: usize = 2;

/// Ingests C1 and C2 = C1 + six more turns as separate sources; returns
/// `(c1_source, c2_source)`.
async fn ingest_metamorphic_pair(state: &Arc<AppState>) -> (Chunk, Chunk) {
    let c1 = conversation(SHORT_TURNS, Some((EARLY_NEEDLE_TURN, EARLY_NEEDLE)));
    let c2 = conversation(EXTENDED_TURNS, Some((EARLY_NEEDLE_TURN, EARLY_NEEDLE)));
    assert!(c2.starts_with(&c1), "C2 must extend C1 byte for byte");
    ingest(state, &c1).await;
    ingest(state, &c2).await;
    (source_for(state, &c1), source_for(state, &c2))
}

#[tokio::test]
async fn appending_turns_keeps_earlier_fragments_byte_identical() {
    let (_, state) = stage1_app(false, |_| {});
    let (first, second) = ingest_metamorphic_pair(&state).await;
    assert_ne!(first.id, second.id);
    let before = fragments_of(&state, &first);
    let after = fragments_of(&state, &second);
    assert_eq!(before.len(), SHORT_TURNS);
    assert_eq!(after.len(), EXTENDED_TURNS);
    // The final turn of C1 is the only one that changes shape in C2: the
    // newline separating it from the appended turns belongs to it there.
    // Every other fragment must be byte-identical in text and range.
    for (index, (old, new)) in before.iter().zip(&after).enumerate().take(SHORT_TURNS - 1) {
        let index = u32::try_from(index).unwrap();
        assert_eq!(old.id, fragment_id(&first.id, index));
        assert_eq!(new.id, fragment_id(&second.id, index));
        assert_eq!(old.content, new.content, "fragment {index} text changed");
        assert_eq!(exact_slice(&first, old), exact_slice(&second, new));
        let (old_meta, new_meta) = (meta(old), meta(new));
        assert_eq!(
            (old_meta.byte_start, old_meta.byte_end),
            (new_meta.byte_start, new_meta.byte_end)
        );
        assert_eq!(old_meta.turn_index, new_meta.turn_index);
        assert_eq!(old_meta.role, new_meta.role);
        assert_eq!(old_meta.conversation_date, new_meta.conversation_date);
        assert_eq!(
            old_meta.conversation_date.as_deref(),
            Some(CONVERSATION_DATE)
        );
    }
    let needle_index = u32::try_from(EARLY_NEEDLE_TURN).unwrap();
    for (source, fragments) in [(&first, &before), (&second, &after)] {
        let needle = &fragments[EARLY_NEEDLE_TURN];
        assert_eq!(needle.id, fragment_id(&source.id, needle_index));
        assert!(exact_slice(source, needle).contains(EARLY_NEEDLE));
        assert_eq!(meta(needle).role.as_deref(), Some("user"));
    }
}

#[tokio::test]
async fn needle_search_returns_a_fragment_from_each_source() {
    let (app, state) = stage1_app(false, |_| {});
    let (first, second) = ingest_metamorphic_pair(&state).await;
    let response = search_json(
        &app,
        json!({"query": EARLY_NEEDLE, "streams": [STREAM], "top_k": 10, "trace": true}),
    )
    .await;
    let ids = hit_ids(&response);
    for source in [&first, &second] {
        assert!(
            ids.iter().any(|id| is_fragment_of(id, source)),
            "no fragment of source {} among {ids:?}; rejections: {}",
            source.id,
            response["trace_metadata"]["rejections"]
        );
        fragment_hit_containing(&state, source, &ids, EARLY_NEEDLE);
    }
    let mcp = mcp_ids(&mcp_search(&state, EARLY_NEEDLE).await);
    for source in [&first, &second] {
        assert!(
            mcp.iter().any(|id| is_fragment_of(id, source)),
            "memory_search lost source {}: {mcp:?}",
            source.id
        );
    }
}

// ── Contract 3: evidence contract reaches the reader ─────────────────────

#[tokio::test]
async fn evidence_fields_reach_http_and_mcp_readers_only_when_enabled() {
    let (app, state) = stage1_app(false, |config| config.search.evidence_fields = true);
    let content = long_conversation();
    ingest(&state, &content).await;
    let source = source_for(&state, &content);
    let fragment = persisted(&state, &fragment_id(&source.id, 12));
    let range = (meta(&fragment).byte_start, meta(&fragment).byte_end);
    assert!(exact_slice(&source, &fragment).contains(NEEDLE));

    let response = search_needle(&app, NEEDLE).await;
    let hit = hits(&response)
        .iter()
        .find(|hit| hit["id"] == fragment.id)
        .unwrap_or_else(|| panic!("fragment hit missing: {response}"));
    let evidence = hit["evidence"].as_object().unwrap();
    let mut keys: Vec<&str> = evidence.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "author",
            "byte_end",
            "byte_start",
            "conversation_date",
            "event_date",
            "event_date_provenance",
            "source_id",
        ]
    );
    assert_eq!(
        hit["evidence"],
        json!({
            "author": "user",
            "conversation_date": CONVERSATION_DATE,
            "event_date": null,
            "event_date_provenance": null,
            "source_id": source.id,
            "byte_start": range.0,
            "byte_end": range.1,
        })
    );
    for hit in hits(&response) {
        assert!(
            hit.get("evidence").is_some(),
            "every hit carries evidence: {hit}"
        );
    }

    // The MCP tag names the source but not the byte range: no MCP tool can
    // read a range, so it stays on the REST surface asserted above.
    let text = mcp_search(&state, NEEDLE).await;
    let tag = format!(
        " [author: user; conversation: {CONVERSATION_DATE}; source: {}] (score: ",
        source.id
    );
    assert!(text.contains(&tag), "{text}");
    assert!(!text.contains(&format!("{}#", source.id)), "{text}");
    assert!(text.contains(&format!("(id: {})", fragment.id)), "{text}");

    let (off_app, off_state) = stage1_app(false, |_| {});
    ingest(&off_state, &content).await;
    let off = search_needle(&off_app, NEEDLE).await;
    assert!(!hits(&off).is_empty());
    for hit in hits(&off) {
        assert!(hit.get("evidence").is_none(), "{hit}");
    }
    assert!(!mcp_search(&off_state, NEEDLE).await.contains("[author:"));
}

// ── Contract 4: extraction failure does not remove text access ───────────

#[tokio::test]
async fn truncated_extraction_reports_failure_and_leaves_fragments_searchable() {
    let (_, state) = stage1_app(true, |_| {});
    assert!(state.config.knowledge_extraction.enabled);
    let chat = Chat::sequence(vec![Ok(ChatHttpReply {
        status: 200,
        body: json!({"choices":[{"finish_reason":"length","message":{
            "content": json!({"facts":[fact(None)]}).to_string()}}]})
        .to_string(),
    })]);
    let content = long_conversation();
    let result = tool_ingest_with(&chat, &state, args(&content), STREAM, None)
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(true), "{}", result.content[0].text);
    let text = &result.content[0].text;
    assert!(text.contains("output token limit reached"), "{text}");
    assert!(text.contains("No facts stored."), "{text}");
    assert!(
        text.ends_with(&format!(" Stored {LONG_TURNS} fragments.")),
        "{text}"
    );
    assert_eq!(chat.calls(), 1);
    let records = chunks(&state, STREAM);
    assert!(
        by_kind(&records, "fact").is_empty(),
        "a truncated reply admits no facts"
    );
    let source = source_for(&state, &content);
    assert!(text.contains(&source.id));
    assert_eq!(fragments_of(&state, &source).len(), LONG_TURNS);
    let ids = mcp_ids(&mcp_search(&state, NEEDLE).await);
    let hit = fragment_hit_containing(&state, &source, &ids, NEEDLE);
    assert_eq!(meta(&hit).role.as_deref(), Some("user"));
}

// ── Contract 5: delete cascade ───────────────────────────────────────────

#[tokio::test]
async fn deleting_the_source_removes_fragments_from_search_cache_and_reads() {
    let (app, state) = stage1_app(false, |config| config.search.cache.enabled = true);
    let content = long_conversation();
    ingest(&state, &content).await;
    let source = source_for(&state, &content);
    let fragment_ids: Vec<String> = fragments_of(&state, &source)
        .iter()
        .map(|chunk| chunk.id.clone())
        .collect();
    assert_eq!(fragment_ids.len(), LONG_TURNS);

    let cold = search_needle(&app, NEEDLE).await;
    fragment_hit_containing(&state, &source, &hit_ids(&cold), NEEDLE);
    assert_eq!(state.query_cache.lock().await.len(), 1);
    let warm = search_needle(&app, NEEDLE).await;
    assert!(hits(&warm)
        .iter()
        .all(|hit| hit["metadata"]["cached"] == true));
    assert!(hit_ids(&warm).iter().any(|id| is_fragment_of(id, &source)));

    let deleted = super::super::tool_delete(&state, json!({"chunk_id": source.id}), STREAM)
        .await
        .unwrap();
    assert_ne!(deleted.is_error, Some(true), "{}", deleted.content[0].text);
    assert!(
        deleted.content[0]
            .text
            .starts_with(&format!("Deleted memory {}.", source.id)),
        "{}",
        deleted.content[0].text
    );
    assert_eq!(
        state.query_cache.lock().await.len(),
        1,
        "the warm entry must still be present so the stale path is exercised"
    );

    let after = search_needle(&app, NEEDLE).await;
    assert!(hits(&after).is_empty(), "{after}");
    assert_eq!(
        mcp_search(&state, NEEDLE).await,
        "No relevant memories found."
    );
    assert!(chunks(&state, STREAM).is_empty());
    for id in fragment_ids.iter().chain(std::iter::once(&source.id)) {
        assert!(
            persisted(&state, id).deleted_at.is_some(),
            "{id} not tombstoned"
        );
        assert_eq!(
            get(&app, id).await.0,
            axum::http::StatusCode::NOT_FOUND,
            "{id}"
        );
    }
}

// ── Contract 6: stage rejections are explained ───────────────────────────

fn stage_ids<'a>(response: &'a Value, stage: &str) -> Vec<&'a str> {
    response["trace_metadata"]["evidence"]["stages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["stage"] == stage)
        .unwrap_or_else(|| panic!("stage {stage} missing: {response}"))["chunk_ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|id| id.as_str().unwrap())
        .collect()
}

#[tokio::test]
async fn traced_search_explains_top_k_truncation_of_fragments() {
    let (app, state) = stage1_app(false, |_| {});
    let content = long_conversation();
    ingest(&state, &content).await;
    let source = source_for(&state, &content);
    let response = search_json(
        &app,
        json!({"query": SHARED_TERM, "streams": [STREAM], "top_k": 2, "trace": true}),
    )
    .await;
    let ranked = stage_ids(&response, "ranked_candidates");
    let survived = stage_ids(&response, "after_filters_and_dedup");
    let answered = stage_ids(&response, "response");
    assert_eq!(answered, hit_ids(&response));
    assert_eq!(answered.len(), 2);
    assert!(
        ranked.len() > answered.len(),
        "the shared term must over-fill top_k: {ranked:?}"
    );
    let rejections = response["trace_metadata"]["rejections"]
        .as_array()
        .unwrap_or_else(|| panic!("rejections missing: {response}"));
    let rejected_ids: Vec<&str> = rejections
        .iter()
        .map(|entry| entry["chunk_id"].as_str().unwrap())
        .collect();
    for id in ranked.iter().filter(|id| !answered.contains(id)) {
        assert!(
            rejected_ids.contains(id),
            "{id} dropped without a reason: {rejections:?}"
        );
    }
    let truncated: Vec<&str> = rejections
        .iter()
        .filter(|entry| entry["reason"] == "truncated_top_k")
        .inspect(|entry| assert_eq!(entry["stage"], "response", "{entry}"))
        .map(|entry| entry["chunk_id"].as_str().unwrap())
        .collect();
    let mut expected: Vec<&str> = survived
        .iter()
        .filter(|id| !answered.contains(id))
        .copied()
        .collect();
    let mut got = truncated.clone();
    expected.sort_unstable();
    got.sort_unstable();
    assert_eq!(
        got, expected,
        "truncated set must equal survived minus response"
    );
    assert!(
        truncated.iter().any(|id| is_fragment_of(id, &source)),
        "a fragment must be among the truncated ids: {truncated:?}"
    );
}

// ── Contract 7: provenance knob ──────────────────────────────────────────

/// Ingests the long conversation under the given fragment role; returns the
/// app, its state and the source.
async fn ingest_with_role(role: ProvenanceRole) -> (axum::Router, Arc<AppState>, Chunk) {
    let (app, state) = stage1_app(false, |config| config.fragments.provenance_role = role);
    let content = long_conversation();
    ingest(&state, &content).await;
    let source = source_for(&state, &content);
    (app, state, source)
}

#[tokio::test]
async fn provenance_role_knob_is_persisted_on_every_fragment() {
    for role in [ProvenanceRole::Claim, ProvenanceRole::Evidence] {
        let (_, state, source) = ingest_with_role(role).await;
        let fragments = fragments_of(&state, &source);
        assert_eq!(fragments.len(), LONG_TURNS);
        for fragment in &fragments {
            assert_eq!(persisted(&state, &fragment.id).provenance_role, role);
        }
        // The source keeps its own role regardless of the knob.
        assert_eq!(
            persisted(&state, &source.id).provenance_role,
            ProvenanceRole::Evidence
        );
    }
}

/// `/v1/search` score of the needle fragment under the given role.
async fn needle_score_with_role(role: ProvenanceRole) -> f64 {
    let (app, state, source) = ingest_with_role(role).await;
    let response = search_needle(&app, NEEDLE).await;
    let needle = fragment_hit_containing(&state, &source, &hit_ids(&response), NEEDLE);
    hits(&response)
        .iter()
        .find(|hit| hit["id"] == needle.id)
        .unwrap()["score"]
        .as_f64()
        .unwrap()
}

/// Pins a pre-existing ranking asymmetry so a change to it is noticed.
///
/// `trust_provenance_multiplier` (`hybrid_search.rs:589`: claim 1.0,
/// evidence 0.5, cue 0.8) is applied only on the fused path —
/// `loomem-core/src/hybrid_search.rs:340-360` inside `fuse_with_vector_budget`
/// (`final_score = fusion_score * time_decay * trust_prov * attrib`). The
/// BM25-only path,
/// `loomem-core/src/hybrid_search/candidate_budget.rs:169-208`
/// (`bm25_only_with_budget`, `final_score = normalized_score * time_decay`),
/// never loads the chunk and applies no provenance or trust multiplier. The
/// handler routes a query there whenever `[storage] vector_enabled = false`
/// or the query classifies as `Simple` (under 5 or over 15 words with no
/// routing keyword) — `loomem-server/src/handlers/search/candidates.rs:85-105`. So
/// the `[fragments] provenance_role` knob affects only fused (vector-enabled,
/// non-Simple) retrieval; on short queries claim and evidence fragments
/// score identically. This harness has no embedder, so it exercises exactly
/// that path: with decay off and byte-identical corpora, the two scores
/// must be equal. Changing the BM25-only path to honour the multiplier
/// would re-rank every existing chunk on short queries, so it is out of
/// scope for stage 1; this test documents the gap instead of hiding it.
#[tokio::test]
async fn bm25_only_path_scores_claim_and_evidence_fragments_identically() {
    let claim = needle_score_with_role(ProvenanceRole::Claim).await;
    let evidence = needle_score_with_role(ProvenanceRole::Evidence).await;
    assert!(evidence > 0.0, "evidence-role fragment scores: {evidence}");
    assert!(
        (claim - evidence).abs() < 1e-9,
        "claim {claim} and evidence {evidence} must score identically on the \
         BM25-only path: the provenance multiplier is applied only in fused \
         retrieval (hybrid_search.rs:340-360). If this fails, either the \
         BM25-only path started honouring provenance (update this pin and \
         the CHANGELOG) or the harness gained a vector leg."
    );
}
