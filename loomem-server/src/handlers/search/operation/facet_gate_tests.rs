use super::*;
use chrono::TimeZone;

fn prepared(query: &str, policy: OperationPolicy, anchor: Option<&str>) -> PreparedOperation {
    let payload: SearchRequest = serde_json::from_value(serde_json::json!({
        "query": query, "operation_policy": policy, "reference_time": anchor,
    }))
    .unwrap();
    let reference_time = normalized_reference_time(&payload);
    let mut prepared = PreparedOperation {
        policy,
        plan: loomem_core::search::plan_operation(query, reference_time, policy),
        reference_time,
        fallback: None,
        e2_lane: None,
        e2_fallback: None,
        explicit_range: None,
    };
    prepare_e2_lane(&payload, &mut prepared);
    prepared
}

#[test]
fn combined_without_date_uses_same_facet_query_as_facet_arm() {
    for query in [
        "Recommend a guitar",
        "Which happened first, launch or move?",
        "List all 3 trips in chronological order",
        "Poleć gitarę",
    ] {
        for anchor in [None, Some("invalid"), Some("2023-05-30T12:00:00Z")] {
            let combined = prepared(query, OperationPolicy::E2C, anchor);
            let facet = prepared(query, OperationPolicy::E2F, anchor);
            let combined_lane = combined.e2_lane_spec().expect(query);
            assert_eq!(combined_lane.query, facet.e2_lane_spec().unwrap().query);
            assert_eq!(combined_lane.inclusive_range, None);
            assert_eq!(combined.e2_fallback_reason(), None);
        }
        assert!(prepared(query, OperationPolicy::E2D, None)
            .e2_lane_spec()
            .is_none());
    }
}

#[test]
fn failed_or_ambiguous_date_never_bypasses_combined_date_gate() {
    for query in [
        "Recommend a guitar last month",
        "Recommend a guitar yesterday",
        "List all trips 2 weeks ago",
        "Recommend a guitar last month and yesterday",
        "Recommend a guitar last month and next year",
        "Poleć gitarę 3 dni temu",
        "Recommend a guitar on 2023-04-01",
        "Recommend a guitar in 2023",
        "Recommend a guitar on April 12",
        "Recommend a guitar in May",
        "List all trips past 2 weeks",
        "Recommend a guitar between April and June",
        "Poleć gitarę w zeszłym tygodniu",
    ] {
        for anchor in [None, Some("invalid")] {
            let combined = prepared(query, OperationPolicy::E2C, anchor);
            assert!(combined.e2_lane_spec().is_none(), "{query}");
            assert_eq!(combined.e2_fallback_reason(), Some("no_reference_time"));
        }
    }
    for query in [
        "Recommend a guitar yesterday",
        "Recommend a guitar last month and yesterday",
        "Recommend a guitar last month and next year",
        "List all trips 2 weeks ago",
        "Recommend a guitar on 2023-04-01",
        "Recommend a guitar in 2023",
        "Recommend a guitar on April 12",
        "Recommend a guitar in May",
        "List all trips past 2 weeks",
        "Recommend a guitar between April and June",
        "Poleć gitarę w zeszłym tygodniu",
    ] {
        let combined = prepared(query, OperationPolicy::E2C, Some("2023-05-30T12:00:00Z"));
        assert!(combined.e2_lane_spec().is_none(), "{query}");
        assert_eq!(
            combined.e2_fallback_reason(),
            Some("unsupported_date_expression")
        );
    }
}

#[test]
fn calendar_combined_keeps_range_and_plain_combined_stays_inactive() {
    let calendar = prepared(
        "What happened last month?",
        OperationPolicy::E2C,
        Some("2023-05-30T12:00:00Z"),
    );
    assert_eq!(
        calendar.e2_lane_spec().unwrap().inclusive_range,
        Some((1_680_307_200, 1_682_899_199))
    );
    assert!(
        prepared("Where is the invoice?", OperationPolicy::E2C, None)
            .e2_lane_spec()
            .is_none()
    );
    assert!(calendar.e2_cache_identity().contains("e3-two-lane-v1"));
}

#[test]
fn cache_identity_uses_effective_dates_instead_of_anchor_text() {
    let make = |hour| PreparedOperation {
        policy: OperationPolicy::E1D,
        plan: OperationPlan::plain(),
        reference_time: Utc.with_ymd_and_hms(2026, 10, 19, hour, 0, 0).single(),
        fallback: None,
        e2_lane: None,
        e2_fallback: None,
        explicit_range: None,
    };
    let date = PreparedDate {
        query: "yesterday".into(),
        filter: Some(DateFilter::Range(1, 2)),
        anchored: true,
    };
    assert_eq!(
        make(1).cache_identity(&date),
        make(23).cache_identity(&date)
    );
}
