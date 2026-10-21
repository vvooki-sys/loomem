//! HTTP transport; local metering is available only in evaluation builds.

use super::{ChatHttpReply, ExtractionChat};
use anyhow::{Context, Result};
use reqwest::Client;
use std::time::Duration;

const PROVIDER_URL: &str = "https://api.openai.com/v1/chat/completions";

/// Production [`ExtractionChat`]: posts to the OpenAI chat endpoint.
pub struct HttpExtractionChat<'a> {
    client: &'a Client,
    api_key: String,
}

impl<'a> HttpExtractionChat<'a> {
    pub fn new(client: &'a Client, api_key: String) -> Self {
        Self { client, api_key }
    }
}

impl ExtractionChat for HttpExtractionChat<'_> {
    fn chat(
        &self,
        request_body: &serde_json::Value,
    ) -> impl std::future::Future<Output = Result<ChatHttpReply>> + Send {
        // Clone captures so the future owns them (Send, no borrow across await).
        let client = self.client.clone();
        let api_key = self.api_key.clone();
        let body = request_body.clone();
        async move {
            let endpoint = endpoint()?;
            let timeout = request_timeout(&endpoint)?;
            let response = client
                .post(endpoint)
                .header("Authorization", format!("Bearer {}", api_key))
                .header("Content-Type", "application/json")
                .json(&body)
                .timeout(timeout)
                .send()
                .await
                .context("Failed to send extraction request")?;
            let status = response.status().as_u16();
            let body = response
                .text()
                .await
                .context("Failed to read extraction response body")?;
            Ok(ChatHttpReply { status, body })
        }
    }
}

fn request_timeout(_endpoint: &str) -> Result<Duration> {
    #[cfg(feature = "evaluation")]
    return evaluation_timeout(
        _endpoint,
        std::env::var("LOOMEM_EVALUATION_EXTRACTION_TIMEOUT_SECS"),
    );
    #[cfg(not(feature = "evaluation"))]
    Ok(Duration::from_secs(30))
}

#[cfg(feature = "evaluation")]
fn evaluation_timeout(
    endpoint: &str,
    value: Result<String, std::env::VarError>,
) -> Result<Duration> {
    let value = match value {
        Ok(value) => value,
        Err(std::env::VarError::NotPresent) => return Ok(Duration::from_secs(30)),
        Err(std::env::VarError::NotUnicode(_)) => {
            anyhow::bail!("Invalid evaluation extraction timeout encoding")
        }
    };
    // A timeout override must never affect a direct provider request.
    evaluation_endpoint(Ok(endpoint.to_string()))?;
    anyhow::ensure!(
        !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()),
        "Evaluation extraction timeout must be an integer in 1..=600 seconds"
    );
    let seconds = value
        .parse::<u64>()
        .context("Invalid evaluation extraction timeout")?;
    anyhow::ensure!(
        (1..=600).contains(&seconds),
        "Evaluation extraction timeout must be in 1..=600 seconds"
    );
    Ok(Duration::from_secs(seconds))
}

fn endpoint() -> Result<String> {
    #[cfg(feature = "evaluation")]
    return evaluation_endpoint(std::env::var("LOOMEM_EVALUATION_EXTRACTION_URL"));
    #[cfg(not(feature = "evaluation"))]
    Ok(PROVIDER_URL.to_string())
}

#[cfg(feature = "evaluation")]
fn evaluation_endpoint(value: Result<String, std::env::VarError>) -> Result<String> {
    let value = match value {
        Ok(value) => value,
        Err(std::env::VarError::NotPresent) => return Ok(PROVIDER_URL.to_string()),
        Err(std::env::VarError::NotUnicode(_)) => {
            anyhow::bail!("Invalid evaluation extraction endpoint encoding")
        }
    };
    anyhow::ensure!(
        value.starts_with("http://127.0.0.1:"),
        "Evaluation extraction endpoint must use literal loopback"
    );
    let url = reqwest::Url::parse(&value).context("Invalid evaluation extraction endpoint")?;
    anyhow::ensure!(
        is_local_metering_url(&url),
        "Evaluation extraction endpoint must be local loopback"
    );
    Ok(url.to_string())
}

#[cfg(feature = "evaluation")]
fn is_local_metering_url(url: &reqwest::Url) -> bool {
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

    #[cfg(not(feature = "evaluation"))]
    #[test]
    fn production_endpoint_is_unchanged() {
        assert_eq!(endpoint().unwrap(), PROVIDER_URL);
        assert_eq!(
            request_timeout(PROVIDER_URL).unwrap(),
            Duration::from_secs(30)
        );
    }

    #[cfg(feature = "evaluation")]
    #[test]
    fn missing_override_uses_provider() {
        assert_eq!(
            evaluation_endpoint(Err(std::env::VarError::NotPresent)).unwrap(),
            PROVIDER_URL
        );
        assert_eq!(
            evaluation_timeout(PROVIDER_URL, Err(std::env::VarError::NotPresent)).unwrap(),
            Duration::from_secs(30)
        );
    }

    #[cfg(feature = "evaluation")]
    #[test]
    fn timeout_override_is_bounded_and_only_for_local_metering() {
        let local = "http://127.0.0.1:49152/v1/chat/completions";
        for seconds in [1, 30, 180, 600] {
            assert_eq!(
                evaluation_timeout(local, Ok(seconds.to_string())).unwrap(),
                Duration::from_secs(seconds)
            );
        }
        assert!(evaluation_timeout(PROVIDER_URL, Ok("180".to_string())).is_err());
        assert!(evaluation_timeout(
            "http://localhost:49152/v1/chat/completions",
            Ok("180".to_string())
        )
        .is_err());
    }

    #[cfg(feature = "evaluation")]
    #[test]
    fn invalid_timeout_fails_closed() {
        let local = "http://127.0.0.1:49152/v1/chat/completions";
        for value in [
            "",
            "0",
            "601",
            "-1",
            "+1",
            "1.5",
            " 180",
            "180 ",
            "forever",
            "18446744073709551616",
        ] {
            assert!(evaluation_timeout(local, Ok(value.to_string())).is_err());
        }
        assert!(evaluation_timeout(
            local,
            Err(std::env::VarError::NotUnicode(std::ffi::OsString::from(
                "invalid"
            )))
        )
        .is_err());
    }

    #[cfg(feature = "evaluation")]
    #[test]
    fn literal_loopback_metering_is_accepted() {
        let local = "http://127.0.0.1:49152/v1/chat/completions";
        assert_eq!(evaluation_endpoint(Ok(local.to_string())).unwrap(), local);
    }

    #[cfg(feature = "evaluation")]
    #[test]
    fn invalid_override_fails_closed() {
        for value in [
            "https://example.test/v1/chat/completions",
            "http://localhost:49152/v1/chat/completions",
            "http://127.1:49152/v1/chat/completions",
            "http://2130706433:49152/v1/chat/completions",
            "http://127.0.0.1/v1/chat/completions",
            "https://127.0.0.1:49152/v1/chat/completions",
            "http://user:password@127.0.0.1:49152/v1/chat/completions",
            "http://127.0.0.1:49152/v1/chat/completions?forward=1",
            "http://127.0.0.1:49152/v1/chat/completions#fragment",
            "http://127.0.0.1:49152/other",
            "not a URL",
        ] {
            assert!(evaluation_endpoint(Ok(value.to_string())).is_err());
        }
        assert!(evaluation_endpoint(Err(std::env::VarError::NotUnicode(
            std::ffi::OsString::from("invalid")
        )))
        .is_err());
    }
}
