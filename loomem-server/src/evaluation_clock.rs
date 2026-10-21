//! Evaluation-only wall-clock override; production binaries never read its env.
use chrono::{DateTime, Utc};
use loomem_core::{Config, HybridSearchEngine};

#[cfg(feature = "evaluation")]
static FIXED_TIME: std::sync::OnceLock<DateTime<Utc>> = std::sync::OnceLock::new();

pub(crate) fn now() -> DateTime<Utc> {
    #[cfg(feature = "evaluation")]
    if let Some(time) = FIXED_TIME.get() {
        return *time;
    }
    Utc::now()
}

pub(crate) fn engine(config: Config) -> anyhow::Result<HybridSearchEngine> {
    #[cfg(feature = "evaluation")]
    if let Some(time) = configured_time(std::env::var("LOOMEM_EVALUATION_TIME_UNIX"))? {
        FIXED_TIME
            .set(time)
            .map_err(|_| anyhow::anyhow!("evaluation clock already initialized"))?;
        tracing::info!(
            evaluation_time_unix = time.timestamp(),
            "Fixed evaluation clock enabled"
        );
        return Ok(HybridSearchEngine::with_fixed_time(config, time));
    }
    Ok(HybridSearchEngine::new(config))
}

#[cfg(feature = "evaluation")]
fn configured_time(
    value: Result<String, std::env::VarError>,
) -> anyhow::Result<Option<DateTime<Utc>>> {
    use anyhow::Context;
    match value {
        Ok(value) => {
            let seconds = value
                .parse::<i64>()
                .context("invalid LOOMEM_EVALUATION_TIME_UNIX integer")?;
            Ok(Some(DateTime::from_timestamp(seconds, 0).context(
                "LOOMEM_EVALUATION_TIME_UNIX outside supported date range",
            )?))
        }
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(error) => Err(error).context("invalid LOOMEM_EVALUATION_TIME_UNIX environment value"),
    }
}

#[cfg(all(test, feature = "evaluation"))]
mod tests {
    use super::*;

    #[test]
    fn accepts_valid_fixed_time_and_absent_setting() {
        assert_eq!(
            configured_time(Ok("1792432800".into()))
                .unwrap()
                .unwrap()
                .timestamp(),
            1_792_432_800
        );
        assert_eq!(
            configured_time(Err(std::env::VarError::NotPresent)).unwrap(),
            None
        );
    }

    #[test]
    fn rejects_invalid_or_out_of_range_time() {
        for value in [
            "",
            "tomorrow",
            "1.5",
            "9223372036854775808",
            "9223372036854775807",
        ] {
            assert!(configured_time(Ok(value.into())).is_err(), "{value}");
        }
    }
}
