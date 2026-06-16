//! Configuration for the access-audit subsystem (ADR-018, cycle /150e).
//!
//! `AccessAuditConfig` is a sub-config owned by the `access_audit` module
//! (CLAUDE.md §5). Composed into the root `Config` with `#[serde(default)]`, so
//! existing `config.toml` files without an `[access_audit]` section load and
//! degrade to **off** (no access records written, zero runtime impact).

use serde::{Deserialize, Serialize};

/// Per-instance access-audit config. Default off (ADR-018 env-gating).
/// `#[derive(Default)]` ⇒ `enabled = false` (bool default).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AccessAuditConfig {
    /// When `true`, read/search/store handlers record `AccessRecord`s. When
    /// `false` (default) the handler hooks are no-ops — byte-identical to
    /// pre-/150e behavior (ADR-018 AC7).
    pub enabled: bool,
}

impl AccessAuditConfig {
    /// Apply env var overrides (per-instance enable). Env takes precedence
    /// over `config.toml`. Mirrors `ContentTypeConfig`: the baked image
    /// carries no `[access_audit]` section (default off), so
    /// `LOOMEM_ACCESS_AUDIT_ENABLED=true` turns it on for a single instance
    /// without a global config edit.
    pub fn apply_env_overrides(&mut self) {
        if let Ok(v) = std::env::var("LOOMEM_ACCESS_AUDIT_ENABLED") {
            match v.as_str() {
                "true" | "1" => self.enabled = true,
                "false" | "0" => self.enabled = false,
                // Empty / unknown → keep current value + WARN (avoid silent
                // regression on a typo like "True").
                other => tracing::warn!(
                    "LOOMEM_ACCESS_AUDIT_ENABLED={:?} not recognized (expected true/false/1/0), keeping current value {}",
                    other,
                    self.enabled
                ),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_off() {
        assert!(!AccessAuditConfig::default().enabled);
    }

    // Env-var tests mutate process-global state and race the multi-threaded
    // cargo test runner; `serial_test` is not a dependency (CLAUDE.md §7), so
    // they are #[ignore]d and run manually — mirrors `config::tests` env cases.
    //   cargo test -p loomem-core --lib -- --ignored --test-threads=1 access_audit

    #[test]
    #[ignore = "env-var race; manually verified (serial_test not in deps)"]
    fn env_true_enables() {
        let mut cfg = AccessAuditConfig::default();
        std::env::set_var("LOOMEM_ACCESS_AUDIT_ENABLED", "true");
        cfg.apply_env_overrides();
        std::env::remove_var("LOOMEM_ACCESS_AUDIT_ENABLED");
        assert!(cfg.enabled);
    }

    #[test]
    #[ignore = "env-var race; manually verified (serial_test not in deps)"]
    fn env_unknown_keeps_current() {
        let mut cfg = AccessAuditConfig { enabled: false };
        std::env::set_var("LOOMEM_ACCESS_AUDIT_ENABLED", "yes"); // typo
        cfg.apply_env_overrides();
        std::env::remove_var("LOOMEM_ACCESS_AUDIT_ENABLED");
        assert!(!cfg.enabled, "unrecognized value must not flip the default");
    }
}
