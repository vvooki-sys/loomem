//! Persisted conservative charges reserve selector budget before network dispatch.
//! Unknown usage retains its charge, including cancellation and process restarts.
use anyhow::{Context, Result};
use serde_json::Value;

use super::{CostTracker, DailyCost, CF_COSTS, COST_WRITE_LOCK};

/// Scoped accounting for a single selector invocation.
#[derive(Clone, Copy)]
pub struct SelectorAccounting<'a> {
    pub tracker: &'a CostTracker,
    pub stream_id: Option<&'a str>,
}

/// Validated provider-reported billed token counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectorUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
}

impl SelectorUsage {
    /// Parse usage independently of output/schema validity; malformed usage is unknown.
    pub fn from_response(response: &Value) -> Option<Self> {
        let usage = response.get("usage")?;
        let input_tokens = u32::try_from(usage.get("prompt_tokens")?.as_u64()?).ok()?;
        let output_tokens = u32::try_from(usage.get("completion_tokens")?.as_u64()?).ok()?;
        let total = usage.get("total_tokens")?.as_u64()?;
        (u64::from(input_tokens) + u64::from(output_tokens) == total).then_some(Self {
            input_tokens,
            output_tokens,
        })
    }
}

/// An already-persisted conservative charge; dropping intentionally retains unknown usage.
pub struct SelectorReservation<'a> {
    tracker: &'a CostTracker,
    keys: Vec<String>,
    reserved: f64,
    prices: (f64, f64),
    pending_keys: Vec<String>,
    input_bound: u32,
    model: String,
}

impl<'a> SelectorAccounting<'a> {
    /// Reserve the full model context and the provider output ceiling.
    /// Unknown models and disabled persistence fail closed before dispatch.
    pub fn reserve(self, model: &str) -> Result<SelectorReservation<'a>> {
        let date = chrono::Utc::now().format("%Y-%m-%d").to_string();
        self.reserve_on_day(model, &date)
    }

    fn reserve_on_day(self, model: &str, date: &str) -> Result<SelectorReservation<'a>> {
        let prices = prices(model).context("Selector model pricing unavailable")?;
        anyhow::ensure!(
            self.tracker.config.daily_cap_usd.is_finite()
                && self.tracker.config.daily_cap_usd > 0.0,
            "Selector budget invalid"
        );
        anyhow::ensure!(
            self.tracker.config.persist,
            "Selector accounting requires persistence"
        );
        // Full provider context avoids assumptions about hidden framing/tokenization.
        let input_bound = if model.starts_with("gpt-4o-mini") {
            128_000
        } else {
            1_047_576
        };
        let reserved = charge(input_bound, 1024, prices);
        let _lock = COST_WRITE_LOCK
            .lock()
            .map_err(|_| anyhow::anyhow!("Cost lock unavailable"))?;
        let mut keys = vec![format!("cost:{date}")];
        if let Some(stream) = self.stream_id {
            keys.push(format!("cost:{stream}:{date}"));
        }
        let pending_keys = pending_keys(self.stream_id);
        let pending = read_pending(self.tracker, &pending_keys)?;
        let mut costs = read_costs(self.tracker, &keys)?;
        for (cost, pending) in costs.iter_mut().zip(&pending) {
            let committed = cost.total_usd - cost.selector_reserved_usd + pending;
            anyhow::ensure!(
                committed + reserved <= self.tracker.config.daily_cap_usd,
                "Selector budget unavailable"
            );
            cost.total_usd += reserved;
            cost.selector_calls += 1;
            cost.selector_unknown_usage_calls += 1;
            cost.selector_reserved_usd += reserved;
        }
        let pending: Vec<_> = pending.iter().map(|amount| amount + reserved).collect();
        save_costs(self.tracker, &keys, &costs, &pending_keys, &pending)?;
        Ok(SelectorReservation {
            tracker: self.tracker,
            keys,
            reserved,
            prices,
            pending_keys,
            input_bound,
            model: model.to_string(),
        })
    }
}

impl SelectorReservation<'_> {
    /// Reconcile known billed usage once, even when the selected output is invalid.
    pub fn reconcile(self, response: &Value) -> Result<()> {
        anyhow::ensure!(
            response.get("service_tier").and_then(Value::as_str) == Some("default"),
            "Selector processing tier unavailable or unsupported"
        );
        let usage = SelectorUsage::from_response(response).context("Selector usage unavailable")?;
        anyhow::ensure!(usage.input_tokens > 0, "Selector prompt usage invalid");
        let returned_model = response
            .get("model")
            .and_then(Value::as_str)
            .context("Selector model unavailable")?;
        anyhow::ensure!(
            same_model(&self.model, returned_model),
            "Selector model mismatch"
        );
        let cached = cached_tokens(response).context("Selector cached usage invalid")?;
        anyhow::ensure!(
            cached <= usage.input_tokens,
            "Selector cached usage exceeds input"
        );
        let discount = if self.prices.0 == 0.15 { 0.5 } else { 0.75 };
        let actual = charge(usage.input_tokens, usage.output_tokens, self.prices)
            - f64::from(cached) * self.prices.0 * discount / 1_000_000.0;
        self.settle(usage, actual)?;
        anyhow::ensure!(
            usage.input_tokens <= self.input_bound && usage.output_tokens <= 1024,
            "Selector reported usage exceeds reservation estimate"
        );
        Ok(())
    }

    fn settle(&self, usage: SelectorUsage, actual: f64) -> Result<()> {
        let _lock = COST_WRITE_LOCK
            .lock()
            .map_err(|_| anyhow::anyhow!("Cost lock unavailable"))?;
        let mut costs = read_costs(self.tracker, &self.keys)?;
        for cost in &mut costs {
            cost.total_usd = (cost.total_usd - self.reserved).max(0.0) + actual;
            cost.selector_reserved_usd = (cost.selector_reserved_usd - self.reserved).max(0.0);
            cost.selector_unknown_usage_calls = cost.selector_unknown_usage_calls.saturating_sub(1);
            cost.total_input_tokens += u64::from(usage.input_tokens);
            cost.total_output_tokens += u64::from(usage.output_tokens);
        }
        let pending: Vec<_> = read_pending(self.tracker, &self.pending_keys)?
            .iter()
            .map(|amount| (amount - self.reserved).max(0.0))
            .collect();
        save_costs(
            self.tracker,
            &self.keys,
            &costs,
            &self.pending_keys,
            &pending,
        )?;
        Ok(())
    }
}

// Standard text rates verified 2026-10-20 against developers.openai.com/api/docs/models/
// gpt-4.1, gpt-4.1-mini, and gpt-4o-mini; snapshots intentionally enumerated.
fn prices(model: &str) -> Option<(f64, f64)> {
    match model {
        "gpt-4o-mini" | "gpt-4o-mini-2024-07-18" => Some((0.15, 0.60)),
        "gpt-4.1-mini" | "gpt-4.1-mini-2025-04-14" => Some((0.40, 1.60)),
        "gpt-4.1" | "gpt-4.1-2025-04-14" => Some((2.0, 8.0)),
        _ => None,
    }
}

fn charge(input: u32, output: u32, prices: (f64, f64)) -> f64 {
    (f64::from(input) * prices.0 + f64::from(output) * prices.1) / 1_000_000.0
}

fn read_costs(tracker: &CostTracker, keys: &[String]) -> Result<Vec<DailyCost>> {
    let cf = tracker
        .store
        .db()
        .cf_handle(CF_COSTS)
        .context("Costs column family missing")?;
    keys.iter()
        .map(|key| {
            let cost: DailyCost = match tracker.store.db().get_cf(&cf, key.as_bytes())? {
                Some(bytes) => serde_json::from_slice(&bytes).context("Invalid cost data")?,
                None => DailyCost {
                    date: key.rsplit(':').next().unwrap_or_default().to_string(),
                    ..DailyCost::default()
                },
            };
            anyhow::ensure!(
                cost.total_usd.is_finite()
                    && cost.selector_reserved_usd.is_finite()
                    && cost.total_usd >= 0.0
                    && cost.selector_reserved_usd >= 0.0
                    && cost.selector_reserved_usd <= cost.total_usd,
                "Invalid cost totals"
            );
            Ok(cost)
        })
        .collect()
}

fn save_costs(
    tracker: &CostTracker,
    keys: &[String],
    costs: &[DailyCost],
    pending_keys: &[String],
    pending: &[f64],
) -> Result<()> {
    let cf = tracker
        .store
        .db()
        .cf_handle(CF_COSTS)
        .context("Costs column family missing")?;
    let mut batch = rocksdb::WriteBatch::default();
    for (key, cost) in keys.iter().zip(costs) {
        batch.put_cf(&cf, key.as_bytes(), serde_json::to_vec(cost)?);
    }
    for (key, amount) in pending_keys.iter().zip(pending) {
        batch.put_cf(&cf, key.as_bytes(), serde_json::to_vec(amount)?);
    }
    let mut options = rocksdb::WriteOptions::default();
    options.set_sync(true);
    tracker
        .store
        .db()
        .write_opt(batch, &options)
        .context("Persist selector accounting")
}

fn pending_keys(stream: Option<&str>) -> Vec<String> {
    let mut keys = vec!["selector:pending:global".to_string()];
    if let Some(stream) = stream {
        keys.push(format!("selector:pending:stream:{stream}"));
    }
    keys
}

fn read_pending(tracker: &CostTracker, keys: &[String]) -> Result<Vec<f64>> {
    let cf = tracker
        .store
        .db()
        .cf_handle(CF_COSTS)
        .context("Costs column family missing")?;
    keys.iter()
        .map(|key| {
            let amount: f64 = match tracker.store.db().get_cf(&cf, key.as_bytes())? {
                Some(bytes) => {
                    serde_json::from_slice(&bytes).context("Invalid selector pending cost")?
                }
                None => 0.0,
            };
            anyhow::ensure!(
                amount.is_finite() && amount >= 0.0,
                "Invalid pending amount"
            );
            Ok(amount)
        })
        .collect()
}

fn same_model(requested: &str, returned: &str) -> bool {
    requested == returned
        || matches!(
            (requested, returned),
            ("gpt-4.1", "gpt-4.1-2025-04-14")
                | ("gpt-4.1-mini", "gpt-4.1-mini-2025-04-14")
                | ("gpt-4o-mini", "gpt-4o-mini-2024-07-18")
        )
}

fn cached_tokens(response: &Value) -> Option<u32> {
    match response.pointer("/usage/prompt_tokens_details") {
        None | Some(Value::Null) => Some(0),
        Some(details) => u32::try_from(details.get("cached_tokens")?.as_u64()?).ok(),
    }
}

#[cfg(test)]
mod tests;
