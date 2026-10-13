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
    validate_limits(search)
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
}
