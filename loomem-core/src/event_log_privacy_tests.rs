use super::*;

#[test]
fn search_query_never_serializes_and_legacy_events_remain_readable() {
    let legacy = r#"{"type":"search","query":"private-canary@example.invalid","stream_id":"test","top_scores":[],"latency_ms":1,"result_count":0}"#;
    let event: MemoryEvent = serde_json::from_str(legacy).unwrap();
    let serialized = serde_json::to_string(&event).unwrap();
    assert!(!serialized.contains("private-canary"));
    assert!(!serialized.contains("query"));
    let parsed: MemoryEvent = serde_json::from_str(&serialized).unwrap();
    match parsed {
        MemoryEvent::Search {
            query,
            result_count,
            ..
        } => {
            assert!(query.is_empty());
            assert_eq!(result_count, 0);
        }
        _ => panic!("expected search event"),
    }
}

#[test]
fn persisted_search_event_omits_query_canary() {
    let dir = tempfile::tempdir().unwrap();
    let config = EventLogConfig {
        enabled: true,
        ..EventLogConfig::default()
    };
    let mut log = EventLog::open(dir.path(), &config).unwrap();
    let event = MemoryEvent::Search {
        query: "private-canary@example.invalid".into(),
        stream_id: "test".into(),
        top_scores: vec![],
        latency_ms: 1,
        result_count: 0,
    };
    log.append(&EventEntry {
        timestamp: 1,
        event,
    })
    .unwrap();
    log.flush().unwrap();
    let persisted = std::fs::read_to_string(dir.path().join("events/events.jsonl")).unwrap();
    assert!(!persisted.contains("private-canary"));
    assert!(!persisted.contains("query"));
    assert!(persisted.contains("search"));
}
