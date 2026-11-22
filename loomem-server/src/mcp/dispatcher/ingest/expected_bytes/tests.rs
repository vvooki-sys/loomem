use super::*;
use loomem_core::fragments::TokenCounterKind;

fn fixture() -> std::sync::Arc<AppState> {
    let (_, original) = crate::tests::make_test_app();
    let mut config = original.config.clone();
    config.fragments.enabled = true;
    config.fragments.token_counter = TokenCounterKind::Approx;
    crate::tests::make_test_app_cfg(config).1
}

fn request(content: &str, ids: &[&str]) -> SourceExpectation {
    SourceExpectation {
        content: content.into(),
        conversation_date: Some("2026-10-01".into()),
        source_id: "00000000-0000-4000-8000-000000000001".into(),
        fragment_ids: ids
            .iter()
            .map(|id| format!("00000000-0000-4000-8000-000000000001:{id}"))
            .collect(),
    }
}

#[test]
fn ascii_unicode_and_html_have_hand_authored_bytes_and_ranges() {
    let state = fixture();
    for (raw, canonical, ranges) in [
        (
            "user: red\nassistant: blue",
            "user: red\nassistant: blue",
            vec![(0, 10), (10, 25)],
        ),
        (
            "user: żółć\nassistant: 🍋",
            "user: żółć\nassistant: 🍋",
            vec![(0, 15), (15, 30)],
        ),
        (
            "  <b>user: red</b> &amp; blue  ",
            "user: red & blue",
            vec![(0, 16)],
        ),
        (
            "user: is 1 < 2?\nassistant: yes, 1 < 2\nuser: thanks",
            "user: is 1 < 2?\nassistant: yes, 1 < 2\nuser: thanks",
            vec![(0, 15), (16, 37), (38, 50)],
        ),
    ] {
        let ids: Vec<String> = (0..ranges.len()).map(|i| format!("f{i}")).collect();
        let req = request(raw, &ids.iter().map(String::as_str).collect::<Vec<_>>());
        let records = reconstruct(&state, &req).unwrap();
        assert_eq!(records[0].content, canonical);
        assert_eq!(records.len(), ranges.len() + 1);
        for (record, (start, end)) in records.iter().skip(1).zip(ranges) {
            let meta = record.fragment.as_ref().unwrap();
            assert_eq!((meta.byte_start, meta.byte_end), (start, end));
            assert_eq!(record.content, &canonical[start..end]);
            assert_eq!(meta.conversation_date.as_deref(), Some("2026-10-01"));
        }
    }
}

#[test]
fn reject_omitted_extra_duplicate_foreign_and_sparse_receipt_ids() {
    let state = fixture();
    for ids in [
        vec![],
        vec!["f0"],
        vec!["f0", "f1", "f2"],
        vec!["f0", "f0"],
        vec!["f0", "f2"],
    ] {
        assert!(reconstruct(&state, &request("user: red\nassistant: blue", &ids)).is_err());
    }
    let mut invalid_source = request("user: red", &["f0"]);
    invalid_source.source_id = "not-a-uuid".into();
    assert!(reconstruct(&state, &invalid_source).is_err());
    let mut foreign = request("user: red", &["f0"]);
    foreign.fragment_ids[0] = "foreign:f0".into();
    assert!(reconstruct(&state, &foreign).is_err());
}

#[test]
fn reject_disabled_missing_tokenizer_invalid_date_and_unstable_source() {
    let (_, state) = crate::tests::make_test_app();
    assert!(reconstruct(&state, &request("user: red", &["f0"])).is_err());
    let mut cfg = state.config.clone();
    cfg.fragments.enabled = true;
    let (_, no_model) = crate::tests::make_test_app_cfg(cfg);
    assert!(reconstruct(&no_model, &request("user: red", &["f0"])).is_err());
    let state = fixture();
    let mut bad_date = request("user: red", &["f0"]);
    bad_date.conversation_date = Some("2026-1-1".into());
    assert!(reconstruct(&state, &bad_date).is_err());
    assert!(reconstruct(
        &state,
        &request("&amp;amp;amp;amp;amp;lt;b&gt;red", &["f0"])
    )
    .is_err());
}

#[tokio::test]
async fn exact_metadata_matches_real_ingest_and_rejects_range_and_marker_drift() {
    let state = fixture();
    let raw = "user: red\nassistant: blue";
    let result = super::super::tool_ingest(
        &state,
        serde_json::json!({
            "content":raw,"conversation_date":"2026-10-01"
        }),
        "synthetic",
        None,
    )
    .await
    .unwrap();
    let receipt = &result.structured_content.unwrap()["ingest_receipt"];
    let req = SourceExpectation {
        content: raw.into(),
        conversation_date: Some("2026-10-01".into()),
        source_id: receipt["source_id"].as_str().unwrap().into(),
        fragment_ids: serde_json::from_value(receipt["fragment_ids"].clone()).unwrap(),
    };
    for record in reconstruct(&state, &req).unwrap() {
        let mut chunk = state.store.get_chunk(&record.id).unwrap().unwrap();
        assert!(metadata_matches(
            &record,
            &chunk,
            req.conversation_date.as_deref()
        ));
        if let Some(meta) = chunk.fragment.as_mut() {
            meta.byte_end += 1;
        } else {
            chunk.metadata = None;
        }
        assert!(!metadata_matches(
            &record,
            &chunk,
            req.conversation_date.as_deref()
        ));
    }
}
