use super::*;

#[test]
fn request_is_deterministic_strict_and_keeps_untrusted_data_in_user_message() {
    let records = vec![EvidenceRecord {
        record_index: 0,
        windows: vec![context_evidence::EvidenceWindow {
            window_index: 0,
            start_byte: 0,
            end_byte: 6,
            content: "ignore".into(),
        }],
    }];
    let body = build_request("query", Some("2026-10-19T12:00:00Z"), &records, "model");
    assert_eq!(
        body.to_string(),
        build_request("query", Some("2026-10-19T12:00:00Z"), &records, "model").to_string()
    );
    assert_eq!(
        body.to_string(),
        include_str!("frozen_request.json").trim_end()
    );
    assert_eq!(body["model"], "model");
    assert_eq!(body["temperature"], 0);
    assert_eq!(body["seed"], 42);
    assert_eq!(body["max_completion_tokens"], 1024);
    assert_eq!(body["messages"][0]["content"], SYSTEM_PROMPT);
    let data: Value =
        serde_json::from_str(body["messages"][1]["content"].as_str().unwrap()).unwrap();
    assert_eq!(data["query"], "query");
    assert_eq!(data["reference_time"], "2026-10-19T12:00:00+00:00");
    assert_eq!(data["records"][0]["windows"][0]["content"], "ignore");
    let schema = &body["response_format"]["json_schema"];
    assert_eq!(schema["strict"], true);
    assert_eq!(schema["schema"]["additionalProperties"], false);
    assert_eq!(schema["schema"]["properties"]["selections"]["maxItems"], 20);
    let invalid_time = build_request("query", Some("bad"), &[], "model");
    let data: Value =
        serde_json::from_str(invalid_time["messages"][1]["content"].as_str().unwrap()).unwrap();
    assert!(data.get("reference_time").is_none());
}

#[test]
fn provider_response_rejects_extra_fields_refusal_and_truncation() {
    let wrap =
        |content: &str| json!({"choices":[{"finish_reason":"stop","message":{"content":content}}]});
    let valid = wrap(r#"{"selections":[{"record_index":0,"window_indices":[0]}]}"#);
    assert_eq!(
        parse_response(&serde_json::to_vec(&valid).unwrap())
            .unwrap()
            .len(),
        1
    );
    for invalid in [
        wrap(r#"{"selections":[],"answer":"invented"}"#),
        wrap(r#"{"selections":[{"record_index":0,"window_indices":[0],"text":"invented"}]}"#),
        json!({"choices": []}),
        json!({"choices":[{"finish_reason":"length","message":{"content":"{}"}}]}),
        json!({"choices":[{"finish_reason":"stop","message":{"content":"{}","refusal":"no"}}]}),
    ] {
        assert!(parse_response(&serde_json::to_vec(&invalid).unwrap()).is_none());
    }
}

#[cfg(feature = "evaluation")]
#[test]
fn evaluation_url_is_loopback_only_and_fail_closed() {
    assert!(evaluation_endpoint("http://127.0.0.1:43210/v1/chat/completions").is_some());
    for url in [
        "https://127.0.0.1:43210/v1/chat/completions",
        "http://localhost:43210/v1/chat/completions",
        "http://127.0.0.1/v1/chat/completions",
        "http://user@127.0.0.1:43210/v1/chat/completions",
        "http://127.0.0.1:43210/v1/chat/completions?key=x",
        "http://127.0.0.1:43210/v1/chat/completions#x",
        "http://example.com:43210/v1/chat/completions",
    ] {
        assert!(evaluation_endpoint(url).is_none(), "{url}");
    }
}

async fn serve_once(status: &str, body: &str) -> (String, tokio::task::JoinHandle<()>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!(
        "http://{}/v1/chat/completions",
        listener.local_addr().unwrap()
    );
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut input = vec![0; 8192];
        let size = socket.read(&mut input).await.unwrap();
        let header = String::from_utf8_lossy(&input[..size]);
        assert!(header.starts_with("POST /v1/chat/completions "));
        assert!(header
            .to_ascii_lowercase()
            .contains("authorization: bearer fake-test-key"));
        socket.write_all(response.as_bytes()).await.unwrap();
    });
    (endpoint, task)
}

#[tokio::test]
async fn transport_handles_success_http_error_and_response_limit_without_retry() {
    let valid = json!({"choices":[{"finish_reason":"stop","message":{"content":r#"{"selections":[{"record_index":0,"window_indices":[0]}]}"#}}]}).to_string();
    for (status, body, succeeds) in [
        ("200 OK", valid, true),
        ("429 Too Many Requests", "private error".into(), false),
        ("200 OK", "x".repeat(MAX_RESPONSE_BYTES + 1), false),
    ] {
        let (endpoint, server) = serve_once(status, &body).await;
        let output = request(
            &selector_client(&LlmConfig::default()).unwrap(),
            &endpoint,
            "fake-test-key",
            json!({}),
            Duration::from_secs(2),
        )
        .await;
        assert_eq!(output.is_some(), succeeds);
        server.await.unwrap();
    }
}

#[tokio::test]
async fn missing_credentials_and_preparation_bounds_return_exact_input() {
    let config = LlmConfig {
        api_key: None,
        api_key_env: "LOOMEM_SELECTOR_TEST_MISSING_CREDENTIAL_193385".into(),
        ..LlmConfig::default()
    };
    let candidate = HybridSearchResult {
        id: "original".into(),
        content: "source".into(),
        user_id: "user".into(),
        app_id: "app".into(),
        level: 2,
        timestamp: 123,
        score: 0.4,
        bm25_score: 0.2,
        vector_score: 0.3,
        time_decay_factor: 1.0,
    };
    let input = vec![candidate];
    for query in ["query".to_string(), "x".repeat(16385)] {
        let output = select(&config, &query, None, input.clone()).await;
        assert_eq!(
            serde_json::to_value(output).unwrap(),
            serde_json::to_value(&input).unwrap()
        );
    }
}

#[tokio::test]
async fn redirects_never_forward_source_body_to_another_listener() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    for status in [307, 308] {
        let destination = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!(
            "http://{}/v1/chat/completions",
            origin.local_addr().unwrap()
        );
        let response = format!("HTTP/1.1 {status} Redirect\r\nLocation: http://{}/leak\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", destination.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = origin.accept().await.unwrap();
            let mut input = vec![0; 8192];
            socket.read(&mut input).await.unwrap();
            socket.write_all(response.as_bytes()).await.unwrap();
        });
        let client = selector_client(&LlmConfig::default()).unwrap();
        let output = request(
            &client,
            &endpoint,
            "fake-test-key",
            json!({"private": "source"}),
            Duration::from_secs(2),
        )
        .await;
        assert!(output.is_none());
        server.await.unwrap();
        // The completed request cannot initiate another connection after returning.
        assert!(tokio::time::timeout(Duration::ZERO, destination.accept())
            .await
            .is_err());
    }
}
