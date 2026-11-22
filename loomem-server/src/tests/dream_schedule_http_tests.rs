//! HTTP-level tests for `/v1/dream/schedule*`. Included from `main.rs`
//! `mod tests` via `#[path]`, so `super::*` is that module: `make_test_app`,
//! `make_test_app_cfg`, `test_config`, `send`, and the axum/http imports.

use super::*;
use std::sync::atomic::Ordering::SeqCst;

fn dream_schedule_req(method: Method, uri: &str, body: Option<&str>) -> Request<Body> {
    let mut b = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, "Bearer admin-token");
    if body.is_some() {
        b = b.header(header::CONTENT_TYPE, "application/json");
    }
    b.body(body.map_or_else(Body::empty, |s| Body::from(s.to_string())))
        .unwrap()
}

/// GET on a fresh store reports the config defaults, `source = config`,
/// and no run data.
#[tokio::test]
async fn dream_schedule_get_returns_config_defaults() {
    let (app, state) = make_test_app();
    let (status, body) = send(
        app,
        dream_schedule_req(Method::GET, "/v1/dream/schedule", None),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&body).expect("valid JSON");
    assert_eq!(v["dream_enabled"], serde_json::json!(false));
    assert_eq!(v["scheduler_enabled"], serde_json::json!(false));
    assert_eq!(
        v["enabled"],
        serde_json::json!(state.config.dream.schedule_enabled)
    );
    assert_eq!(
        v["interval_secs"],
        serde_json::json!(state.config.dream.schedule_interval_secs)
    );
    assert_eq!(v["source"], serde_json::json!("config"));
    assert_eq!(
        v["poll_secs"],
        serde_json::json!(loomem_core::scheduler::DREAM_POLL_SECS)
    );
    assert_eq!(v["queued"], serde_json::json!(false));
    assert_eq!(v["paused"], serde_json::json!(false));
    assert!(v["last_run_at"].is_null(), "{v}");
    assert!(v["next_run_at"].is_null(), "{v}");
    assert!(v["run_requested_at"].is_null(), "{v}");
    assert!(v["last_result"].is_null(), "{v}");
}

/// PUT persists the override (`source = override`), anchors a row without a
/// base, keeps the anchor across interval changes, and re-anchors on a
/// disabled → enabled transition so `next_run_at` is never in the past.
#[tokio::test]
async fn dream_schedule_put_round_trips_and_anchors() {
    let (app, state) = make_test_app();
    let put = |body: &'static str| {
        let app = app.clone();
        async move {
            let (status, bytes) = send(
                app,
                dream_schedule_req(Method::PUT, "/v1/dream/schedule", Some(body)),
            )
            .await;
            assert_eq!(
                status,
                StatusCode::OK,
                "{}",
                String::from_utf8_lossy(&bytes)
            );
            let v: serde_json::Value = serde_json::from_slice(&bytes).expect("valid JSON");
            v
        }
    };

    // 1. Enable with a 12 h interval: anchored now, next = anchor + interval.
    let before = loomem_core::workers_registry::now_epoch();
    let v = put(r#"{"enabled": true, "interval_secs": 43200}"#).await;
    assert_eq!(v["enabled"], serde_json::json!(true));
    assert_eq!(v["interval_secs"], serde_json::json!(43200));
    assert_eq!(v["source"], serde_json::json!("override"));
    let row = loomem_core::dream_schedule::load(&state.store).expect("row");
    assert_eq!(row.enabled, Some(true));
    assert_eq!(row.interval_secs, Some(43200));
    assert_eq!(row.last_seen_enabled, Some(true));
    let anchor = row.anchor_at.expect("PUT anchors a row without a base");
    assert!(anchor >= before);
    assert_eq!(v["next_run_at"], serde_json::json!(anchor + 43200));

    // 2. Change the interval while enabled: the anchor is kept.
    let v = put(r#"{"enabled": true, "interval_secs": 7200}"#).await;
    assert_eq!(v["next_run_at"], serde_json::json!(anchor + 7200));
    let row = loomem_core::dream_schedule::load(&state.store).expect("row");
    assert_eq!(row.anchor_at, Some(anchor));

    // 3. Disable: no next run, transition recorded.
    let v = put(r#"{"enabled": false, "interval_secs": 7200}"#).await;
    assert_eq!(v["enabled"], serde_json::json!(false));
    assert!(v["next_run_at"].is_null(), "{v}");
    let row = loomem_core::dream_schedule::load(&state.store).expect("row");
    assert_eq!(row.last_seen_enabled, Some(false));

    // 4. Re-enable: re-anchored (no catch-up), next = new anchor + interval.
    let v = put(r#"{"enabled": true, "interval_secs": 7200}"#).await;
    let row = loomem_core::dream_schedule::load(&state.store).expect("row");
    let re_anchor = row.anchor_at.expect("still anchored");
    assert!(re_anchor >= anchor);
    assert_eq!(v["next_run_at"], serde_json::json!(re_anchor + 7200));
    assert_eq!(row.last_seen_enabled, Some(true));
}

#[tokio::test]
async fn dream_schedule_put_rejects_out_of_range_interval() {
    let (app, state) = make_test_app();
    for body in [
        r#"{"enabled": true, "interval_secs": 3599}"#,
        r#"{"enabled": true, "interval_secs": 2592001}"#,
    ] {
        let (status, resp) = send(
            app.clone(),
            dream_schedule_req(Method::PUT, "/v1/dream/schedule", Some(body)),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        let v: serde_json::Value = serde_json::from_slice(&resp).expect("valid JSON");
        assert!(v["error"].as_str().unwrap_or("").contains("interval_secs"));
    }
    // Nothing was written.
    let row = loomem_core::dream_schedule::load(&state.store).expect("row");
    assert_eq!(
        row,
        loomem_core::dream_schedule::DreamScheduleRow::default()
    );
}

/// POST run → 202, `queued = true`, and `run_requested_at` persisted so
/// the scheduler picks it up at its next poll.
#[tokio::test]
async fn dream_schedule_run_queues_when_dream_and_scheduler_enabled() {
    let mut config = test_config();
    config.dream.enabled = true;
    config.scheduler.enabled = true;
    let (app, state) = make_test_app_cfg(config);
    let (status, body) = send(
        app,
        dream_schedule_req(Method::POST, "/v1/dream/schedule/run", None),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::ACCEPTED,
        "{}",
        String::from_utf8_lossy(&body)
    );
    let v: serde_json::Value = serde_json::from_slice(&body).expect("valid JSON");
    assert_eq!(v["queued"], serde_json::json!(true));
    assert!(v["run_requested_at"].is_u64(), "{v}");

    let row = loomem_core::dream_schedule::load(&state.store).expect("row");
    assert!(row.run_requested_at.is_some());
    assert_eq!(
        row.run_requested_at.map(serde_json::Value::from),
        Some(v["run_requested_at"].clone())
    );
}

/// The test config has `dream.enabled = false` and the scheduler off:
/// queuing is refused with 409 and no row is written.
#[tokio::test]
async fn dream_schedule_run_conflicts_when_dream_disabled() {
    let (app, state) = make_test_app();
    let (status, body) = send(
        app,
        dream_schedule_req(Method::POST, "/v1/dream/schedule/run", None),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let v: serde_json::Value = serde_json::from_slice(&body).expect("valid JSON");
    assert!(
        v["error"].as_str().unwrap_or("").contains("disabled"),
        "{v}"
    );
    let row = loomem_core::dream_schedule::load(&state.store).expect("row");
    assert!(row.run_requested_at.is_none());
}

/// Dream enabled but scheduler off → 409 as well (nobody would execute it).
#[tokio::test]
async fn dream_schedule_run_conflicts_when_scheduler_disabled() {
    let mut config = test_config();
    config.dream.enabled = true;
    let (app, _state) = make_test_app_cfg(config);
    let (status, body) = send(
        app,
        dream_schedule_req(Method::POST, "/v1/dream/schedule/run", None),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let v: serde_json::Value = serde_json::from_slice(&body).expect("valid JSON");
    assert!(
        v["error"].as_str().unwrap_or("").contains("Scheduler"),
        "{v}"
    );
}

/// A paused `dream` worker would never execute the request: 409, nothing
/// written, and GET reports `paused: true`.
#[tokio::test]
async fn dream_schedule_run_conflicts_when_worker_paused() {
    let mut config = test_config();
    config.dream.enabled = true;
    config.scheduler.enabled = true;
    let (app, state) = make_test_app_cfg(config);
    state
        .workers
        .get("dream")
        .expect("dream worker registered")
        .paused
        .store(true, SeqCst);

    let (status, body) = send(
        app.clone(),
        dream_schedule_req(Method::POST, "/v1/dream/schedule/run", None),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let v: serde_json::Value = serde_json::from_slice(&body).expect("valid JSON");
    assert_eq!(v["error"], serde_json::json!("Dream worker is paused"));
    let row = loomem_core::dream_schedule::load(&state.store).expect("row");
    assert!(row.run_requested_at.is_none());

    let (status, body) = send(
        app,
        dream_schedule_req(Method::GET, "/v1/dream/schedule", None),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&body).expect("valid JSON");
    assert_eq!(v["paused"], serde_json::json!(true));
}

/// Without any bearer the auth middleware rejects the admin endpoints
/// before the handler runs (401). The 403 path for a non-admin bearer is
/// covered at handler level in `handlers::dream_schedule::tests`.
#[tokio::test]
async fn dream_schedule_requires_auth() {
    let (app, _state) = make_test_app();
    for (method, uri) in [
        (Method::GET, "/v1/dream/schedule"),
        (Method::PUT, "/v1/dream/schedule"),
        (Method::POST, "/v1/dream/schedule/run"),
    ] {
        let req = Request::builder()
            .method(method.clone())
            .uri(uri)
            .body(Body::empty())
            .unwrap();
        let (status, _body) = send(app.clone(), req).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {uri}");
    }
}
