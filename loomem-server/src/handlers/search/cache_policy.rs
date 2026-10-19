//! Cache identity and safe ordering for the search response pipeline.
use super::{DateFilter, SearchRequest};
use loomem_core::query_cache::{QueryCache, QueryCacheConfig};
use loomem_core::HybridSearchResult;
use serde_json::json;
use std::collections::HashSet;

pub(super) fn cache_key(
    config: &QueryCacheConfig,
    payload: &SearchRequest,
    streams: Option<&[String]>,
    date_filter: Option<&DateFilter>,
    limit: usize,
    operation_plan_identity: Option<&str>,
) -> Option<u64> {
    // These responses need fresh pipeline metadata or explicit side-effect control.
    if !config.enabled
        || payload.trace
        || payload.debug_channels
        || payload.debug_signal_breakdown
        || payload.debug_operation
        || payload.include_associations
        || payload.dry_run
    {
        return None;
    }
    let dates = date_filter.map(|DateFilter::Range(start, end)| (*start, *end));
    let mut identity = json!({
        "query": payload.query,
        "dates": dates,
        "valid_at": payload.valid_at,
        "include_superseded": payload.include_superseded,
        "fact_type": payload.fact_type,
        "subject": payload.subject,
        "min_confidence": payload.min_confidence,
        "filters": payload.filters,
        "requested_top_k": payload.top_k,
        "user_id": payload.user_id,
    });
    if operation_plan_identity.is_some() {
        identity["operation_plan"] = json!(operation_plan_identity);
    }
    Some(QueryCache::hash_query_with_source(
        &identity.to_string(),
        streams,
        payload.entity.as_deref(),
        payload.date_from.as_deref(),
        payload.date_to.as_deref(),
        limit,
        payload.source_agent.as_deref(),
        payload.exclude_source_agents.as_deref(),
    ))
}

/// Reranking changes order only: preserve eligible membership and cardinality.
pub(super) fn reranked_results(
    indices: Vec<usize>,
    candidates: &[HybridSearchResult],
    eligible_results: &[HybridSearchResult],
    top_k: usize,
) -> Vec<HybridSearchResult> {
    let mut seen = HashSet::new();
    indices
        .into_iter()
        .filter_map(|index| candidates.get(index))
        .chain(eligible_results.iter())
        .filter(|candidate| seen.insert(candidate.id.clone()))
        .take(top_k)
        .cloned()
        .collect()
}

pub(super) fn truncate(mut outcome: super::FilterOutcome) -> super::FilterOutcome {
    outcome.results.truncate(outcome.final_top_k);
    outcome
}

pub(super) async fn publish_reranked(
    cache: &tokio::sync::Mutex<QueryCache>,
    identity: (u64, u64),
    indices: Vec<usize>,
    candidates: &[HybridSearchResult],
    eligible_results: &[HybridSearchResult],
    top_k: usize,
) {
    let reranked = reranked_results(indices, candidates, eligible_results, top_k);
    if reranked.is_empty() {
        return;
    }
    let count = reranked.len();
    let published = cache
        .lock()
        .await
        .insert_reranked_if_current(identity.0, identity.1, reranked);
    tracing::info!(
        published,
        "Async rerank complete: {} → {} results",
        candidates.len(),
        count
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(value: serde_json::Value) -> SearchRequest {
        serde_json::from_value(value).expect("valid request fixture")
    }

    #[test]
    fn cache_identity_covers_filters_and_raw_query() {
        let config = QueryCacheConfig::default();
        let base = cache_key(
            &config,
            &request(json!({"query": "apple"})),
            None,
            None,
            10,
            Some("plain"),
        );
        for extra in [
            json!({"valid_at": 100}),
            json!({"fact_type": "event"}),
            json!({"subject": "alice"}),
            json!({"min_confidence": 0.8}),
            json!({"include_superseded": true}),
            json!({"top_k": 10}),
            json!({"query": "apples"}),
        ] {
            let mut value = json!({"query": "apple"});
            value
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            assert_ne!(
                base,
                cache_key(&config, &request(value), None, None, 10, Some("plain"))
            );
        }
        assert_ne!(
            base,
            cache_key(
                &config,
                &request(json!({"query": "apple"})),
                None,
                Some(&DateFilter::Range(0, 100)),
                10,
                Some("plain")
            )
        );
        assert_ne!(
            base,
            cache_key(
                &config,
                &request(json!({"query": "apple"})),
                None,
                None,
                10,
                Some("preference")
            )
        );
    }

    #[test]
    fn raw_reference_time_never_creates_cache_churn() {
        let config = QueryCacheConfig::default();
        let first = request(json!({
            "query": "apple",
            "reference_time": "invalid-a",
            "operation_policy": "E1-F"
        }));
        let second = request(json!({
            "query": "apple",
            "reference_time": "invalid-b",
            "operation_policy": "E1-F"
        }));
        assert_eq!(
            cache_key(&config, &first, None, None, 10, Some("E1-F|none|plan")),
            cache_key(&config, &second, None, None, 10, Some("E1-F|none|plan"))
        );
    }

    #[test]
    fn ineffective_operation_fields_preserve_e0_identity() {
        let config = QueryCacheConfig::default();
        let baseline = request(json!({"query": "apple"}));
        let disabled = request(json!({
            "query": "apple",
            "reference_time": "2026-10-19T12:00:00Z",
            "operation_policy": "E1-C"
        }));
        assert_eq!(
            cache_key(&config, &baseline, None, None, 10, None),
            cache_key(&config, &disabled, None, None, 10, None)
        );
    }

    #[test]
    fn fresh_response_flags_bypass_cache() {
        for flag in [
            "trace",
            "debug_channels",
            "debug_signal_breakdown",
            "debug_operation",
            "include_associations",
            "dry_run",
        ] {
            let mut value = json!({"query": "apple"});
            value[flag] = json!(true);
            assert!(cache_key(
                &QueryCacheConfig::default(),
                &request(value),
                None,
                None,
                10,
                Some("plain")
            )
            .is_none());
        }
    }
}
