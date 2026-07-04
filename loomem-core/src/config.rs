use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

// Re-exports: each *Config lives in its owning module (CLAUDE.md §5).
// These `pub use` re-exports preserve the historical
// `use crate::config::FooConfig` paths used throughout the codebase.
pub use crate::access_audit::AccessAuditConfig;
pub use crate::advisor::AdvisorConfig;
pub use crate::associator::clustering::ClusteringConfig;
pub use crate::associator::AssociatorConfig;
pub use crate::consolidation::ConsolidationConfig;
pub use crate::content_type::ContentTypeConfig;
pub use crate::contradiction::ContradictionConfig;
pub use crate::cost_tracker::CostConfig;
pub use crate::decay::{DecayConfig, DecayWorkerConfig};
pub use crate::dream::DreamConfig;
pub use crate::entity_extractor::EntityExtractionConfig;
pub use crate::event_log::EventLogConfig;
pub use crate::feedback::FeedbackConfig;
pub use crate::graph::GraphSearchConfig;
pub use crate::hybrid_search::{
    ComplexityConfig, HybridWeightsConfig, ImportanceConfig, SearchConfig,
};
pub use crate::intent_log::IntentLogConfig;
pub use crate::llm::LlmConfig;
pub use crate::manifest::ManifestConfig;
pub use crate::memory_extractor::KnowledgeExtractionConfig;
pub use crate::memory_generator::MemoryGeneratorConfig;
pub use crate::pii_filter::PiiConfig;
pub use crate::profile::ProfileConfig;
pub use crate::scheduler::{SchedulerConfig, WorkerConfig};
pub use crate::storage::{RocksDbConfig, StorageConfig};
pub use crate::tantivy_index::TantivyConfig;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub storage: StorageConfig,
    pub search: SearchConfig,
    pub worker: WorkerConfig,
    pub scheduler: SchedulerConfig,
    pub llm: LlmConfig,
    pub server: ServerConfig,
    pub resource_guards: ResourceGuardsConfig,
    pub streams: StreamsConfig,
    #[serde(default)]
    pub namespaces: HashMap<String, String>,
    pub pii: PiiConfig,
    pub cost: CostConfig,
    #[serde(default)]
    pub memory_generator: MemoryGeneratorConfig,
    #[serde(default)]
    pub entity_extraction: EntityExtractionConfig,
    #[serde(default)]
    pub contradiction: ContradictionConfig,
    #[serde(default)]
    pub knowledge_extraction: KnowledgeExtractionConfig,
    #[serde(default)]
    pub profile: ProfileConfig,
    /// Stream-kind-aware manifest for shared/project streams (ADR-014,
    /// cycle/139). `#[serde(default)]` so configs without `[manifest]` load
    /// and degrade to the deterministic minimum.
    #[serde(default)]
    pub manifest: ManifestConfig,
    #[serde(default)]
    pub dream: DreamConfig,
    #[serde(default)]
    pub retention: RetentionConfig,
    #[serde(default)]
    pub event_log: EventLogConfig,
    #[serde(default)]
    pub associator: AssociatorConfig,
    #[serde(default)]
    pub advisor: AdvisorConfig,
    /// Feedback write-side endpoint configuration (cycle /112).
    #[serde(default)]
    pub feedback: FeedbackConfig,
    /// Content-type classification config (ADR-017, cycle /142). `#[serde(default)]`
    /// so configs without `[content_type]` load (deterministic-only, no LLM HTTP).
    #[serde(default)]
    pub content_type: ContentTypeConfig,
    /// Access-audit subsystem (ADR-018, cycle /150e). `#[serde(default)]` so
    /// configs without `[access_audit]` load and degrade to off (no records).
    #[serde(default)]
    pub access_audit: AccessAuditConfig,
    /// Per-stream rate limiting (audit 2026-07-01 item 3). `#[serde(default)]`
    /// so configs without `[rate_limit]` load and degrade to off.
    #[serde(default)]
    pub rate_limit: RateLimitConfig,
    /// MCP `memory_search` top_k defaults/caps (roadmap W2). `#[serde(default)]`
    /// so configs without `[mcp]` load and keep the previous hardcoded
    /// behavior (5/20 normal, 30/30 aggregation) byte-identically.
    #[serde(default)]
    pub mcp: McpConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetentionConfig {
    #[serde(default = "default_soft_delete_days")]
    pub soft_delete_days: u64,
    #[serde(default = "default_purge_interval")]
    pub hard_purge_interval_secs: u64,
}

fn default_soft_delete_days() -> u64 {
    30
}
fn default_purge_interval() -> u64 {
    86400
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            soft_delete_days: 30,
            hard_purge_interval_secs: 86400,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionConfig {
    pub interval_secs: u64,
    pub timeout_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupConfig {
    #[serde(default = "default_backup_enabled")]
    pub enabled: bool,
    #[serde(default = "default_backup_interval")]
    pub interval_secs: u64,
    #[serde(default = "default_backup_max_copies")]
    pub max_copies: usize,
}

fn default_backup_enabled() -> bool {
    false
}
fn default_backup_interval() -> u64 {
    86400
} // 24h
fn default_backup_max_copies() -> usize {
    3
}

impl Default for BackupConfig {
    fn default() -> Self {
        Self {
            enabled: default_backup_enabled(),
            interval_secs: default_backup_interval(),
            max_copies: default_backup_max_copies(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    #[serde(default = "default_auth_token_env")]
    pub auth_token_env: String,
    /// Cycle /001 (MemIR): when true, MCP `memory_store` honors the caller's
    /// `source` for the trust tier (allowing `a1`). When false (default), MCP
    /// writes are clamped to at most `a2`, so an untrusted or prompt-injected
    /// client cannot self-elevate to full-trust `a1`. Single-user/dogfood
    /// instances may enable it; multi-client/cloud should leave it off.
    #[serde(default)]
    pub honor_caller_trust_source: bool,
}

fn default_auth_token_env() -> String {
    "LOOMEM_AUTH_TOKEN".to_string()
}

/// Per-stream token-bucket rate limiting for the hot request paths (HTTP
/// `/v1`/`/api` + MCP tools) — audit 2026-07-01 item 3.
///
/// Declared here rather than next to `loomem-server/src/rate_limiter.rs`
/// because the root [`Config`] composes it and `loomem-core` cannot depend on
/// server types — same precedent as [`ServerConfig`]. Enforcement lives in
/// `loomem-server`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateLimitConfig {
    #[serde(default = "default_rate_limit_enabled")]
    pub enabled: bool,
    /// Max requests per minute for search/read operations (per stream).
    #[serde(default = "default_search_rpm")]
    pub search_rpm: u32,
    /// Max requests per minute for store/ingest operations (per stream).
    #[serde(default = "default_store_rpm")]
    pub store_rpm: u32,
    /// Max requests per minute for destructive/expensive operations
    /// (delete/purge/dream) per stream.
    #[serde(default = "default_delete_rpm")]
    pub delete_rpm: u32,
}

fn default_rate_limit_enabled() -> bool {
    false
}
fn default_search_rpm() -> u32 {
    120
}
fn default_store_rpm() -> u32 {
    60
}
fn default_delete_rpm() -> u32 {
    10
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            enabled: default_rate_limit_enabled(),
            search_rpm: default_search_rpm(),
            store_rpm: default_store_rpm(),
            delete_rpm: default_delete_rpm(),
        }
    }
}

/// MCP transport-layer limits for `memory_search` (roadmap W2): the `top_k`
/// default applied when the caller omits the parameter, and the cap applied
/// to explicit requests — separately for normal and aggregation-detected
/// queries. Previously hardcoded in the dispatcher (5/20 and 30/30), which
/// blocked any experiment with `top_k > 20` over MCP.
///
/// Declared here rather than next to `loomem-server/src/mcp/dispatcher.rs`
/// because the root [`Config`] composes it and `loomem-core` cannot depend on
/// server types — same precedent as [`ServerConfig`] and [`RateLimitConfig`].
/// Enforcement lives in the MCP dispatcher.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpConfig {
    /// `top_k` used when a normal (non-aggregation) search omits it.
    #[serde(default = "default_mcp_search_default_top_k")]
    pub search_default_top_k: usize,
    /// Hard cap on `top_k` for normal searches, explicit or defaulted.
    #[serde(default = "default_mcp_search_max_top_k")]
    pub search_max_top_k: usize,
    /// `top_k` used when an aggregation-detected search omits it.
    #[serde(default = "default_mcp_aggregation_default_top_k")]
    pub aggregation_default_top_k: usize,
    /// Hard cap on `top_k` for aggregation-detected searches.
    #[serde(default = "default_mcp_aggregation_max_top_k")]
    pub aggregation_max_top_k: usize,
}

fn default_mcp_search_default_top_k() -> usize {
    5
}
fn default_mcp_search_max_top_k() -> usize {
    20
}
fn default_mcp_aggregation_default_top_k() -> usize {
    30
}
fn default_mcp_aggregation_max_top_k() -> usize {
    30
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            search_default_top_k: default_mcp_search_default_top_k(),
            search_max_top_k: default_mcp_search_max_top_k(),
            aggregation_default_top_k: default_mcp_aggregation_default_top_k(),
            aggregation_max_top_k: default_mcp_aggregation_max_top_k(),
        }
    }
}

impl McpConfig {
    /// The effective `top_k` for an MCP `memory_search` call: the explicit
    /// caller value if present (never silently overridden by complexity
    /// tiers), else the configured default — both clamped to the configured
    /// cap for the query class. Mirrors the dispatcher's previous hardcoded
    /// `args.top_k.unwrap_or(default).min(cap)` byte-for-byte under shipped
    /// values.
    pub fn effective_search_top_k(&self, requested: Option<usize>, is_aggregation: bool) -> usize {
        let (default, cap) = if is_aggregation {
            (self.aggregation_default_top_k, self.aggregation_max_top_k)
        } else {
            (self.search_default_top_k, self.search_max_top_k)
        };
        requested.unwrap_or(default).min(cap)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceGuardsConfig {
    pub max_cpu_cores: f64,
    pub max_memory_mb: usize,
    pub min_disk_space_mb: u64,
    pub llm_timeout_secs: u64,
    pub worker_timeout_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamsConfig {
    pub shared: String,
    pub agents: HashMap<String, AgentStreams>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentStreams {
    pub raw: String,
    pub compressed: String,
}

impl Default for StreamsConfig {
    fn default() -> Self {
        Self {
            shared: "001".to_string(),
            agents: HashMap::new(),
        }
    }
}

impl Default for ResourceGuardsConfig {
    fn default() -> Self {
        Self {
            max_cpu_cores: 1.0,
            max_memory_mb: 512,
            min_disk_space_mb: 1024,
            llm_timeout_secs: 30,
            worker_timeout_secs: 60,
        }
    }
}

impl Config {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read config file: {}", path.display()))?;

        let mut config: Config = toml::from_str(&content)
            .with_context(|| format!("Failed to parse config file: {}", path.display()))?;

        config.manifest.apply_env_overrides(); // Env override for manifest governance (per-instance; ADR-015)
        config.content_type.apply_env_overrides(); // LOOMEM_CONTENT_TYPE_ENABLED per-instance typing toggle (/143)
        config.access_audit.apply_env_overrides(); // LOOMEM_ACCESS_AUDIT_ENABLED per-instance access-audit toggle (ADR-018, /150e)
        config.llm.apply_env_overrides(); // LOOMEM_EMBEDDING_PROVIDER / _DIM per-instance embedding override (cloud/Docker)
        config.worker.consolidation.apply_env_overrides(); // LOOMEM_CONSOLIDATION_INTERVAL_SECS per-instance (cloud)

        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        // Storage validation
        anyhow::ensure!(
            self.storage.rocksdb.max_open_files > 0,
            "rocksdb.max_open_files must be positive"
        );
        anyhow::ensure!(
            self.storage.rocksdb.write_buffer_size > 0,
            "rocksdb.write_buffer_size must be positive"
        );
        anyhow::ensure!(
            self.storage.rocksdb.max_write_buffer_number > 0,
            "rocksdb.max_write_buffer_number must be positive"
        );

        // MCP top_k limits: a default above its cap would silently truncate
        // every defaulted request — fail fast at load instead.
        anyhow::ensure!(
            self.mcp.search_max_top_k > 0 && self.mcp.aggregation_max_top_k > 0,
            "mcp.search_max_top_k and mcp.aggregation_max_top_k must be positive"
        );
        anyhow::ensure!(
            self.mcp.search_default_top_k <= self.mcp.search_max_top_k,
            "mcp.search_default_top_k must not exceed mcp.search_max_top_k"
        );
        anyhow::ensure!(
            self.mcp.aggregation_default_top_k <= self.mcp.aggregation_max_top_k,
            "mcp.aggregation_default_top_k must not exceed mcp.aggregation_max_top_k"
        );

        if self.storage.tantivy.enabled {
            anyhow::ensure!(
                self.storage.tantivy.heap_size_mb > 0,
                "tantivy.heap_size_mb must be positive when enabled"
            );
        }

        // Rate-limit validation (audit 2026-07-01 item 3): an enabled limiter
        // with rpm = 0 would reject every request in that category.
        if self.rate_limit.enabled {
            anyhow::ensure!(
                self.rate_limit.search_rpm > 0,
                "rate_limit.search_rpm must be positive when enabled"
            );
            anyhow::ensure!(
                self.rate_limit.store_rpm > 0,
                "rate_limit.store_rpm must be positive when enabled"
            );
            anyhow::ensure!(
                self.rate_limit.delete_rpm > 0,
                "rate_limit.delete_rpm must be positive when enabled"
            );
        }

        // Search validation
        anyhow::ensure!(
            self.search.hybrid_weights.vector >= 0.0 && self.search.hybrid_weights.vector <= 1.0,
            "hybrid_weights.vector must be between 0.0 and 1.0"
        );
        anyhow::ensure!(
            self.search.hybrid_weights.bm25 >= 0.0 && self.search.hybrid_weights.bm25 <= 1.0,
            "hybrid_weights.bm25 must be between 0.0 and 1.0"
        );
        anyhow::ensure!(
            self.search.decay.l0_lambda >= 0.0 && self.search.decay.l0_lambda <= 1.0,
            "decay.l0_lambda must be between 0.0 and 1.0"
        );
        anyhow::ensure!(
            self.search.decay.l1_lambda >= 0.0 && self.search.decay.l1_lambda <= 1.0,
            "decay.l1_lambda must be between 0.0 and 1.0"
        );
        anyhow::ensure!(
            self.search.surprise_boost >= 0.0,
            "surprise_boost must be non-negative"
        );
        anyhow::ensure!(self.search.top_k > 0, "top_k must be positive");

        // Worker validation
        anyhow::ensure!(
            self.worker.consolidation.interval_secs > 0,
            "consolidation.interval_secs must be positive"
        );
        anyhow::ensure!(
            self.worker.consolidation.batch_size > 0,
            "consolidation.batch_size must be positive"
        );
        anyhow::ensure!(
            self.worker.consolidation.concurrency > 0,
            "consolidation.concurrency must be positive"
        );
        anyhow::ensure!(
            self.worker.consolidation.timeout_secs > 0,
            "consolidation.timeout_secs must be positive"
        );

        anyhow::ensure!(
            self.worker.decay_worker.factor > 0.0 && self.worker.decay_worker.factor <= 1.0,
            "decay_worker.factor must be between 0.0 and 1.0 (exclusive of 0.0)"
        );
        anyhow::ensure!(
            self.worker.decay_worker.interval_secs > 0,
            "decay_worker.interval_secs must be positive"
        );

        anyhow::ensure!(
            self.worker.compaction.interval_secs > 0,
            "compaction.interval_secs must be positive"
        );

        anyhow::ensure!(
            self.worker.clustering.max_iterations > 0,
            "clustering.max_iterations must be positive"
        );
        anyhow::ensure!(
            self.worker.clustering.interval_secs > 0,
            "clustering.interval_secs must be positive"
        );

        // LLM validation
        anyhow::ensure!(
            !self.llm.provider.is_empty(),
            "llm.provider must not be empty"
        );
        anyhow::ensure!(
            !self.llm.api_key_env.is_empty(),
            "llm.api_key_env must not be empty"
        );
        anyhow::ensure!(
            !self.llm.embedding_model.is_empty(),
            "llm.embedding_model must not be empty"
        );
        anyhow::ensure!(
            self.llm.embedding_dim > 0,
            "llm.embedding_dim must be positive"
        );
        anyhow::ensure!(
            matches!(self.llm.embedding_provider.as_str(), "local" | "openai"),
            "llm.embedding_provider must be \"local\" or \"openai\" (got {:?})",
            self.llm.embedding_provider
        );
        anyhow::ensure!(
            self.llm.timeout_secs > 0,
            "llm.timeout_secs must be positive"
        );

        // Server validation
        anyhow::ensure!(
            !self.server.host.is_empty(),
            "server.host must not be empty"
        );
        anyhow::ensure!(self.server.port > 0, "server.port must be positive");

        // Resource guards validation
        anyhow::ensure!(
            self.resource_guards.max_cpu_cores > 0.0,
            "resource_guards.max_cpu_cores must be positive"
        );
        anyhow::ensure!(
            self.resource_guards.max_memory_mb > 0,
            "resource_guards.max_memory_mb must be positive"
        );
        anyhow::ensure!(
            self.resource_guards.min_disk_space_mb > 0,
            "resource_guards.min_disk_space_mb must be positive"
        );
        anyhow::ensure!(
            self.resource_guards.llm_timeout_secs > 0,
            "resource_guards.llm_timeout_secs must be positive"
        );
        anyhow::ensure!(
            self.resource_guards.worker_timeout_secs > 0,
            "resource_guards.worker_timeout_secs must be positive"
        );

        // Streams validation
        anyhow::ensure!(
            !self.streams.shared.is_empty(),
            "streams.shared must not be empty"
        );

        Ok(())
    }

    pub fn log_summary(&self) {
        tracing::info!("Configuration loaded and validated:");
        tracing::info!("  Storage: {:?}", self.storage.data_dir);
        tracing::info!(
            "  RocksDB: compression={}, max_open_files={}",
            self.storage.rocksdb.compression,
            self.storage.rocksdb.max_open_files
        );
        tracing::info!("  Vector search: enabled={}", self.storage.vector_enabled);
        tracing::info!("  Tantivy: enabled={}", self.storage.tantivy.enabled);
        tracing::info!(
            "  Intent log: enabled={}, sync_on_write={}",
            self.storage.intent_log.enabled,
            self.storage.intent_log.sync_on_write
        );
        let auth_active = !self.server.auth_token_env.is_empty()
            && std::env::var(&self.server.auth_token_env).is_ok();
        tracing::info!(
            "  Server: {}:{} (auth={})",
            self.server.host,
            self.server.port,
            if auth_active { "enabled" } else { "disabled" }
        );
        tracing::info!("  Scheduler: enabled={}", self.scheduler.enabled);
        tracing::info!(
            "  Resource guards: max_cpu={}, max_mem={}MB",
            self.resource_guards.max_cpu_cores,
            self.resource_guards.max_memory_mb
        );
        tracing::info!(
            "  Workers: consolidation={}s, decay={}s, compaction={}s, clustering={}s",
            self.worker.consolidation.interval_secs,
            self.worker.decay_worker.interval_secs,
            self.worker.compaction.interval_secs,
            self.worker.clustering.interval_secs
        );
        tracing::info!(
            "  LLM: provider={}, model={}, timeout={}s",
            self.llm.provider,
            self.llm.embedding_model,
            self.llm.timeout_secs
        );
        tracing::info!(
            "  Streams: shared={}, agents={}",
            self.streams.shared,
            self.streams.agents.len()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_toml_parses_event_log_enabled_true() {
        // /150a Gap 5: config.toml is the source of truth for event_log.enabled;
        // the code Default is `false` (event_log.rs). This guards against silent
        // drift — if config.toml ever loses `enabled = true`, the effective value
        // would flip without anyone noticing.
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../config.toml");
        let content = std::fs::read_to_string(path).expect("read config.toml");
        let cfg: Config = toml::from_str(&content).expect("parse config.toml");
        assert!(
            cfg.event_log.enabled,
            "config.toml [event_log].enabled must stay true (code Default is false)"
        );
    }

    /// Shipped config.toml must reproduce the previously hardcoded MCP
    /// memory_search limits byte-identically (roadmap W2): 5/20 normal,
    /// 30/30 aggregation. Guards against drift between the file and the
    /// code Default (which covers configs without an `[mcp]` section).
    #[test]
    fn config_toml_mcp_limits_match_previous_hardcoded_behavior() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../config.toml");
        let content = std::fs::read_to_string(path).expect("read config.toml");
        let cfg: Config = toml::from_str(&content).expect("parse config.toml");
        assert_eq!(cfg.mcp.search_default_top_k, 5);
        assert_eq!(cfg.mcp.search_max_top_k, 20);
        assert_eq!(cfg.mcp.aggregation_default_top_k, 30);
        assert_eq!(cfg.mcp.aggregation_max_top_k, 30);
        // And the code Default must agree with the shipped file.
        let dflt = McpConfig::default();
        assert_eq!(dflt.search_default_top_k, cfg.mcp.search_default_top_k);
        assert_eq!(dflt.search_max_top_k, cfg.mcp.search_max_top_k);
        assert_eq!(
            dflt.aggregation_default_top_k,
            cfg.mcp.aggregation_default_top_k
        );
        assert_eq!(dflt.aggregation_max_top_k, cfg.mcp.aggregation_max_top_k);
    }

    /// Clamp contract for the MCP dispatcher (roadmap W2): explicit beyond
    /// cap → cap; omitted → default; aggregation uses its own pair;
    /// config-driven values are respected (a raised cap admits top_k > 20).
    #[test]
    fn mcp_effective_search_top_k_clamps_and_defaults() {
        let shipped = McpConfig::default();
        // Omitted → default, per query class.
        assert_eq!(shipped.effective_search_top_k(None, false), 5);
        assert_eq!(shipped.effective_search_top_k(None, true), 30);
        // Explicit within cap → honored (never silently overridden).
        assert_eq!(shipped.effective_search_top_k(Some(12), false), 12);
        assert_eq!(shipped.effective_search_top_k(Some(8), true), 8);
        // Explicit beyond cap → clamped to cap.
        assert_eq!(shipped.effective_search_top_k(Some(40), false), 20);
        assert_eq!(shipped.effective_search_top_k(Some(99), true), 30);

        // Raised caps (bench-style config copy) admit larger windows.
        let raised = McpConfig {
            search_default_top_k: 5,
            search_max_top_k: 40,
            aggregation_default_top_k: 30,
            aggregation_max_top_k: 60,
        };
        assert_eq!(raised.effective_search_top_k(Some(40), false), 40);
        assert_eq!(raised.effective_search_top_k(Some(50), false), 40);
        assert_eq!(raised.effective_search_top_k(Some(60), true), 60);
        assert_eq!(raised.effective_search_top_k(None, false), 5);
    }

    /// A config without an `[mcp]` section (every pre-W2 deployment) must
    /// load and keep the previous hardcoded behavior.
    #[test]
    fn mcp_section_absent_defaults_to_previous_behavior() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../config.toml");
        let content = std::fs::read_to_string(path).expect("read config.toml");
        let without_mcp: String = content
            .lines()
            .scan(false, |in_mcp, line| {
                if line.trim() == "[mcp]" {
                    *in_mcp = true;
                } else if *in_mcp && line.trim_start().starts_with('[') {
                    *in_mcp = false;
                }
                Some(if *in_mcp { None } else { Some(line) })
            })
            .flatten()
            .collect::<Vec<_>>()
            .join("\n");
        let cfg: Config = toml::from_str(&without_mcp).expect("parse config.toml without [mcp]");
        assert_eq!(cfg.mcp.search_default_top_k, 5);
        assert_eq!(cfg.mcp.search_max_top_k, 20);
        assert_eq!(cfg.mcp.aggregation_default_top_k, 30);
        assert_eq!(cfg.mcp.aggregation_max_top_k, 30);
    }

    /// validate() rejects a default above its cap (silent truncation of every
    /// defaulted request) and non-positive caps.
    #[test]
    fn mcp_validate_rejects_default_above_cap() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../config.toml");
        let content = std::fs::read_to_string(path).expect("read config.toml");
        let mut cfg: Config = toml::from_str(&content).expect("parse config.toml");
        cfg.mcp.search_default_top_k = 25; // above search_max_top_k = 20
        let err = cfg.validate().expect_err("default above cap must fail");
        assert!(
            err.to_string().contains("search_default_top_k"),
            "unexpected error: {err}"
        );
    }
}
