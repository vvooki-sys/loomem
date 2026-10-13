use super::*;

#[test]
fn structured_response_preserves_event_metadata_with_json_fence() {
    let json = r#"{"observations":[{"type":"event","content":"Launch on 2024-01-02","event_at_raw":"2024-01-02","confidence":0.9,"importance":"high"}]}"#;
    for response in [
        json.to_string(),
        format!("```json\n{json}\n```"),
        format!("```\n{json}\n```"),
    ] {
        let parsed = parse_structured_response(&response, 1_704_067_200, "test")
            .expect("structured response must parse with or without a fence");
        assert_eq!(
            parsed.extraction_meta.event_date.as_deref(),
            Some("2024-01-02")
        );
        assert_eq!(parsed.importance, Some(1.0));
        assert!(parsed.content.contains("Launch on 2024-01-02"));
        assert!(!parsed.content.contains("```"));
    }
}

#[test]
fn consolidation_excludes_superseded_and_deleted_sources() {
    let mut chunk: Chunk = serde_json::from_value(serde_json::json!({
        "id": "L0:a", "content": "Old preference", "stream": "s", "level": 0,
        "score": 1.0, "timestamp": 1000, "consolidated": false,
        "dormant": false, "in_progress": false
    }))
    .expect("test chunk");
    assert!(is_consolidation_source(&chunk));
    chunk.is_latest = false;
    assert!(!is_consolidation_source(&chunk));
    chunk.is_latest = true;
    chunk.deleted_at = Some(2000);
    assert!(!is_consolidation_source(&chunk));
}
