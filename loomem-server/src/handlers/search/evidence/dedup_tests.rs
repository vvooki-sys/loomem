//! Pinned outcome of the content dedup pass in `filter_candidates`: the kept
//! ids, the `(id, stage, reason)` rejections and `dedup_removed` for one fixed
//! candidate set covering every pairing the dedup rule distinguishes.
use super::tests::seed;
use super::*;
use crate::auth::{AuthContext, KeyScope};
use loomem_core::fragments::FragmentMeta;
use loomem_core::storage::UserRole;
use serde_json::json;

const TURN: &str = "user: Turn 2 is about copper boilers; the lighthouse keeper logged it. \
                    More notes on copper boilers follow for turn 2.";
const TURN3: &str = "assistant: Turn 3 covers the brass lanterns in the harbour office; \
                     the clerk filed it twice.";
const HOTEL: &str = "For our autumn project planning conversation, I want to record my current \
                     travel preference for the upcoming conference and the accommodation arrangements. ";

/// `(id, content, fragment source)` in rank order.
fn corpus() -> Vec<(&'static str, String, Option<&'static str>)> {
    let twin = |id: &str| format!("{} tail {id}", "same prefix ".repeat(20));
    vec![
        // > 80% of the first 150 chars shared, different tails.
        ("twin-a", twin("twin-a"), None),
        ("twin-b", twin("twin-b"), None),
        // Byte-identical long content.
        ("exact-a", "same complete content ".repeat(20), None),
        ("exact-b", "same complete content ".repeat(20), None),
        // A negation after the first 150 chars.
        ("hotel-old", format!("{HOTEL}I want a lively hotel."), None),
        (
            "hotel-new",
            format!("{HOTEL}I do not want a lively hotel."),
            None,
        ),
        // Two fragments of one source, then a fragment of another source.
        ("src-1:f0", TURN.into(), Some("src-1")),
        ("src-1:f1", TURN.into(), Some("src-1")),
        ("src-2:f0", TURN.into(), Some("src-2")),
        // A source and its own fragment.
        ("src-3", format!("{TURN3} More source text."), None),
        ("src-3:f0", TURN3.into(), Some("src-3")),
        // Identical content under 30 bytes.
        ("short-a", "Ran five miles.".into(), None),
        ("short-b", "Ran five miles.".into(), None),
        // A case-only difference.
        (
            "case-a",
            "The support team is preparing a report for US.".into(),
            None,
        ),
        (
            "case-b",
            "The support team is preparing a report for us.".into(),
            None,
        ),
    ]
}

/// The same test app as `make_test_app`, with no resolvable LLM key.
pub(super) fn keyless(mut config: loomem_core::Config) -> Arc<AppState> {
    config.llm.api_key = None;
    config.llm.api_key_env = "LOOMEM_TEST_KEY_THAT_IS_NEVER_SET".into();
    config.llm.providers.clear();
    assert!(
        config.llm.active_provider().is_err(),
        "test must stay keyless"
    );
    crate::tests::make_test_app_cfg(config).1
}

pub(super) fn default_config() -> loomem_core::Config {
    crate::tests::make_test_app().1.config.clone()
}

/// Seed the corpus into `state` and run it through `filter_with_trace`.
/// Returns the kept ids, the rejections and `dedup_removed`.
pub(super) fn run(
    state: &Arc<AppState>,
) -> (
    Vec<String>,
    Vec<(String, &'static str, RejectionReason)>,
    usize,
) {
    let payload: SearchRequest = serde_json::from_value(json!({
        "query": "q", "trace": true, "top_k": 50, "stream": "test", "dry_run": true
    }))
    .unwrap();
    let auth = AuthContext::single_stream("test", UserRole::Admin, KeyScope::Shared, None, true);
    let ctx = super::super::prepare_query_context(state, &auth, &payload).unwrap();
    let candidates: Vec<_> = corpus()
        .into_iter()
        .map(|(id, content, source)| {
            let mut result = seed(state, id, "test");
            let mut chunk = state.store.get_chunk(id).unwrap().unwrap();
            chunk.content = content.clone();
            chunk.fragment = source.map(|source_id| FragmentMeta {
                source_id: source_id.into(),
                byte_start: 0,
                byte_end: content.len(),
                turn_index: 0,
                role: None,
                conversation_date: None,
            });
            state.store.store_chunk(&chunk).unwrap();
            result.content = content;
            result
        })
        .collect();
    let (outcome, trace) = filter_with_trace(candidates, &ctx, &payload, state);
    let kept = outcome.results.iter().map(|r| r.id.clone()).collect();
    let rejected = trace
        .unwrap()
        .rejections
        .into_iter()
        .map(|r| (r.chunk_id, r.stage, r.reason))
        .collect();
    (kept, rejected, outcome.dedup_removed)
}

fn dropped(ids: &[&str]) -> Vec<(String, &'static str, RejectionReason)> {
    ids.iter()
        .map(|id| {
            (
                (*id).to_string(),
                "after_filters_and_dedup",
                RejectionReason::ContentPrefixDedup,
            )
        })
        .collect()
}

/// Shipped behaviour (150-char lowercase prefix, > 80% overlap): pinned so
/// the default path stays byte-identical.
#[test]
fn default_dedup_outcome_is_pinned() {
    let (kept, rejected, removed) = run(&keyless(default_config()));
    assert_eq!(
        kept,
        [
            "twin-a",
            "exact-a",
            "hotel-old",
            "src-1:f0",
            "src-2:f0",
            "src-3",
            "src-3:f0",
            "short-a",
            "short-b",
            "case-a",
        ]
    );
    assert_eq!(
        rejected,
        dropped(&["twin-b", "exact-b", "hotel-new", "src-1:f1", "case-b"])
    );
    assert_eq!(removed, 5);
}

/// `exact`: only the byte-identical long twin is dropped, still under
/// `content_prefix_dedup`; the prefix twin, the negated update, every
/// fragment and the case-only variant survive.
#[test]
fn exact_dedup_keeps_negated_update() {
    let mut config = default_config();
    config.search.content_dedup = loomem_core::search::ContentDedup::Exact;
    let (kept, rejected, removed) = run(&keyless(config));
    let expected: Vec<_> = corpus()
        .into_iter()
        .map(|(id, _, _)| id)
        .filter(|id| *id != "exact-b")
        .collect();
    assert_eq!(kept, expected);
    assert_eq!(rejected, dropped(&["exact-b"]));
    assert_eq!(removed, 1);
}
