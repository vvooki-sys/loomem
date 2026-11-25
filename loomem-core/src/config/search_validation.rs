//! Search configuration validation, extracted to keep Config::validate bounded.
use crate::hybrid_search::{SearchCandidateBudget, SearchConfig};
use anyhow::{Context, Result};

pub(super) fn validate(search: &SearchConfig) -> Result<()> {
    anyhow::ensure!(
        search.hybrid_weights.vector >= 0.0 && search.hybrid_weights.vector <= 1.0,
        "hybrid_weights.vector must be between 0.0 and 1.0"
    );
    anyhow::ensure!(
        search.hybrid_weights.bm25 >= 0.0 && search.hybrid_weights.bm25 <= 1.0,
        "hybrid_weights.bm25 must be between 0.0 and 1.0"
    );
    anyhow::ensure!(
        search.decay.l0_lambda >= 0.0 && search.decay.l0_lambda <= 1.0,
        "decay.l0_lambda must be between 0.0 and 1.0"
    );
    anyhow::ensure!(
        search.decay.l1_lambda >= 0.0 && search.decay.l1_lambda <= 1.0,
        "decay.l1_lambda must be between 0.0 and 1.0"
    );
    anyhow::ensure!(
        search.surprise_boost >= 0.0,
        "surprise_boost must be non-negative"
    );
    validate_limits(search)?;
    validate_levers(search)
}

fn validate_limits(search: &SearchConfig) -> Result<()> {
    anyhow::ensure!(search.top_k > 0, "top_k must be positive");
    anyhow::ensure!(
        search.max_request_top_k > 0,
        "max_request_top_k must be positive"
    );
    anyhow::ensure!(
        search.top_k <= search.max_request_top_k,
        "search.top_k must not exceed max_request_top_k"
    );
    SearchCandidateBudget::new(search.top_k, search.max_request_top_k)
        .context("max_request_top_k exceeds supported candidate budget")?;
    Ok(())
}

/// Smallest non-zero `rerank_onnx_max_tokens`. The cap counts the
/// cross-encoder's special tokens (3 for a BERT pair: `[CLS] q [SEP] d
/// [SEP]`), and the tokenizer subtracts them from the cap unchecked, so a
/// cap at or below that count panics (debug) or wraps (release). 16 leaves
/// room for the special tokens plus a few tokens of query and document;
/// anything smaller could never be meant.
const MIN_RERANK_ONNX_MAX_TOKENS: usize = 16;

/// Quality-lever flags: every one is off/neutral by default; these checks
/// only reject values that could never be meant (`0` caps, negative or
/// non-finite weights). `rerank_onnx_max_tokens = 0` is a valid "off";
/// otherwise it must be at least [`MIN_RERANK_ONNX_MAX_TOKENS`].
fn validate_levers(search: &SearchConfig) -> Result<()> {
    anyhow::ensure!(
        search.rerank_llm_max_chars > 0,
        "search.rerank_llm_max_chars must be positive"
    );
    anyhow::ensure!(
        search.rerank_onnx_max_tokens == 0
            || search.rerank_onnx_max_tokens >= MIN_RERANK_ONNX_MAX_TOKENS,
        "search.rerank_onnx_max_tokens must be 0 (no truncation) or at least {MIN_RERANK_ONNX_MAX_TOKENS}"
    );
    anyhow::ensure!(
        search.rerank_sync_timeout_ms > 0,
        "search.rerank_sync_timeout_ms must be positive"
    );
    anyhow::ensure!(
        search.event_date_proximity_boost >= 0.0 && search.event_date_proximity_boost.is_finite(),
        "search.event_date_proximity_boost must be a finite non-negative number"
    );
    anyhow::ensure!(
        search.latest_state_boost >= 1.0 && search.latest_state_boost.is_finite(),
        "search.latest_state_boost must be a finite number >= 1.0 (1.0 = disabled)"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn config() -> Config {
        toml::from_str(include_str!("../../../config.toml")).unwrap()
    }

    #[test]
    fn validates_request_ceiling_and_arithmetic_at_config_load() {
        let mut config = config();
        config.validate().unwrap();
        config.search.max_request_top_k = 0;
        assert!(config.validate().is_err());
        config.search.max_request_top_k = config.search.top_k - 1;
        assert!(config.validate().is_err());
        config.search.max_request_top_k = usize::MAX;
        assert!(config.validate().is_err());
        config.search.max_request_top_k = 1000;
        config.validate().unwrap();
    }

    #[test]
    fn missing_new_limit_deserializes_compatible_default() {
        let original = config();
        let mut search = serde_json::to_value(&original.search).unwrap();
        search.as_object_mut().unwrap().remove("max_request_top_k");
        let decoded: SearchConfig = serde_json::from_value(search).unwrap();
        assert_eq!(decoded.max_request_top_k, 1000);
        assert_eq!(decoded.top_k, original.search.top_k);
    }

    const LEVER_KEYS: [&str; 8] = [
        "rerank_date_prefix",
        "rerank_llm_max_chars",
        "rerank_onnx_max_tokens",
        "rerank_sync",
        "rerank_sync_timeout_ms",
        "event_date_proximity_boost",
        "content_dedup",
        "latest_state_boost",
    ];

    fn assert_levers_off(search: &SearchConfig) {
        assert!(!search.rerank_date_prefix);
        assert_eq!(search.rerank_llm_max_chars, 200);
        assert_eq!(search.rerank_onnx_max_tokens, 0);
        assert!(!search.rerank_sync);
        assert_eq!(search.rerank_sync_timeout_ms, 2000);
        assert_eq!(search.event_date_proximity_boost, 0.0);
        assert_eq!(search.content_dedup, crate::search::ContentDedup::Prefix);
        assert_eq!(search.latest_state_boost, 1.0);
    }

    /// A pre-lever config file (none of the new keys) loads with every lever
    /// off/neutral — the previous hardcoded behavior.
    #[test]
    fn missing_lever_keys_deserialize_to_off_defaults() {
        let mut search = serde_json::to_value(&config().search).unwrap();
        let object = search.as_object_mut().unwrap();
        for key in LEVER_KEYS {
            assert!(object.remove(key).is_some(), "{key} missing from fixture");
        }
        let decoded: SearchConfig = serde_json::from_value(search).unwrap();
        assert_levers_off(&decoded);
    }

    /// Default-off guard: the shipped config.toml spells every lever out and
    /// keeps it off/neutral, so a stock deployment stays byte-identical.
    #[test]
    fn shipped_config_keeps_every_lever_off() {
        let config = config();
        assert_levers_off(&config.search);
        config.validate().unwrap();
    }

    /// `content_dedup` is a closed enum: an unknown value fails the load,
    /// `"exact"` loads.
    #[test]
    fn content_dedup_rejects_unknown_value() {
        let shipped = include_str!("../../../config.toml");
        let line = "content_dedup = \"prefix\"";
        assert_eq!(shipped.matches(line).count(), 1);
        let bogus = shipped.replace(line, "content_dedup = \"bogus\"");
        assert!(toml::from_str::<Config>(&bogus).is_err());
        let exact: Config =
            toml::from_str(&shipped.replace(line, "content_dedup = \"exact\"")).unwrap();
        assert_eq!(
            exact.search.content_dedup,
            crate::search::ContentDedup::Exact
        );
        exact.validate().unwrap();
    }

    #[test]
    fn lever_validation_rejects_zero_caps_and_bad_weights() {
        let mut zero_chars = config();
        zero_chars.search.rerank_llm_max_chars = 0;
        assert!(zero_chars.validate().is_err());

        for tiny in [1, 2, 3, MIN_RERANK_ONNX_MAX_TOKENS - 1] {
            let mut onnx = config();
            onnx.search.rerank_onnx_max_tokens = tiny;
            assert!(onnx.validate().is_err(), "{tiny}");
        }
        for ok in [0, MIN_RERANK_ONNX_MAX_TOKENS, 512] {
            let mut onnx = config();
            onnx.search.rerank_onnx_max_tokens = ok;
            onnx.validate().unwrap();
        }

        let mut zero_timeout = config();
        zero_timeout.search.rerank_sync_timeout_ms = 0;
        assert!(zero_timeout.validate().is_err());

        let mut boost = config();
        boost.search.event_date_proximity_boost = -0.1;
        assert!(boost.validate().is_err());
        boost.search.event_date_proximity_boost = f64::NAN;
        assert!(boost.validate().is_err());
        boost.search.event_date_proximity_boost = 1.5;
        boost.validate().unwrap();

        let mut latest = config();
        for bad in [0.9, 0.0, -1.0, f64::NAN, f64::INFINITY] {
            latest.search.latest_state_boost = bad;
            assert!(latest.validate().is_err(), "{bad}");
        }
        for ok in [1.0, 1.5, 2.0] {
            latest.search.latest_state_boost = ok;
            latest.validate().unwrap();
        }
    }
}
