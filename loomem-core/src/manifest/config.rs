//! Configuration for stream-kind-aware manifests (ADR-014, cycle/139).
//!
//! `ManifestConfig` is a sub-config owned by the `manifest` module (CLAUDE.md
//! §5). Governance for each shared/project stream is declarative and lives in
//! `config.streams[stream_id]` — operator-maintained, never LLM-generated.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Per-instance manifest configuration. Composed into the root `Config` with
/// `#[serde(default)]`, so existing `config.toml` files without a `[manifest]`
/// section load and degrade to the deterministic minimum (ADR-014).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestConfig {
    /// When false, the LLM contents-summary step is skipped entirely (no HTTP);
    /// the manifest still returns governance + stats. Default false.
    pub enabled: bool,
    /// Model used for the contents-summary completion.
    pub model: String,
    /// Cap on chunks fed to the contents-summary prompt.
    pub max_chunks: usize,
    /// TTL for the on-disk manifest cache (`data_dir/manifests/`).
    pub cache_ttl_secs: u64,
    /// Declarative governance keyed by `stream_id`. Absent entry → manifest
    /// reports `governance_configured = false` (never falls back to a person).
    #[serde(default)]
    pub streams: HashMap<String, StreamGovernance>,
}

impl Default for ManifestConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            model: "gpt-4.1-mini".to_string(),
            max_chunks: 100,
            cache_ttl_secs: 3600,
            streams: HashMap::new(),
        }
    }
}

impl ManifestConfig {
    /// Apply env var override. `LOOMEM_MANIFEST_CONFIG` carries a COMPLETE
    /// `ManifestConfig` as inline JSON and fully replaces the config-file value
    /// (env takes precedence — mirrors `AuthConfig::apply_env_overrides`). Blank
    /// or malformed → keep current + WARN (never crash boot on operator typo).
    pub fn apply_env_overrides(&mut self) {
        if let Ok(v) = std::env::var("LOOMEM_MANIFEST_CONFIG") {
            if v.trim().is_empty() {
                return; // set-but-blank → keep current
            }
            match serde_json::from_str::<ManifestConfig>(&v) {
                Ok(parsed) => *self = parsed,
                Err(e) => tracing::warn!(
                    "LOOMEM_MANIFEST_CONFIG parse failed ({e}); keeping config.toml/default manifest"
                ),
            }
        }
    }
}

/// Operator-declared governance for a single shared/project stream. These
/// fields are load-bearing (ADR-014 alt. B): hallucinating a rule or
/// source-of-truth is worse than omitting it, so they are never LLM-generated.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamGovernance {
    pub title: String,
    pub purpose: String,
    pub scope_includes: String,
    pub scope_excludes: String,
    pub governance: String,
    pub source_of_truth: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    // LOOMEM_MANIFEST_CONFIG is process-global; serialize env-mutating tests so
    // the multi-threaded cargo runner does not race (mirrors /95 env-lock
    // pattern; serial_test is not a dependency per brief §6). std Mutex suffices —
    // these are sync tests. Poison is recovered: a panicking test still releases
    // a usable lock to the next.
    //
    // SCOPE: this guard is module-local — it only serializes tests *in this
    // module*. It is invisible to callers elsewhere in the loomem-core test
    // binary. `Config::load` (config.rs) reads LOOMEM_MANIFEST_CONFIG via
    // `config.manifest.apply_env_overrides()` and is currently boot-only (no
    // loomem-core test exercises it), so there is no live cross-module race
    // today. If a future test reads/writes LOOMEM_MANIFEST_CONFIG outside this
    // module (e.g. a `Config::load` integration test), it MUST either take this
    // same `env_guard()` or follow the `#[ignore]` sibling pattern used by the
    // auth env tests in config.rs — otherwise it can race against these tests.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn env_guard() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn snapshot(c: &ManifestConfig) -> String {
        serde_json::to_string(c).expect("ManifestConfig serializes")
    }

    const GOVERNANCE_JSON: &str = r#"{"enabled":true,"model":"gpt-4.1-mini","max_chunks":50,"cache_ttl_secs":1800,"streams":{"__shared_team__":{"title":"Team Shared Memory","purpose":"work memory","scope_includes":"projects","scope_excludes":"private","governance":"team-only","source_of_truth":"knowledge-base import"}}}"#;

    #[test]
    fn ac1_valid_json_replaces_and_populates_streams() {
        let _g = env_guard();
        let mut cfg = ManifestConfig::default();
        std::env::set_var("LOOMEM_MANIFEST_CONFIG", GOVERNANCE_JSON);
        cfg.apply_env_overrides();
        std::env::remove_var("LOOMEM_MANIFEST_CONFIG");

        assert!(cfg.enabled, "enabled flipped from default false");
        let gov = cfg
            .streams
            .get("__shared_team__")
            .expect("__shared_team__ governance present");
        assert_eq!(gov.title, "Team Shared Memory");
        assert_eq!(gov.governance, "team-only");
        assert_eq!(gov.source_of_truth, "knowledge-base import");
    }

    #[test]
    fn ac2_env_wins_over_config_file_value() {
        let _g = env_guard();
        // Simulate a value loaded from config.toml: enabled=false, distinct model.
        let mut cfg = ManifestConfig {
            enabled: false,
            model: "config-toml-model".to_string(),
            ..ManifestConfig::default()
        };
        std::env::set_var("LOOMEM_MANIFEST_CONFIG", GOVERNANCE_JSON);
        cfg.apply_env_overrides();
        std::env::remove_var("LOOMEM_MANIFEST_CONFIG");

        // Full replace: env's enabled=true and model win over config-file values.
        assert!(cfg.enabled, "env enabled=true must win over config false");
        assert_eq!(
            cfg.model, "gpt-4.1-mini",
            "env model must win (full replace)"
        );
    }

    #[test]
    fn ac3_unset_keeps_input_byte_identical() {
        let _g = env_guard();
        std::env::remove_var("LOOMEM_MANIFEST_CONFIG");
        let mut cfg = ManifestConfig {
            enabled: true,
            model: "custom".to_string(),
            max_chunks: 7,
            cache_ttl_secs: 11,
            ..ManifestConfig::default()
        };
        let before = snapshot(&cfg);
        cfg.apply_env_overrides();
        assert_eq!(before, snapshot(&cfg), "unset env must not mutate config");
    }

    #[test]
    fn ac4_malformed_json_warns_and_keeps_current() {
        let _g = env_guard();
        let mut cfg = ManifestConfig::default();
        let before = snapshot(&cfg);
        std::env::set_var("LOOMEM_MANIFEST_CONFIG", "{bad");
        cfg.apply_env_overrides(); // must not panic
        std::env::remove_var("LOOMEM_MANIFEST_CONFIG");
        assert_eq!(
            before,
            snapshot(&cfg),
            "malformed env must not mutate config"
        );
    }

    #[test]
    fn ac5_blank_keeps_current() {
        let _g = env_guard();
        let mut cfg = ManifestConfig::default();
        let before = snapshot(&cfg);
        std::env::set_var("LOOMEM_MANIFEST_CONFIG", "");
        cfg.apply_env_overrides();
        std::env::remove_var("LOOMEM_MANIFEST_CONFIG");
        assert_eq!(before, snapshot(&cfg), "blank env must not mutate config");
    }
}
