use super::*;
use crate::{config::RocksDbConfig, cost_tracker::CostConfig, storage::RocksDbStore};
use serde_json::json;
use std::sync::Arc;

fn tracker(cap: f64) -> (tempfile::TempDir, CostTracker) {
    let temp = tempfile::tempdir().unwrap();
    let store = Arc::new(
        RocksDbStore::open(
            temp.path(),
            &RocksDbConfig {
                max_open_files: 100,
                compression: "none".into(),
                write_buffer_size: 4 * 1024 * 1024,
                max_write_buffer_number: 2,
            },
        )
        .unwrap(),
    );
    let tracker = CostTracker::new(
        store,
        CostConfig {
            daily_cap_usd: cap,
            ..CostConfig::default()
        },
        reqwest::Client::new(),
    );
    (temp, tracker)
}

fn response() -> Value {
    json!({"service_tier":"default","model":"gpt-4.1-2025-04-14", "usage":{"prompt_tokens":1000,"completion_tokens":100,"total_tokens":1100}})
}

#[test]
fn known_usage_reconciles_global_and_stream_and_cached_discount() {
    let (_temp, tracker) = tracker(10.0);
    let accounting = SelectorAccounting {
        tracker: &tracker,
        stream_id: Some("stream"),
    };
    let reservation = accounting.reserve("gpt-4.1").unwrap();
    assert_eq!(tracker.daily_summary().selector_unknown_usage_calls, 1);
    reservation.reconcile(&response()).unwrap();
    for scope in [None, Some("stream")] {
        let cost = tracker.get_cost(scope).unwrap();
        assert!((cost.total_usd - 0.0028).abs() < 1e-12);
        assert_eq!(cost.selector_calls, 1);
        assert_eq!(cost.selector_unknown_usage_calls, 0);
        assert_eq!(cost.total_input_tokens, 1000);
    }
    let mut cached = response();
    cached["usage"]["prompt_tokens_details"] = json!({"cached_tokens":1000});
    accounting
        .reserve("gpt-4.1")
        .unwrap()
        .reconcile(&cached)
        .unwrap();
    assert!((tracker.daily_summary().total_usd - 0.0041).abs() < 1e-12);
}

#[test]
fn unknown_invalid_usage_and_unsupported_models_preserve_reservations() {
    let (_temp, tracker) = tracker(3.0);
    let accounting = SelectorAccounting {
        tracker: &tracker,
        stream_id: None,
    };
    assert!(accounting.reserve("unpriced").is_err());
    let reservation = accounting.reserve("gpt-4.1").unwrap();
    let before = tracker.daily_summary();
    assert!(reservation.reconcile(&json!({"service_tier":"default","model":"gpt-4.1","usage":{"prompt_tokens":1000,"completion_tokens":100,"total_tokens":1}})).is_err());
    assert_eq!(tracker.daily_summary().total_usd, before.total_usd);
    assert!(accounting.reserve("gpt-4.1").is_err());
    assert_eq!(tracker.daily_summary().selector_unknown_usage_calls, 1);
}

#[test]
fn independent_tracker_instances_compete_for_one_durable_budget() {
    let (_temp, tracker) = tracker(3.0);
    let other = CostTracker::new(
        tracker.store.clone(),
        tracker.config.clone(),
        reqwest::Client::new(),
    );
    let first = SelectorAccounting {
        tracker: &tracker,
        stream_id: Some("stream"),
    }
    .reserve("gpt-4.1")
    .unwrap();
    let second = SelectorAccounting {
        tracker: &other,
        stream_id: Some("other"),
    }
    .reserve("gpt-4.1");
    assert!(second.is_err());
    drop(first);
    assert_eq!(other.daily_summary().selector_unknown_usage_calls, 1);
    assert_eq!(
        read_pending(&other, &pending_keys(None)).unwrap()[0],
        other.daily_summary().total_usd
    );
}

#[test]
fn persist_disabled_is_fail_closed_and_old_daily_serialization_is_compatible() {
    let (_temp, mut tracker) = tracker(10.0);
    tracker.config.persist = false;
    assert!(SelectorAccounting {
        tracker: &tracker,
        stream_id: None
    }
    .reserve("gpt-4.1")
    .is_err());
    let old = json!({"date":"2000-01-01","total_usd":0,"embed_calls":0,"compress_calls":0,"total_input_tokens":0,"total_output_tokens":0});
    let cost: DailyCost = serde_json::from_value(old).unwrap();
    assert_eq!(cost.selector_calls, 0);
}

#[test]
fn day_rollover_settles_original_bucket_and_unknown_blocks_next_day() {
    let (_temp, tracker) = tracker(3.0);
    let accounting = SelectorAccounting {
        tracker: &tracker,
        stream_id: Some("stream"),
    };
    let first = accounting.reserve_on_day("gpt-4.1", "2000-01-01").unwrap();
    assert!(accounting.reserve_on_day("gpt-4.1", "2000-01-02").is_err());
    first.reconcile(&response()).unwrap();
    let keys = vec!["cost:2000-01-01".into(), "cost:stream:2000-01-01".into()];
    for cost in read_costs(&tracker, &keys).unwrap() {
        assert!((cost.total_usd - 0.0028).abs() < 1e-12);
        assert_eq!(cost.selector_unknown_usage_calls, 0);
    }
    assert!(accounting.reserve_on_day("gpt-4.1", "2000-01-02").is_ok());
}

#[test]
fn known_usage_above_estimate_is_recorded_in_full_and_returns_breach() {
    let (_temp, tracker) = tracker(3.0);
    let accounting = SelectorAccounting {
        tracker: &tracker,
        stream_id: None,
    };
    let reservation = accounting.reserve("gpt-4.1").unwrap();
    let huge = json!({"service_tier":"default","model":"gpt-4.1","usage":{"prompt_tokens":2000000,"completion_tokens":100,"total_tokens":2000100}});
    assert!(reservation.reconcile(&huge).is_err());
    assert!((tracker.daily_summary().total_usd - 4.0008).abs() < 1e-12);
    assert_eq!(tracker.daily_summary().selector_unknown_usage_calls, 0);
    assert!(accounting.reserve("gpt-4.1").is_err());
}

#[test]
fn competing_threads_admit_only_one_and_unknown_survives_store_reopen() {
    let (temp, tracker) = tracker(3.0);
    let shared = Arc::new(tracker);
    let barrier = Arc::new(std::sync::Barrier::new(3));
    let threads: Vec<_> = (0..2)
        .map(|_| {
            let tracker = shared.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                SelectorAccounting {
                    tracker: &tracker,
                    stream_id: Some("stream"),
                }
                .reserve_on_day("gpt-4.1", "2000-01-01")
                .is_ok()
            })
        })
        .collect();
    barrier.wait();
    let admitted = threads
        .into_iter()
        .map(|handle| usize::from(handle.join().unwrap()))
        .sum::<usize>();
    assert_eq!(admitted, 1);
    drop(shared);
    let store = Arc::new(
        RocksDbStore::open(
            temp.path(),
            &RocksDbConfig {
                max_open_files: 100,
                compression: "none".into(),
                write_buffer_size: 4 * 1024 * 1024,
                max_write_buffer_number: 2,
            },
        )
        .unwrap(),
    );
    let tracker = CostTracker::new(
        store,
        CostConfig {
            daily_cap_usd: 3.0,
            ..CostConfig::default()
        },
        reqwest::Client::new(),
    );
    assert!(SelectorAccounting {
        tracker: &tracker,
        stream_id: Some("stream")
    }
    .reserve_on_day("gpt-4.1", "2000-01-02")
    .is_err());
}

#[test]
fn invalid_model_or_cached_counts_retain_full_estimated_charge() {
    let (_temp, tracker) = tracker(10.0);
    for response in [
        json!({"service_tier":"default","model":"gpt-4.1-mini","usage":{"prompt_tokens":1000,"completion_tokens":100,"total_tokens":1100}}),
        json!({"service_tier":"default","model":"gpt-4.1","usage":{"prompt_tokens":1000,"completion_tokens":100,"total_tokens":1100,"prompt_tokens_details":{"cached_tokens":1001}}}),
    ] {
        let before = tracker.daily_summary().selector_unknown_usage_calls;
        let reservation = SelectorAccounting {
            tracker: &tracker,
            stream_id: None,
        }
        .reserve("gpt-4.1")
        .unwrap();
        assert!(reservation.reconcile(&response).is_err());
        assert_eq!(
            tracker.daily_summary().selector_unknown_usage_calls,
            before + 1
        );
    }
}

#[test]
fn missing_or_different_service_tier_retains_unknown_reservation() {
    let (_temp, tracker) = tracker(10.0);
    for tier in [None, Some("priority"), Some("flex"), Some("auto")] {
        let mut envelope = response();
        if let Some(tier) = tier {
            envelope["service_tier"] = json!(tier);
        } else {
            envelope.as_object_mut().unwrap().remove("service_tier");
        }
        let before = tracker.daily_summary();
        let reservation = SelectorAccounting {
            tracker: &tracker,
            stream_id: Some("stream"),
        }
        .reserve("gpt-4.1")
        .unwrap();
        let charged = tracker.daily_summary().total_usd;
        assert!(reservation.reconcile(&envelope).is_err());
        let after = tracker.daily_summary();
        assert_eq!(after.total_usd, charged);
        assert_eq!(
            after.selector_unknown_usage_calls,
            before.selector_unknown_usage_calls + 1
        );
        assert_eq!(after.total_input_tokens, before.total_input_tokens);
        assert_eq!(tracker.get_cost(Some("stream")).unwrap().total_usd, charged);
    }
}
