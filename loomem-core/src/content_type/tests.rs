//! Unit tests for the content-type classifier (ADR-017, cycle/142 + /143).
//! Extracted from `mod.rs` to keep the production module under the §1 file SLOC
//! budget (wzorzec `manifest/tests.rs`). `use super::*` sees the module's private
//! items (the cache/sidecar helpers). Since /143 the LLM is the only classifier,
//! so every test stubs it — zero real HTTP.

use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use tempfile::TempDir;

use crate::storage::RocksDbConfig;

fn test_store() -> (TempDir, RocksDbStore) {
    let tmp = TempDir::new().expect("tempdir");
    let cfg = RocksDbConfig {
        max_open_files: 100,
        compression: "lz4".to_string(),
        write_buffer_size: 4 * 1024 * 1024,
        max_write_buffer_number: 2,
    };
    let store = RocksDbStore::open(tmp.path(), &cfg).expect("open store");
    (tmp, store)
}

/// Stub LLM classifier — returns a fixed type, counts invocations.
struct StubClassifier {
    ret: ContentType,
    calls: AtomicUsize,
}
impl StubClassifier {
    fn new(ret: ContentType) -> Self {
        Self {
            ret,
            calls: AtomicUsize::new(0),
        }
    }
}
impl ContentTypeClassifier for StubClassifier {
    async fn classify(&self, _content: &str) -> anyhow::Result<ContentType> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.ret)
    }
}

/// Stub whose LLM call always errors (exercises the error → `None` path).
struct ErrClassifier;
impl ContentTypeClassifier for ErrClassifier {
    async fn classify(&self, _content: &str) -> anyhow::Result<ContentType> {
        anyhow::bail!("simulated LLM failure")
    }
}

/// Stub that must never be polled (asserts the LLM path is not taken).
struct PanicClassifier;
impl ContentTypeClassifier for PanicClassifier {
    async fn classify(&self, _content: &str) -> anyhow::Result<ContentType> {
        panic!("LLM classifier must not be called")
    }
}

fn enabled_cfg() -> ContentTypeConfig {
    ContentTypeConfig {
        enabled: true,
        ..ContentTypeConfig::default()
    }
}

// AC-2: enabled + stub-LLM returning `case_study` → `Some(meta)` with that type
// and source `llm`; the value round-trips through the sidecar. Zero HTTP.
#[tokio::test]
async fn ac2_enabled_classifies_via_llm_and_persists() {
    let (_tmp, store) = test_store();
    let stub = StubClassifier::new(ContentType::CaseStudy);
    let meta = classify_content(&stub, &enabled_cfg(), &store, "narrative client work")
        .await
        .expect("enabled + Ok(stub) → Some");
    assert_eq!(stub.calls.load(Ordering::SeqCst), 1, "LLM called once");
    assert_eq!(meta.content_type, ContentType::CaseStudy);
    assert_eq!(meta.source, ClassifierSource::Llm);

    put_content_type(&store, "chunk-1", &meta);
    assert_eq!(get_content_type(&store, "chunk-1"), Some(meta));
}

// AC-3: `enabled=false` → `None`, LLM never called.
#[tokio::test]
async fn ac3_disabled_returns_none_never_calls_llm() {
    let (_tmp, store) = test_store();
    let cfg = ContentTypeConfig::default();
    assert!(!cfg.enabled);
    let out = classify_content(&PanicClassifier, &cfg, &store, "anything").await;
    assert_eq!(out, None);
}

// AC-3: enabled but the LLM call errors → `None` (no guess), no sidecar entry.
#[tokio::test]
async fn ac3_llm_error_returns_none() {
    let (_tmp, store) = test_store();
    let out = classify_content(&ErrClassifier, &enabled_cfg(), &store, "anything").await;
    assert_eq!(out, None);
}

// AC-8: a second classification of the same content+model hits the cache —
// the LLM stub is called exactly once. Key includes the model.
#[tokio::test]
async fn ac8_llm_cache_hit_skips_second_call() {
    let (_tmp, store) = test_store();
    let stub = StubClassifier::new(ContentType::Article);
    let cfg = enabled_cfg();
    let content = "repeated content";
    let _ = classify_content(&stub, &cfg, &store, content).await;
    let _ = classify_content(&stub, &cfg, &store, content).await;
    assert_eq!(
        stub.calls.load(Ordering::SeqCst),
        1,
        "second call must hit cache, not the LLM"
    );
}

// AC-8: a different model is a different cache key → the LLM is called again.
#[tokio::test]
async fn ac8_cache_key_includes_model() {
    let (_tmp, store) = test_store();
    let stub = StubClassifier::new(ContentType::Article);
    let content = "repeated content";
    let cfg_a = ContentTypeConfig {
        enabled: true,
        model: "model-a".to_string(),
    };
    let cfg_b = ContentTypeConfig {
        enabled: true,
        model: "model-b".to_string(),
    };
    let _ = classify_content(&stub, &cfg_a, &store, content).await;
    let _ = classify_content(&stub, &cfg_b, &store, content).await;
    assert_eq!(
        stub.calls.load(Ordering::SeqCst),
        2,
        "distinct models must each call the LLM"
    );
}

// Sidecar: put → get round-trip; absent id → None; batch hydration.
#[test]
fn sidecar_roundtrip_and_absent() {
    let (_tmp, store) = test_store();
    let meta = ContentTypeMeta {
        content_type: ContentType::Changelog,
        source: ClassifierSource::Llm,
    };
    put_content_type(&store, "chunk-1", &meta);
    assert_eq!(get_content_type(&store, "chunk-1"), Some(meta));
    assert_eq!(get_content_type(&store, "absent"), None);

    let ids = vec!["chunk-1".to_string(), "absent".to_string()];
    let map = get_content_types(&store, &ids);
    assert_eq!(map.len(), 1);
    assert_eq!(map.get("chunk-1"), Some(&meta));
}

// A stale /142 `deterministic`-source sidecar row no longer deserializes into
// the /143 `ContentTypeMeta` → reads as `None` (no tag) until backfill rewrites.
#[test]
fn stale_deterministic_sidecar_reads_as_none() {
    let (_tmp, store) = test_store();
    let legacy = br#"{"content_type":"policy","band":"high","source":"deterministic"}"#;
    store
        .db()
        .put(sidecar_key("legacy"), legacy)
        .expect("seed legacy row");
    assert_eq!(get_content_type(&store, "legacy"), None);
}

#[test]
fn content_type_str_roundtrips() {
    for ct in [
        ContentType::OperationalInstruction,
        ContentType::Policy,
        ContentType::Changelog,
        ContentType::CaseStudy,
        ContentType::Article,
        ContentType::PersonProfile,
        ContentType::Index,
        ContentType::OrgFact,
        ContentType::TechnicalProject,
        ContentType::Other,
    ] {
        assert_eq!(ContentType::parse(ct.as_str()), Some(ct));
    }
    assert_eq!(ContentType::parse("nonsense"), None);
}

// ── LOOMEM_CONTENT_TYPE_ENABLED env override (/143) ──
// `#[ignore]` per repo convention (`config.rs` shared-scope tests): env-var
// mutation races the multi-threaded test runner and `serial_test` is not a dep.
// Run explicitly with `cargo test -- --ignored content_type_env`.

#[test]
#[ignore = "env-var race; serial_test not in deps (matches config.rs convention)"]
fn content_type_env_override_true_enables() {
    let mut cfg = ContentTypeConfig::default();
    assert!(!cfg.enabled, "sanity: default disabled");
    std::env::set_var("LOOMEM_CONTENT_TYPE_ENABLED", "true");
    cfg.apply_env_overrides();
    std::env::remove_var("LOOMEM_CONTENT_TYPE_ENABLED");
    assert!(cfg.enabled, "true must enable typing");
}

#[test]
#[ignore = "env-var race; serial_test not in deps (matches config.rs convention)"]
fn content_type_env_override_false_disables() {
    // Both falsy spellings share one match arm (`"false" | "0"`); cover each so
    // a refactor dropping either branch is caught (`0` is the common Unix form).
    for falsy in ["false", "0"] {
        let mut cfg = ContentTypeConfig {
            enabled: true,
            ..ContentTypeConfig::default()
        };
        std::env::set_var("LOOMEM_CONTENT_TYPE_ENABLED", falsy);
        cfg.apply_env_overrides();
        std::env::remove_var("LOOMEM_CONTENT_TYPE_ENABLED");
        assert!(!cfg.enabled, "{falsy:?} must disable typing");
    }
}

#[test]
#[ignore = "env-var race; serial_test not in deps (matches config.rs convention)"]
fn content_type_env_override_unknown_value_keeps_previous() {
    let mut cfg = ContentTypeConfig {
        enabled: true,
        ..ContentTypeConfig::default()
    };
    std::env::set_var("LOOMEM_CONTENT_TYPE_ENABLED", "True"); // typo — capital T
    cfg.apply_env_overrides();
    std::env::remove_var("LOOMEM_CONTENT_TYPE_ENABLED");
    assert!(cfg.enabled, "unrecognized value must not regress current");
}
