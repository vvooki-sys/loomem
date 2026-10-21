//! Injectable scoring time, isolated from operation reference dates.
use chrono::Utc;
use tracing::debug;

use super::HybridSearchEngine;
use crate::decay::DecayConfig;

#[derive(Default)]
pub(super) enum SearchClock {
    #[default]
    System,
    #[cfg(feature = "evaluation")]
    Fixed(chrono::DateTime<Utc>),
}

impl SearchClock {
    fn timestamp(&self) -> i64 {
        match self {
            Self::System => Utc::now().timestamp(),
            #[cfg(feature = "evaluation")]
            Self::Fixed(time) => time.timestamp(),
        }
    }
}

impl HybridSearchEngine {
    /// Freeze scoring time for reproducible evaluation; decay remains enabled.
    #[cfg(feature = "evaluation")]
    pub fn with_fixed_time(config: crate::config::Config, time: chrono::DateTime<Utc>) -> Self {
        Self {
            config,
            clock: SearchClock::Fixed(time),
        }
    }

    /// Compute time decay factor based on age and level-specific lambda
    pub(super) fn compute_time_decay(
        &self,
        timestamp: i64,
        level: i32,
        decay_config: &DecayConfig,
    ) -> f64 {
        let now = self.clock.timestamp();
        let age_seconds = (now - timestamp) as f64;

        // Select lambda based on level
        let lambda = match level {
            0 => decay_config.l0_lambda,
            _ => decay_config.l1_lambda,
        };

        // Exponential decay: e^(-lambda * age)
        // age is in seconds, so we normalize by dividing by 86400 (seconds per day)
        let age_days = age_seconds / 86400.0;
        let decay = (-lambda * age_days).exp();

        debug!(
            "Time decay: level={}, age_days={:.2}, lambda={}, decay={:.4}",
            level, age_days, lambda, decay
        );

        decay
    }
}

#[cfg(all(test, feature = "evaluation"))]
mod tests {
    use super::*;

    #[test]
    fn fixed_clock_keeps_decay_enabled_and_identical_across_engines() {
        let time = chrono::DateTime::from_timestamp(1_792_430_400, 0).unwrap();
        let config = crate::hybrid_search::tests::test_config();
        let first = HybridSearchEngine::with_fixed_time(config.clone(), time);
        let second = HybridSearchEngine::with_fixed_time(config.clone(), time);
        let yesterday = time.timestamp() - 86_400;
        let expected = (-config.search.decay.l0_lambda).exp();
        for _ in 0..3 {
            let decay = first.compute_time_decay(yesterday, 0, &config.search.decay);
            assert_eq!(decay, expected);
            assert_eq!(
                decay,
                second.compute_time_decay(yesterday, 0, &config.search.decay)
            );
            assert!(decay < 1.0);
        }
        assert_eq!(
            first.compute_time_decay(time.timestamp(), 0, &config.search.decay),
            1.0
        );
    }
}
