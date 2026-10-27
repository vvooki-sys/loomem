//! Evaluation-only loopback routing for deterministic provider fixtures.

#[cfg(feature = "evaluation")]
use anyhow::Context;
use anyhow::Result;

/// Chat endpoint for one call site: `provider_url` (the active provider's
/// `<base_url>/chat/completions`), or in evaluation builds the loopback
/// fixture named by `variable`. Production routing never reads `variable`.
pub fn chat_completions(variable: &str, provider_url: &str) -> Result<String> {
    #[cfg(feature = "evaluation")]
    return evaluation(variable, provider_url, std::env::var(variable));
    #[cfg(not(feature = "evaluation"))]
    {
        let _ = variable;
        Ok(provider_url.to_string())
    }
}

#[cfg(feature = "evaluation")]
fn evaluation(
    variable: &str,
    provider_url: &str,
    value: Result<String, std::env::VarError>,
) -> Result<String> {
    let value = match value {
        Ok(value) => value,
        Err(std::env::VarError::NotPresent) => return Ok(provider_url.to_string()),
        Err(std::env::VarError::NotUnicode(_)) => {
            anyhow::bail!("Invalid evaluation endpoint encoding for {variable}")
        }
    };
    let url = reqwest::Url::parse(&value).context("Invalid evaluation endpoint")?;
    anyhow::ensure!(
        is_loopback(&url),
        "Evaluation endpoint must use literal loopback"
    );
    Ok(url.to_string())
}

#[cfg(feature = "evaluation")]
fn is_loopback(url: &reqwest::Url) -> bool {
    url.scheme() == "http"
        && url.host_str() == Some("127.0.0.1")
        && url.port().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && url.path() == "/v1/chat/completions"
        && url.query().is_none()
        && url.fragment().is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROVIDER_URL: &str = "https://api.openai.com/v1/chat/completions";

    #[cfg(not(feature = "evaluation"))]
    #[test]
    fn production_url_is_fixed() {
        assert_eq!(
            chat_completions("LOOMEM_EVALUATION_PROFILE_URL", PROVIDER_URL).unwrap(),
            PROVIDER_URL
        );
        let ollama = "http://localhost:11434/v1/chat/completions";
        assert_eq!(
            chat_completions("LOOMEM_EVALUATION_PROFILE_URL", ollama).unwrap(),
            ollama
        );
    }

    #[cfg(feature = "evaluation")]
    #[test]
    fn evaluation_url_requires_exact_loopback_endpoint() {
        let local = "http://127.0.0.1:49152/v1/chat/completions";
        assert_eq!(
            evaluation("TEST", PROVIDER_URL, Ok(local.to_string())).unwrap(),
            local
        );
        for invalid in [
            "https://api.openai.com/v1/chat/completions",
            "http://localhost:49152/v1/chat/completions",
            "http://127.0.0.1:49152/other",
        ] {
            assert!(evaluation("TEST", PROVIDER_URL, Ok(invalid.to_string())).is_err());
        }
        assert_eq!(
            evaluation("TEST", PROVIDER_URL, Err(std::env::VarError::NotPresent)).unwrap(),
            PROVIDER_URL
        );
    }
}
