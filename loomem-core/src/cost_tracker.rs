use anyhow::{Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::storage::RocksDbStore;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostConfig {
    pub daily_cap_usd: f64,
    pub alert_threshold_usd: f64,
    pub anomaly_multiplier: f64,
    pub persist: bool,
}

impl Default for CostConfig {
    fn default() -> Self {
        Self {
            daily_cap_usd: 0.50,
            alert_threshold_usd: 0.30,
            anomaly_multiplier: 3.0,
            persist: true,
        }
    }
}

const CF_COSTS: &str = "costs";

// Hardcoded pricing (USD per 1M tokens)
const EMBED_INPUT_PRICE: f64 = 0.02;
const GPT4O_MINI_INPUT_PRICE: f64 = 0.15;
const GPT4O_MINI_OUTPUT_PRICE: f64 = 0.60;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DailyCost {
    pub date: String,
    pub total_usd: f64,
    pub embed_calls: u64,
    pub compress_calls: u64,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
}

impl Default for DailyCost {
    fn default() -> Self {
        Self {
            date: Utc::now().format("%Y-%m-%d").to_string(),
            total_usd: 0.0,
            embed_calls: 0,
            compress_calls: 0,
            total_input_tokens: 0,
            total_output_tokens: 0,
        }
    }
}

#[derive(Debug)]
pub struct BudgetExceeded {
    pub current: f64,
    pub limit: f64,
}

impl std::fmt::Display for BudgetExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Daily budget exceeded: ${:.4} / ${:.2}",
            self.current, self.limit
        )
    }
}

impl std::error::Error for BudgetExceeded {}

pub struct CostTracker {
    store: Arc<RocksDbStore>,
    config: CostConfig,
    last_alert: Arc<Mutex<Option<Instant>>>,
    telegram_chat_id: Option<String>,
    http_client: reqwest::Client,
}

impl CostTracker {
    pub fn new(store: Arc<RocksDbStore>, config: CostConfig, http_client: reqwest::Client) -> Self {
        let telegram_chat_id = std::env::var("LOOMEM_TELEGRAM_CHAT_ID").ok();

        if telegram_chat_id.is_some() {
            info!("Telegram alerts enabled for cost tracking");
        }

        Self {
            store,
            config,
            last_alert: Arc::new(Mutex::new(None)),
            telegram_chat_id,
            http_client,
        }
    }

    fn today_key(workspace_id: Option<&str>) -> String {
        let date = Utc::now().format("%Y-%m-%d");
        match workspace_id {
            Some(ws) => format!("cost:{}:{}", ws, date),
            None => format!("cost:{}", date), // global key for backward compat
        }
    }

    fn get_cost(&self, workspace_id: Option<&str>) -> Result<DailyCost> {
        let cf = self
            .store
            .db()
            .cf_handle(CF_COSTS)
            .ok_or_else(|| anyhow::anyhow!("Costs column family not found"))?;
        let key = Self::today_key(workspace_id);

        match self.store.db().get_cf(cf, key.as_bytes())? {
            Some(bytes) => {
                let cost: DailyCost =
                    serde_json::from_slice(&bytes).context("Failed to deserialize daily cost")?;
                Ok(cost)
            }
            None => Ok(DailyCost::default()),
        }
    }

    fn get_today(&self) -> Result<DailyCost> {
        self.get_cost(None)
    }

    fn save_cost(&self, cost: &DailyCost, workspace_id: Option<&str>) -> Result<()> {
        if !self.config.persist {
            return Ok(());
        }

        let cf = self
            .store
            .db()
            .cf_handle(CF_COSTS)
            .ok_or_else(|| anyhow::anyhow!("Costs column family not found"))?;
        let key = Self::today_key(workspace_id);
        let value = serde_json::to_vec(cost).context("Failed to serialize daily cost")?;

        self.store
            .db()
            .put_cf(&cf, key.as_bytes(), &value)
            .context("Failed to save daily cost")?;

        Ok(())
    }

    #[allow(dead_code)] // internal helper kept for symmetry with save_cost; may be used by future daily rollup
    fn save_today(&self, cost: &DailyCost) -> Result<()> {
        self.save_cost(cost, None)
    }

    /// Record cost for a specific workspace. Also updates global counter.
    pub fn record_for_workspace(
        &self,
        tokens_in: u64,
        tokens_out: u64,
        model: &str,
        workspace_id: &str,
    ) -> Result<()> {
        // Record per-workspace
        self.record_internal(tokens_in, tokens_out, model, Some(workspace_id))?;
        // Also record global
        self.record_internal(tokens_in, tokens_out, model, None)
    }

    pub fn record(&self, tokens_in: u64, tokens_out: u64, model: &str) -> Result<()> {
        self.record_internal(tokens_in, tokens_out, model, None)
    }

    fn record_internal(
        &self,
        tokens_in: u64,
        tokens_out: u64,
        model: &str,
        workspace_id: Option<&str>,
    ) -> Result<()> {
        let mut cost = self.get_cost(workspace_id)?;

        // Calculate cost based on model
        let cost_usd = match model {
            m if m.starts_with("text-embedding") => {
                cost.embed_calls += 1;
                (tokens_in as f64 / 1_000_000.0) * EMBED_INPUT_PRICE
            }
            "gpt-4o-mini" | "gpt-4.1-mini" => {
                cost.compress_calls += 1;
                let input_cost = (tokens_in as f64 / 1_000_000.0) * GPT4O_MINI_INPUT_PRICE;
                let output_cost = (tokens_out as f64 / 1_000_000.0) * GPT4O_MINI_OUTPUT_PRICE;
                input_cost + output_cost
            }
            _ => {
                warn!("Unknown model for cost tracking: {}", model);
                0.0
            }
        };

        cost.total_usd += cost_usd;
        cost.total_input_tokens += tokens_in;
        cost.total_output_tokens += tokens_out;

        self.save_cost(&cost, workspace_id)?;

        // Check for alert conditions (on global cost)
        if workspace_id.is_none() && cost.total_usd >= self.config.alert_threshold_usd {
            self.check_alert_conditions(&cost);
        }

        Ok(())
    }

    /// Check budget for a specific workspace.
    pub fn check_budget_for_workspace(&self, workspace_id: &str) -> Result<(), BudgetExceeded> {
        let cost = self.get_cost(Some(workspace_id)).map_err(|e| {
            warn!("Failed to get workspace cost: {}", e);
            BudgetExceeded {
                current: 0.0,
                limit: self.config.daily_cap_usd,
            }
        })?;
        if cost.total_usd >= self.config.daily_cap_usd {
            Err(BudgetExceeded {
                current: cost.total_usd,
                limit: self.config.daily_cap_usd,
            })
        } else {
            Ok(())
        }
    }

    pub fn check_budget(&self) -> Result<(), BudgetExceeded> {
        let cost = self.get_today().map_err(|e| {
            warn!("Failed to get today's cost: {}", e);
            BudgetExceeded {
                current: 0.0,
                limit: self.config.daily_cap_usd,
            }
        })?;

        if cost.total_usd >= self.config.daily_cap_usd {
            Err(BudgetExceeded {
                current: cost.total_usd,
                limit: self.config.daily_cap_usd,
            })
        } else {
            Ok(())
        }
    }

    pub fn daily_summary(&self) -> DailyCost {
        self.get_today().unwrap_or_default()
    }

    fn check_alert_conditions(&self, today: &DailyCost) {
        // Budget exceeded alert
        if today.total_usd >= self.config.daily_cap_usd {
            let message = format!(
                "⚠️ Loomem: Daily budget exceeded!\nSpent: ${:.4}\nLimit: ${:.2}",
                today.total_usd, self.config.daily_cap_usd
            );
            self.send_alert(&message);
            return;
        }

        // Anomaly detection (cost > 3x 7-day average)
        if let Ok(avg) = self.get_7day_average() {
            if today.total_usd > avg * self.config.anomaly_multiplier {
                let message = format!(
                    "⚠️ Loomem: Cost anomaly detected!\nToday: ${:.4}\n7-day avg: ${:.4}\nThreshold: {:.1}x",
                    today.total_usd, avg, self.config.anomaly_multiplier
                );
                self.send_alert(&message);
            }
        }
    }

    fn get_7day_average(&self) -> Result<f64> {
        let cf = self
            .store
            .db()
            .cf_handle(CF_COSTS)
            .ok_or_else(|| anyhow::anyhow!("Costs column family not found"))?;
        let mut total = 0.0;
        let mut count = 0;

        // Get last 7 days
        for i in 1..=7 {
            let date = Utc::now() - chrono::Duration::days(i);
            let key = format!("cost:{}", date.format("%Y-%m-%d"));

            if let Some(bytes) = self.store.db().get_cf(cf, key.as_bytes())? {
                if let Ok(cost) = serde_json::from_slice::<DailyCost>(&bytes) {
                    total += cost.total_usd;
                    count += 1;
                }
            }
        }

        if count > 0 {
            Ok(total / count as f64)
        } else {
            Ok(0.0)
        }
    }

    fn send_alert(&self, message: &str) {
        // Debounce: max 1 alert per hour
        let mut last_alert = match self.last_alert.try_lock() {
            Ok(guard) => guard,
            Err(_) => return, // Lock contention, skip
        };

        if let Some(last) = *last_alert {
            if last.elapsed() < std::time::Duration::from_secs(3600) {
                return; // Too soon
            }
        }

        *last_alert = Some(Instant::now());
        drop(last_alert);

        // Send Telegram alert
        if let Some(ref chat_id) = self.telegram_chat_id {
            let client = self.http_client.clone();
            let chat_id = chat_id.clone();
            let message = message.to_string();

            tokio::spawn(async move {
                let bot_token = match std::env::var("TELEGRAM_BOT_TOKEN") {
                    Ok(token) => token,
                    Err(_) => {
                        warn!("TELEGRAM_BOT_TOKEN not set, skipping alert");
                        return;
                    }
                };

                let url = format!("https://api.telegram.org/bot{}/sendMessage", bot_token);
                let params = [("chat_id", chat_id.as_str()), ("text", message.as_str())];

                let result: Result<reqwest::Response, _> =
                    client.post(&url).form(&params).send().await;
                match result {
                    Ok(resp) if resp.status().is_success() => {
                        info!("Telegram alert sent successfully");
                    }
                    Ok(resp) => {
                        warn!("Telegram alert failed: status {}", resp.status());
                    }
                    Err(e) => {
                        warn!("Failed to send Telegram alert: {}", e);
                    }
                }
            });
        }
    }
}

// ---- ECA-31: Cost guardrails ----

/// Per-stream cost budget status tiers.
/// Determines which expensive operations should be disabled based on usage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CostBudgetStatus {
    /// Under 80% of daily budget — all features enabled.
    Normal,
    /// 80-95% of daily budget — disable reranking to save tokens.
    RerankerDisabled,
    /// 95-100% of daily budget — disable associations (most expensive).
    AssociationsDisabled,
    /// Over 100% of daily budget — log-only mode, no LLM calls.
    LogOnly,
}

impl CostBudgetStatus {
    /// Returns a human-readable description of the budget tier.
    pub fn description(&self) -> &'static str {
        match self {
            CostBudgetStatus::Normal => "Normal operation",
            CostBudgetStatus::RerankerDisabled => "Reranker disabled (80-95% budget)",
            CostBudgetStatus::AssociationsDisabled => "Associations disabled (95-100% budget)",
            CostBudgetStatus::LogOnly => "Log-only mode (budget exceeded)",
        }
    }

    /// Whether reranking should be allowed in this tier.
    pub fn allow_reranker(&self) -> bool {
        matches!(self, CostBudgetStatus::Normal)
    }

    /// Whether associations should be allowed in this tier.
    pub fn allow_associations(&self) -> bool {
        matches!(
            self,
            CostBudgetStatus::Normal | CostBudgetStatus::RerankerDisabled
        )
    }

    /// Whether LLM calls (consolidation, extraction) should be allowed.
    pub fn allow_llm_calls(&self) -> bool {
        !matches!(self, CostBudgetStatus::LogOnly)
    }
}

/// Check cost budget status for a stream based on today's token usage.
///
/// Uses the CostTracker's daily cost data and the configured daily cap
/// to determine which tier of operations should be available.
///
/// This is advisory — it logs warnings but does not hard-block operations.
pub fn check_cost_budget(store: &RocksDbStore, daily_budget_usd: f64) -> CostBudgetStatus {
    let today_key = format!("cost:{}", Utc::now().format("%Y-%m-%d"));

    // Try to read from the costs column family
    let cost_opt = store
        .db()
        .cf_handle("costs")
        .and_then(|cf| store.db().get_cf(cf, today_key.as_bytes()).ok().flatten());

    let today_cost = match cost_opt {
        Some(bytes) => serde_json::from_slice::<DailyCost>(&bytes)
            .map(|c| c.total_usd)
            .unwrap_or(0.0),
        None => 0.0,
    };

    if daily_budget_usd <= 0.0 {
        // No budget configured — always normal
        return CostBudgetStatus::Normal;
    }

    let usage_pct = today_cost / daily_budget_usd;

    let status = if usage_pct >= 1.0 {
        CostBudgetStatus::LogOnly
    } else if usage_pct >= 0.95 {
        CostBudgetStatus::AssociationsDisabled
    } else if usage_pct >= 0.80 {
        CostBudgetStatus::RerankerDisabled
    } else {
        CostBudgetStatus::Normal
    };

    if !matches!(status, CostBudgetStatus::Normal) {
        warn!(
            "Cost guardrail active: {:.1}% of ${:.2} budget used (${:.4}). Status: {}",
            usage_pct * 100.0,
            daily_budget_usd,
            today_cost,
            status.description()
        );
    }

    status
}

/// Check cost budget using per-stream workspace data.
pub fn check_cost_budget_for_stream(
    store: &RocksDbStore,
    stream_id: &str,
    daily_budget_usd: f64,
) -> CostBudgetStatus {
    let today_key = format!("cost:{}:{}", stream_id, Utc::now().format("%Y-%m-%d"));

    let cost_opt = store
        .db()
        .cf_handle("costs")
        .and_then(|cf| store.db().get_cf(cf, today_key.as_bytes()).ok().flatten());

    let today_cost = match cost_opt {
        Some(bytes) => serde_json::from_slice::<DailyCost>(&bytes)
            .map(|c| c.total_usd)
            .unwrap_or(0.0),
        None => 0.0,
    };

    if daily_budget_usd <= 0.0 {
        return CostBudgetStatus::Normal;
    }

    let usage_pct = today_cost / daily_budget_usd;

    if usage_pct >= 1.0 {
        CostBudgetStatus::LogOnly
    } else if usage_pct >= 0.95 {
        CostBudgetStatus::AssociationsDisabled
    } else if usage_pct >= 0.80 {
        CostBudgetStatus::RerankerDisabled
    } else {
        CostBudgetStatus::Normal
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RocksDbConfig;
    use tempfile::TempDir;

    fn create_test_store() -> (TempDir, Arc<RocksDbStore>) {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let config = RocksDbConfig {
            max_open_files: 100,
            compression: "none".to_string(),
            write_buffer_size: 4 * 1024 * 1024,
            max_write_buffer_number: 2,
        };

        let store =
            RocksDbStore::open(temp_dir.path(), &config).expect("Failed to open test store");

        (temp_dir, Arc::new(store))
    }

    #[test]
    fn test_budget_exceeded() {
        let (_temp, store) = create_test_store();
        let config = CostConfig {
            daily_cap_usd: 0.10,
            ..Default::default()
        };
        let client = reqwest::Client::new();
        let tracker = CostTracker::new(store, config, client);

        // Record costs — 10M tokens at $0.02/1M = $0.20, exceeding $0.10 cap
        tracker
            .record(10_000_000, 0, "text-embedding-3-small")
            .expect("Failed to record");

        // Should exceed budget
        let result = tracker.check_budget();
        assert!(result.is_err());
    }
}
