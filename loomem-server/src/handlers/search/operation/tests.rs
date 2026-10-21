use super::*;
use loomem_core::search::{OperationPlan, QueryOperation};

fn candidate(index: usize, content: &str) -> HybridSearchResult {
    HybridSearchResult {
        id: format!("r{index:03}"),
        content: content.to_string(),
        user_id: String::new(),
        app_id: String::new(),
        level: 0,
        timestamp: 0,
        score: 1.0,
        bm25_score: 1.0,
        vector_score: 0.0,
        time_decay_factor: 1.0,
    }
}

fn plan() -> OperationPlan {
    OperationPlan {
        operation: QueryOperation::PreferenceAdvice {
            topic: "guitar".into(),
        },
        facets: vec!["guitar".into()],
        added_terms: vec!["prefer".into(), "enjoy".into()],
    }
}

fn prepared_plan() -> PreparedOperation {
    PreparedOperation {
        policy: OperationPolicy::E1F,
        plan: plan(),
        reference_time: None,
        fallback: None,
        e2_lane: None,
        e2_fallback: None,
        explicit_range: None,
    }
}

#[test]
fn protects_top_twenty_and_changes_at_most_two() {
    let input: Vec<_> = (0..80)
        .map(|index| candidate(index, if index >= 35 { "prefer enjoy" } else { "plain" }))
        .collect();
    let baseline: Vec<_> = input.iter().map(|item| item.id.clone()).collect();
    let (ranked, metrics) = apply(input, &prepared_plan(), true);
    assert_eq!(
        ranked[..20].iter().map(|item| &item.id).collect::<Vec<_>>(),
        baseline[..20].iter().collect::<Vec<_>>()
    );
    assert_eq!(metrics.changed_admissions, 2);
    assert_eq!(&ranked[20].id, "r035");
    assert_eq!(&ranked[21].id, "r036");
}

#[test]
fn repetition_does_not_change_priority() {
    let mut once: Vec<_> = (0..60).map(|index| candidate(index, "plain")).collect();
    once[40].content = "prefer enjoy".into();
    once[41].content = "prefer enjoy".repeat(100);
    let (ranked, _) = rerank(once, &plan());
    assert_eq!(&ranked[20].id, "r040");
    assert_eq!(&ranked[21].id, "r041");
}

#[test]
fn copied_query_flood_is_saturated_at_two() {
    let input: Vec<_> = (0..60)
        .map(|index| candidate(index, if index >= 35 { "prefer" } else { "plain" }))
        .collect();
    let (_, metrics) = rerank(input, &plan());
    assert_eq!(metrics.changed_admissions, 2);
    assert_eq!(metrics.matched_candidates, 25);
}

#[test]
fn request_scan_overflow_fails_open_byte_identical() {
    let input: Vec<_> = (0..40)
        .map(|index| candidate(index, &"x".repeat(MAX_RECORD_SCAN_BYTES)))
        .collect();
    let ids: Vec<_> = input.iter().map(|item| item.id.clone()).collect();
    let (ranked, metrics) = apply(input, &prepared_plan(), true);
    assert_eq!(
        ranked
            .iter()
            .map(|item| item.id.clone())
            .collect::<Vec<_>>(),
        ids
    );
    assert_eq!(metrics.fallback, Some("scan_limit"));
    assert_eq!(metrics.scanned_bytes, MAX_REQUEST_SCAN_BYTES);
    assert_eq!(metrics.scanned_candidates, 32);
    assert!(metrics.temporary_bytes > 0);
    assert!(metrics.temporary_bytes <= 1024 * 1024);
}

#[test]
fn temporary_memory_bound_is_monotonic_and_below_budget() {
    let small = (0..25)
        .map(|index| candidate(index, "prefer enjoy"))
        .collect();
    let bounded_content = format!("prefer enjoy {}", "x".repeat(2_880));
    let large = (0..MAX_CANDIDATES)
        .map(|index| candidate(index, &bounded_content))
        .collect();
    let small_metrics = apply(small, &prepared_plan(), true).1;
    let large_metrics = apply(large, &prepared_plan(), true).1;
    assert_eq!(large_metrics.scanned_candidates, MAX_CANDIDATES);
    assert!(large_metrics.scanned_bytes <= MAX_REQUEST_SCAN_BYTES);
    assert!(large_metrics.temporary_bytes >= small_metrics.temporary_bytes);
    assert!(large_metrics.temporary_bytes <= 1024 * 1024);
}

#[test]
fn operation_stage_has_no_bpe_or_context_packing_boundary() {
    let server_source = include_str!("../operation.rs")
        .split("#[cfg(test)]")
        .next()
        .unwrap_or_default();
    let core_source = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../loomem-core/src/search/operation.rs"
    ));
    for forbidden in ["tiktoken", "count_text", "pack_context", "tokenizer"] {
        assert!(!server_source.contains(forbidden), "{forbidden}");
        assert!(!core_source.contains(forbidden), "{forbidden}");
    }
}

#[test]
fn candidate_limit_fails_open_byte_identical() {
    let input: Vec<_> = (0..=MAX_CANDIDATES)
        .map(|index| candidate(index, "prefer"))
        .collect();
    let ids: Vec<_> = input.iter().map(|item| item.id.clone()).collect();
    let prepared = prepared_plan();
    let (ranked, metrics) = apply(input, &prepared, true);
    assert_eq!(
        ranked
            .iter()
            .map(|item| item.id.clone())
            .collect::<Vec<_>>(),
        ids
    );
    assert_eq!(metrics.fallback, Some("candidate_limit"));
    assert!(metrics.temporary_bytes > 0);
}

#[test]
fn telemetry_has_base_memory_without_scan_or_when_disabled() {
    let no_scan = PreparedOperation {
        policy: OperationPolicy::E1F,
        plan: OperationPlan::plain(),
        reference_time: None,
        fallback: None,
        e2_lane: None,
        e2_fallback: None,
        explicit_range: None,
    };
    let no_scan_metrics = apply(Vec::new(), &no_scan, true).1;
    assert!(no_scan_metrics.temporary_bytes >= 64);
    assert_eq!(no_scan_metrics.fallback, None);

    let disabled_metrics = apply(
        Vec::new(),
        &PreparedOperation::baseline(Some("disabled")),
        true,
    )
    .1;
    assert!(disabled_metrics.temporary_bytes >= 64);
    assert_eq!(disabled_metrics.fallback, Some("disabled"));

    let non_debug_metrics = apply(Vec::new(), &prepared_plan(), false).1;
    assert_eq!(non_debug_metrics.temporary_bytes, 0);
}

fn valid_gate() -> GateState {
    GateState {
        operation_enabled: true,
        configured_top_k: 10,
        multi_query: false,
        rare_lane: false,
        multi_stream: false,
        downstream_rerank: false,
        requested_top_k: 60,
        query_bytes: 32,
    }
}

#[test]
fn request_and_candidate_budgets_are_fail_closed() {
    assert_eq!(gate_reason(valid_gate()), None);
    let mut too_many_returned = valid_gate();
    too_many_returned.requested_top_k = 61;
    assert_eq!(gate_reason(too_many_returned), Some("returned_limit"));
    let mut too_many_candidates = valid_gate();
    too_many_candidates.configured_top_k = (MAX_CANDIDATES / 3) + 1;
    assert_eq!(gate_reason(too_many_candidates), Some("returned_limit"));
}

#[test]
fn extra_retrieval_leaves_disable_operation_path() {
    let mutators: [fn(&mut GateState); 3] = [
        |state: &mut GateState| state.multi_query = true,
        |state: &mut GateState| state.rare_lane = true,
        |state: &mut GateState| state.multi_stream = true,
    ];
    for mutate in mutators {
        let mut state = valid_gate();
        mutate(&mut state);
        assert!(gate_reason(state).is_some());
    }
}

#[test]
fn oversized_query_disables_date_and_combined_cells() {
    for policy in [OperationPolicy::E1D, OperationPolicy::E1C] {
        let mut state = valid_gate();
        state.query_bytes = MAX_QUERY_BYTES + 1;
        assert_eq!(gate_reason(state), Some("query_limit"), "{policy:?}");
    }
}

#[test]
fn rerank_is_deterministic_and_nonmutating() {
    let input: Vec<_> = (0..60)
        .map(|index| candidate(index, if index == 45 { "prefer" } else { "plain" }))
        .collect();
    let first = rerank(input.clone(), &plan()).0;
    let second = rerank(input.clone(), &plan()).0;
    assert_eq!(
        first.iter().map(|item| &item.id).collect::<Vec<_>>(),
        second.iter().map(|item| &item.id).collect::<Vec<_>>()
    );
    let originals: HashMap<_, _> = input.iter().map(|item| (&item.id, &item.content)).collect();
    assert!(first
        .iter()
        .all(|item| originals.get(&item.id) == Some(&&item.content)));
}
