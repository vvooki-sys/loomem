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
            let response = client
                .post(endpoint()?)
                .header("Authorization", format!("Bearer {}", api_key))
                .header("Content-Type", "application/json")
                .json(&body)
                .timeout(Duration::from_secs(30))
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
    }

    #[cfg(feature = "evaluation")]
    #[test]
    fn missing_override_uses_provider() {
        assert_eq!(
            evaluation_endpoint(Err(std::env::VarError::NotPresent)).unwrap(),
            PROVIDER_URL
        );
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
