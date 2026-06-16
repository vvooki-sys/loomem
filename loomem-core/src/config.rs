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
}

fn default_auth_token_env() -> String {
    "LOOMEM_AUTH_TOKEN".to_string()
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

        if self.storage.tantivy.enabled {
            anyhow::ensure!(
                self.storage.tantivy.heap_size_mb > 0,
                "tantivy.heap_size_mb must be positive when enabled"
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
}
