//! Named completion providers (`[llm.providers.<name>]`).
//!
//! Every provider speaks the OpenAI chat-completions wire format; it differs
//! only in `base_url` and in whether it needs a key. Local or remote does not
//! matter: Ollama, vLLM or a company gateway may run without a key, a hosted
//! API needs one. `llm.provider` picks the active entry. A config without a
//! `[llm.providers]` table keeps the built-in `openai` provider, which reads
//! the legacy top-level `llm.api_key` / `llm.api_key_env`. An entry named
//! `openai` refines the built-in provider: it sets the base URL and the
//! request options; its own `api_key_env` wins, and only the inline
//! `llm.api_key` is a fallback (never the legacy env var).

mod capabilities;
mod reply;

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;

pub use capabilities::{completion_limit_of, ChatCapabilities, CompletionLimit, REASONING_EFFORTS};
pub use reply::{ensure_choice_not_truncated, ensure_not_truncated, TruncatedCompletion};

/// Name of the built-in provider used when `llm.providers` has no such entry.
pub const BUILTIN_OPENAI: &str = "openai";

/// Base URL of the built-in `openai` provider.
pub const OPENAI_BASE_URL: &str = "https://api.openai.com/v1";

/// One `[llm.providers.<name>]` entry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LlmProviderConfig {
    /// API root without the endpoint path, e.g. `http://localhost:11434/v1`.
    pub base_url: String,
    /// Env var that holds the key. Unset → the provider takes no key and calls
    /// carry no `Authorization` header. Set → the env var must be non-empty.
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// Request options for this provider's models (`omit_temperature`,
    /// `max_completion_tokens`, `completion_tokens_floor`,
    /// `reasoning_effort`); all off by default.
    #[serde(flatten)]
    pub chat: ChatCapabilities,
}

/// The active provider after config and env lookup.
#[derive(Clone, PartialEq, Eq)]
pub struct ResolvedProvider {
    pub name: String,
    /// Normalized: no trailing `/`.
    pub base_url: String,
    /// `None` only for a provider configured without `api_key_env`.
    pub api_key: Option<String>,
    /// Request options of the entry; default (identity) for the built-in
    /// `openai` provider.
    pub chat: ChatCapabilities,
}

impl ResolvedProvider {
    /// Full URL of the chat-completions endpoint.
    pub fn chat_completions_url(&self) -> String {
        format!("{}/chat/completions", self.base_url)
    }

    /// Apply this provider's request options to a `serde_json` chat body
    /// (see [`ChatCapabilities::apply`]). Untouched for a provider without
    /// options, so call sites can wrap every body unconditionally.
    pub fn chat_body(&self, body: serde_json::Value) -> serde_json::Value {
        self.chat.apply(body)
    }

    /// POST to the chat-completions endpoint. `Authorization` is set only when
    /// the provider has a key; a keyless provider gets no header at all.
    pub fn chat_request(&self, client: &reqwest::Client) -> reqwest::RequestBuilder {
        self.authorize(client.post(self.chat_completions_url()))
    }

    /// Add the provider's bearer key, if it has one. For a request to a URL
    /// from `evaluation_url::chat_completions`, which in evaluation builds may
    /// be a loopback fixture instead of `base_url`.
    pub fn authorize(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.api_key {
            Some(key) => request.bearer_auth(key),
            None => request,
        }
    }

    /// Send a request built by [`Self::chat_request`] (or [`Self::authorize`])
    /// and log the call: DEBUG
    /// for every call, WARN on a transport error or a non-2xx status. The
    /// response is returned untouched, so callers keep their own retry and
    /// parsing. `call_site` is a short static tag such as `"llm.compress"`.
    pub async fn send_chat(
        &self,
        request: reqwest::RequestBuilder,
        call_site: &'static str,
        model: &str,
    ) -> reqwest::Result<reqwest::Response> {
        let started = std::time::Instant::now();
        let result = request.send().await;
        let latency_ms = started.elapsed().as_millis();
        match &result {
            Ok(response) if response.status().is_success() => tracing::debug!(
                "LLM chat: provider={} model={model} site={call_site} status={} latency_ms={latency_ms}",
                self.name,
                response.status().as_u16()
            ),
            Ok(response) => tracing::warn!(
                "LLM chat failed: provider={} model={model} site={call_site} status={} latency_ms={latency_ms}",
                self.name,
                response.status().as_u16()
            ),
            Err(e) => tracing::warn!(
                "LLM chat failed: provider={} model={model} site={call_site} error={e} latency_ms={latency_ms}",
                self.name
            ),
        }
        result
    }
}

// Manual impl so a key never reaches logs through `{:?}`.
impl fmt::Debug for ResolvedProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResolvedProvider")
            .field("name", &self.name)
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProviderError {
    #[error(
        "llm.provider {0:?} is not defined in [llm.providers] and is not the built-in \"openai\""
    )]
    Unknown(String),
    #[error("llm provider {provider:?} needs a key, but env var {env} is not set or is empty")]
    MissingKey { provider: String, env: String },
}

/// Resolve `active` against `providers`. `lookup` reads an env var; it is a
/// parameter so tests stay free of process-wide env mutation.
///
/// `legacy_key` / `legacy_key_env` are the top-level `llm.api_key` /
/// `llm.api_key_env`, used by the built-in `openai` provider. An entry named
/// `openai` falls back to the inline `legacy_key` only (see
/// [`entry_api_key`]); `legacy_key_env` is never read for a named entry.
pub fn resolve(
    active: &str,
    providers: &BTreeMap<String, LlmProviderConfig>,
    legacy_key: Option<&str>,
    legacy_key_env: &str,
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<ResolvedProvider, ProviderError> {
    if let Some(p) = providers.get(active) {
        return Ok(ResolvedProvider {
            name: active.to_string(),
            base_url: p.base_url.trim_end_matches('/').to_string(),
            api_key: entry_api_key(active, p, legacy_key, &lookup)?,
            chat: p.chat.clone(),
        });
    }

    if active != BUILTIN_OPENAI {
        return Err(ProviderError::Unknown(active.to_string()));
    }
    let api_key = inline_key(legacy_key)
        .or_else(|| lookup(legacy_key_env).filter(|k| !k.is_empty()))
        .ok_or_else(|| ProviderError::MissingKey {
            provider: active.to_string(),
            env: legacy_key_env.to_string(),
        })?;
    Ok(ResolvedProvider {
        name: active.to_string(),
        base_url: OPENAI_BASE_URL.to_string(),
        api_key: Some(api_key),
        chat: ChatCapabilities::default(),
    })
}

/// The inline legacy `llm.api_key`, when non-empty.
fn inline_key(legacy_key: Option<&str>) -> Option<String> {
    legacy_key.filter(|k| !k.is_empty()).map(str::to_string)
}

/// Key of a named entry: `None` for an entry without `api_key_env`,
/// otherwise the entry's env var. The entry named `openai` refines the
/// built-in provider, so it falls back to the inline legacy `llm.api_key`
/// (configs and fixtures that keep the key there still resolve) — but never
/// to the legacy `llm.api_key_env`: an `openai` entry pointed at a proxy with
/// its own, unset env var must fail closed rather than send the real OpenAI
/// key elsewhere.
fn entry_api_key(
    active: &str,
    entry: &LlmProviderConfig,
    legacy_key: Option<&str>,
    lookup: &impl Fn(&str) -> Option<String>,
) -> Result<Option<String>, ProviderError> {
    let Some(env) = entry.api_key_env.as_deref() else {
        return Ok(None);
    };
    let inline = || {
        (active == BUILTIN_OPENAI)
            .then(|| inline_key(legacy_key))
            .flatten()
    };
    lookup(env)
        .filter(|k| !k.is_empty())
        .or_else(inline)
        .map(Some)
        .ok_or_else(|| ProviderError::MissingKey {
            provider: active.to_string(),
            env: env.to_string(),
        })
}

/// Static checks (no env access): the active provider exists, every
/// `base_url` is a usable API root (see [`check_base_url`]), and no
/// `api_key_env` is an empty string.
pub fn validate(
    active: &str,
    providers: &BTreeMap<String, LlmProviderConfig>,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        active == BUILTIN_OPENAI || providers.contains_key(active),
        "{}",
        ProviderError::Unknown(active.to_string())
    );
    for (name, p) in providers {
        if let Err(why) = check_base_url(&p.base_url) {
            anyhow::bail!("llm.providers.{name}.base_url {why}");
        }
        anyhow::ensure!(
            p.api_key_env.as_deref() != Some(""),
            "llm.providers.{name}.api_key_env must not be empty; remove it for a provider without a key"
        );
        p.chat.validate(name)?;
    }
    Ok(())
}

/// Why a `base_url` cannot serve as an API root: it must be an absolute
/// http(s) URL (which implies a host), and carry nothing that `chat_completions_url`
/// would append to incorrectly (query, fragment) or that would leak through
/// the startup log (`user:password@`). The value itself is never part of the
/// message, so a rejected URL with credentials does not reach logs either.
fn check_base_url(raw: &str) -> Result<(), &'static str> {
    if !(raw.starts_with("http://") || raw.starts_with("https://")) {
        return Err("must start with http:// or https://");
    }
    // For http(s) the parser already requires a host (`http://` is an error).
    let url = reqwest::Url::parse(raw).map_err(|_| "is not a valid URL")?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err("must not carry credentials; use api_key_env instead");
    }
    if url.query().is_some() {
        return Err("must not have a query string");
    }
    if url.fragment().is_some() {
        return Err("must not have a fragment");
    }
    Ok(())
}

/// One-line startup description of the active provider, for DEV/STG logs.
/// Shows where the key comes from and whether it is present, never the key.
/// An unknown provider cannot reach this point: `validate` rejects it first.
pub fn describe(
    active: &str,
    providers: &BTreeMap<String, LlmProviderConfig>,
    legacy_key: Option<&str>,
    legacy_key_env: &str,
    lookup: impl Fn(&str) -> Option<String>,
) -> String {
    let presence = |env: &str| {
        let state = if lookup(env).is_some_and(|k| !k.is_empty()) {
            "set"
        } else {
            "missing"
        };
        format!("env:{env} ({state})")
    };
    let inline = inline_key(legacy_key).is_some();
    let (base_url, auth) = match providers.get(active) {
        Some(p) => (
            p.base_url.trim_end_matches('/').to_string(),
            match p.api_key_env.as_deref() {
                None => "none".to_string(),
                // The `openai` entry falls back to the inline key only, as in
                // `entry_api_key`; its own env var is reported otherwise.
                Some(env)
                    if active == BUILTIN_OPENAI
                        && inline
                        && !lookup(env).is_some_and(|k| !k.is_empty()) =>
                {
                    "config:api_key".to_string()
                }
                Some(env) => presence(env),
            },
        ),
        None if inline => (OPENAI_BASE_URL.to_string(), "config:api_key".to_string()),
        None => (OPENAI_BASE_URL.to_string(), presence(legacy_key_env)),
    };
    format!("provider={active}, base_url={base_url}, auth={auth}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn providers(entries: &[(&str, &str, Option<&str>)]) -> BTreeMap<String, LlmProviderConfig> {
        entries
            .iter()
            .map(|(name, url, env)| {
                (
                    (*name).to_string(),
                    LlmProviderConfig {
                        base_url: (*url).to_string(),
                        api_key_env: env.map(str::to_string),
                        chat: ChatCapabilities::default(),
                    },
                )
            })
            .collect()
    }

    fn env(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |name| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| (*v).to_string())
        }
    }

    #[test]
    fn provider_without_key_env_resolves_without_key() {
        let p = providers(&[("ollama", "http://localhost:11434/v1/", None)]);
        let r = resolve("ollama", &p, None, "OPENAI_API_KEY", env(&[])).unwrap();
        assert_eq!(r.base_url, "http://localhost:11434/v1");
        assert_eq!(
            r.chat_completions_url(),
            "http://localhost:11434/v1/chat/completions"
        );
        assert_eq!(r.api_key, None);
    }

    #[test]
    fn provider_with_key_env_reads_the_key() {
        let p = providers(&[("router", "https://router.example/v1", Some("ROUTER_KEY"))]);
        let r = resolve(
            "router",
            &p,
            None,
            "OPENAI_API_KEY",
            env(&[("ROUTER_KEY", "k1")]),
        )
        .unwrap();
        assert_eq!(r.api_key.as_deref(), Some("k1"));
    }

    #[test]
    fn provider_with_key_env_fails_when_key_is_missing_or_empty() {
        let p = providers(&[("router", "https://router.example/v1", Some("ROUTER_KEY"))]);
        let expected = ProviderError::MissingKey {
            provider: "router".into(),
            env: "ROUTER_KEY".into(),
        };
        assert_eq!(
            resolve("router", &p, None, "X", env(&[])).unwrap_err(),
            expected
        );
        assert_eq!(
            resolve("router", &p, None, "X", env(&[("ROUTER_KEY", "")])).unwrap_err(),
            expected
        );
    }

    #[test]
    fn builtin_openai_uses_legacy_key_fields() {
        let none = BTreeMap::new();
        let from_env = resolve(
            "openai",
            &none,
            None,
            "OPENAI_API_KEY",
            env(&[("OPENAI_API_KEY", "e")]),
        )
        .unwrap();
        assert_eq!(from_env.base_url, OPENAI_BASE_URL);
        assert_eq!(from_env.api_key.as_deref(), Some("e"));
        // Inline key wins over the env var, matching LlmConfig::get_api_key.
        let inline = resolve(
            "openai",
            &none,
            Some("i"),
            "OPENAI_API_KEY",
            env(&[("OPENAI_API_KEY", "e")]),
        )
        .unwrap();
        assert_eq!(inline.api_key.as_deref(), Some("i"));
        assert!(matches!(
            resolve("openai", &none, Some(""), "OPENAI_API_KEY", env(&[])),
            Err(ProviderError::MissingKey { .. })
        ));
    }

    #[test]
    fn configured_openai_entry_overrides_builtin() {
        let p = providers(&[("openai", "https://proxy.example/v1", Some("PROXY_KEY"))]);
        let r = resolve(
            "openai",
            &p,
            Some("legacy"),
            "OPENAI_API_KEY",
            env(&[("PROXY_KEY", "p")]),
        )
        .unwrap();
        assert_eq!(r.base_url, "https://proxy.example/v1");
        assert_eq!(r.api_key.as_deref(), Some("p"));
    }

    /// An entry named `openai` refines the built-in provider: its own env var
    /// wins, and the inline legacy `llm.api_key` is the only fallback, so a
    /// config (or test fixture) that keeps its key inline still resolves.
    /// The legacy `llm.api_key_env` is never read for the entry.
    #[test]
    fn openai_entry_falls_back_to_inline_legacy_key_only() {
        let p = providers(&[(
            "openai",
            "https://api.openai.com/v1",
            Some("OPENAI_API_KEY"),
        )]);
        let inline = resolve("openai", &p, Some("sk-inline"), "OPENAI_API_KEY", env(&[])).unwrap();
        assert_eq!(inline.api_key.as_deref(), Some("sk-inline"));
        // A key that lives only in the legacy env var does not count.
        assert_eq!(
            resolve(
                "openai",
                &p,
                None,
                "LEGACY_KEY",
                env(&[("LEGACY_KEY", "sk-legacy")]),
            )
            .unwrap_err(),
            ProviderError::MissingKey {
                provider: "openai".into(),
                env: "OPENAI_API_KEY".into(),
            }
        );
        // The entry's env var still wins over the inline key.
        let own = resolve(
            "openai",
            &p,
            Some("sk-inline"),
            "OPENAI_API_KEY",
            env(&[("OPENAI_API_KEY", "sk-env")]),
        )
        .unwrap();
        assert_eq!(own.api_key.as_deref(), Some("sk-env"));
        assert_eq!(
            resolve("openai", &p, Some(""), "OPENAI_API_KEY", env(&[])).unwrap_err(),
            ProviderError::MissingKey {
                provider: "openai".into(),
                env: "OPENAI_API_KEY".into(),
            }
        );
        // Other named entries do not fall back.
        let other = providers(&[("router", "https://r.example/v1", Some("ROUTER_KEY"))]);
        assert!(matches!(
            resolve(
                "router",
                &other,
                Some("sk-inline"),
                "OPENAI_API_KEY",
                env(&[])
            ),
            Err(ProviderError::MissingKey { .. })
        ));
        // describe() follows the same fallback and never shows the key.
        let shown = describe("openai", &p, Some("sk-inline"), "OPENAI_API_KEY", env(&[]));
        assert_eq!(
            shown,
            "provider=openai, base_url=https://api.openai.com/v1, auth=config:api_key"
        );
        // Without an inline key the entry's own env var is reported, not the
        // legacy one.
        assert_eq!(
            describe(
                "openai",
                &p,
                None,
                "LEGACY_KEY",
                env(&[("LEGACY_KEY", "sk-legacy")])
            ),
            "provider=openai, base_url=https://api.openai.com/v1, auth=env:OPENAI_API_KEY (missing)"
        );
        assert_eq!(
            describe(
                "openai",
                &p,
                Some("sk-inline"),
                "OPENAI_API_KEY",
                env(&[("OPENAI_API_KEY", "sk-env")])
            ),
            "provider=openai, base_url=https://api.openai.com/v1, auth=env:OPENAI_API_KEY (set)"
        );
    }

    /// Fail-closed regression guard: an `openai` entry pointed at a proxy with
    /// its own, unset env var must not pick up the real OpenAI key from the
    /// legacy env var.
    #[test]
    fn openai_entry_with_unset_own_env_never_reads_legacy_env() {
        let p = providers(&[("openai", "https://proxy.example/v1", Some("PROXY_KEY"))]);
        let real_key_only = env(&[("OPENAI_API_KEY", "sk-real")]);
        assert_eq!(
            resolve("openai", &p, None, "OPENAI_API_KEY", &real_key_only).unwrap_err(),
            ProviderError::MissingKey {
                provider: "openai".into(),
                env: "PROXY_KEY".into(),
            }
        );
        assert_eq!(
            describe("openai", &p, None, "OPENAI_API_KEY", &real_key_only),
            "provider=openai, base_url=https://proxy.example/v1, auth=env:PROXY_KEY (missing)"
        );
        // With its own env var set the proxy entry resolves to that key only.
        let both = env(&[("OPENAI_API_KEY", "sk-real"), ("PROXY_KEY", "sk-proxy")]);
        let r = resolve("openai", &p, None, "OPENAI_API_KEY", &both).unwrap();
        assert_eq!(r.api_key.as_deref(), Some("sk-proxy"));
    }

    #[test]
    fn resolve_carries_the_entry_capabilities_and_builtin_has_none() {
        let mut p = providers(&[(
            "openai",
            "https://api.openai.com/v1",
            Some("OPENAI_API_KEY"),
        )]);
        p.get_mut("openai").unwrap().chat = ChatCapabilities {
            max_completion_tokens: true,
            reasoning_effort: Some("low".into()),
            ..Default::default()
        };
        let entry = resolve("openai", &p, Some("k"), "OPENAI_API_KEY", env(&[])).unwrap();
        assert!(entry.chat.max_completion_tokens);
        assert_eq!(entry.chat.reasoning_effort.as_deref(), Some("low"));
        let body = serde_json::json!({"model": "m", "max_tokens": 10, "temperature": 0.0});
        assert_eq!(entry.chat_body(body.clone())["max_completion_tokens"], 10);
        let builtin = resolve(
            "openai",
            &BTreeMap::new(),
            Some("k"),
            "OPENAI_API_KEY",
            env(&[]),
        )
        .unwrap();
        assert!(builtin.chat.is_default());
        assert_eq!(builtin.chat_body(body.clone()), body);
    }

    #[test]
    fn validate_rejects_unknown_reasoning_effort() {
        let mut p = providers(&[("a", "http://x", None)]);
        p.get_mut("a").unwrap().chat.reasoning_effort = Some("extreme".into());
        let err = validate("a", &p).unwrap_err().to_string();
        assert!(err.contains("llm.providers.a.reasoning_effort"), "{err}");
        p.get_mut("a").unwrap().chat.reasoning_effort = Some("low".into());
        validate("a", &p).unwrap();
    }

    #[test]
    fn unknown_provider_is_an_error() {
        let err = resolve("nope", &BTreeMap::new(), None, "X", env(&[])).unwrap_err();
        assert_eq!(err, ProviderError::Unknown("nope".into()));
        assert!(validate("nope", &BTreeMap::new()).is_err());
    }

    #[test]
    fn validate_rejects_bad_url_and_empty_key_env() {
        assert!(validate("openai", &BTreeMap::new()).is_ok());
        assert!(validate("a", &providers(&[("a", "localhost:11434", None)])).is_err());
        assert!(validate("a", &providers(&[("a", "http://x", Some(""))])).is_err());
        assert!(validate("a", &providers(&[("a", "http://x", None)])).is_ok());
    }

    #[test]
    fn validate_rejects_unusable_api_roots() {
        let rejected: &[(&str, &str)] = &[
            ("http://", "is not a valid URL"),
            (
                "https://user:secret@gateway.example/v1",
                "must not carry credentials",
            ),
            (
                "https://token@gateway.example/v1",
                "must not carry credentials",
            ),
            (
                "https://gateway.example/v1?region=eu",
                "must not have a query string",
            ),
            ("https://gateway.example/v1#eu", "must not have a fragment"),
            ("http://exa mple/v1", "is not a valid URL"),
        ];
        for (url, why) in rejected {
            let err = validate("a", &providers(&[("a", url, None)]))
                .expect_err(url)
                .to_string();
            assert!(err.starts_with("llm.providers.a.base_url "), "{err}");
            assert!(err.contains(why), "{url}: {err}");
            assert!(!err.contains("secret"), "URL credentials leaked: {err}");
        }
        for url in [
            "http://localhost:11434/v1/",
            "https://gateway.example:8443/v1",
            "https://api.openai.com/v1",
        ] {
            validate("a", &providers(&[("a", url, None)])).expect(url);
        }
    }

    fn built_chat_request(api_key: Option<&str>) -> reqwest::Request {
        let provider = ResolvedProvider {
            name: "p".into(),
            base_url: "http://localhost:11434/v1".into(),
            api_key: api_key.map(str::to_string),
            chat: ChatCapabilities::default(),
        };
        provider
            .chat_request(&reqwest::Client::new())
            .build()
            .expect("build chat request")
    }

    #[test]
    fn chat_request_posts_to_chat_completions_with_bearer_key() {
        let request = built_chat_request(Some("sk-test"));
        assert_eq!(request.method(), reqwest::Method::POST);
        assert_eq!(
            request.url().as_str(),
            "http://localhost:11434/v1/chat/completions"
        );
        assert_eq!(
            request.headers().get(reqwest::header::AUTHORIZATION),
            Some(&reqwest::header::HeaderValue::from_static("Bearer sk-test"))
        );
    }

    #[test]
    fn chat_request_without_key_sends_no_authorization_header() {
        let request = built_chat_request(None);
        assert!(request
            .headers()
            .get(reqwest::header::AUTHORIZATION)
            .is_none());
    }

    #[test]
    fn describe_shows_key_source_but_never_the_key() {
        let p = providers(&[
            ("ollama", "http://localhost:11434/v1/", None),
            ("router", "https://router.example/v1", Some("ROUTER_KEY")),
        ]);
        let none = BTreeMap::new();
        assert_eq!(
            describe("ollama", &p, None, "OPENAI_API_KEY", env(&[])),
            "provider=ollama, base_url=http://localhost:11434/v1, auth=none"
        );
        assert_eq!(
            describe(
                "router",
                &p,
                None,
                "X",
                env(&[("ROUTER_KEY", "secret-value")])
            ),
            "provider=router, base_url=https://router.example/v1, auth=env:ROUTER_KEY (set)"
        );
        assert_eq!(
            describe("router", &p, None, "X", env(&[])),
            "provider=router, base_url=https://router.example/v1, auth=env:ROUTER_KEY (missing)"
        );
        assert_eq!(
            describe(
                "openai",
                &none,
                None,
                "OPENAI_API_KEY",
                env(&[("OPENAI_API_KEY", "s")])
            ),
            "provider=openai, base_url=https://api.openai.com/v1, auth=env:OPENAI_API_KEY (set)"
        );
        let inline = describe(
            "openai",
            &none,
            Some("secret-value"),
            "OPENAI_API_KEY",
            env(&[]),
        );
        assert_eq!(
            inline,
            "provider=openai, base_url=https://api.openai.com/v1, auth=config:api_key"
        );
        assert!(!inline.contains("secret-value"));
    }

    #[test]
    fn debug_output_redacts_the_key() {
        let r = ResolvedProvider {
            name: "x".into(),
            base_url: "http://x".into(),
            api_key: Some("secret-value".into()),
            chat: ChatCapabilities::default(),
        };
        let shown = format!("{r:?}");
        assert!(!shown.contains("secret-value"));
        assert!(shown.contains("<redacted>"));
    }

    #[test]
    fn providers_table_parses_from_toml() {
        let parsed: BTreeMap<String, LlmProviderConfig> = toml::from_str(
            r#"
            [ollama]
            base_url = "http://localhost:11434/v1"

            [router]
            base_url = "https://router.example/v1"
            api_key_env = "ROUTER_KEY"
            "#,
        )
        .unwrap();
        assert_eq!(parsed["ollama"].api_key_env, None);
        assert_eq!(parsed["router"].api_key_env.as_deref(), Some("ROUTER_KEY"));
    }

    fn provider_at(base_url: &str, api_key: Option<&str>) -> ResolvedProvider {
        ResolvedProvider {
            name: "mock".into(),
            base_url: base_url.into(),
            api_key: api_key.map(str::to_string),
            chat: ChatCapabilities::default(),
        }
    }

    async fn send_to(
        provider: &ResolvedProvider,
        request: reqwest::RequestBuilder,
    ) -> reqwest::Result<reqwest::Response> {
        let body = serde_json::json!({"model": "m", "messages": []});
        tokio::time::timeout(
            crate::test_http::DEADLINE,
            provider.send_chat(request.json(&body), "test", "m"),
        )
        .await
        .expect("send_chat did not finish in time")
    }

    #[tokio::test]
    async fn send_chat_posts_bearer_key_to_chat_completions() {
        let (base_url, server) =
            crate::test_http::serve_once("200 OK", r#"{"ok":true}"#, Some("Bearer sk-test")).await;
        let provider = provider_at(&base_url, Some("sk-test"));
        let request = provider.chat_request(&reqwest::Client::new());
        let response = send_to(&provider, request).await.expect("send");
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(response.text().await.unwrap(), r#"{"ok":true}"#);
        crate::test_http::finish(server).await;
    }

    #[tokio::test]
    async fn send_chat_keyless_provider_sends_no_authorization_header() {
        let (base_url, server) = crate::test_http::serve_once("200 OK", "{}", None).await;
        let provider = provider_at(&base_url, None);
        let request = provider.chat_request(&reqwest::Client::new());
        let response = send_to(&provider, request).await.expect("send");
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        crate::test_http::finish(server).await;
    }

    #[tokio::test]
    async fn send_chat_returns_non_2xx_response_as_ok() {
        let (base_url, server) =
            crate::test_http::serve_once("429 Too Many Requests", "slow down", None).await;
        let provider = provider_at(&base_url, None);
        let request = provider.chat_request(&reqwest::Client::new());
        let response = send_to(&provider, request).await.expect("send");
        assert_eq!(response.status(), reqwest::StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.text().await.unwrap(), "slow down");
        crate::test_http::finish(server).await;
    }

    #[tokio::test]
    async fn authorize_adds_key_to_a_request_for_another_url() {
        let (mock_url, server) =
            crate::test_http::serve_once("200 OK", "{}", Some("Bearer sk-test")).await;
        // As with an evaluation loopback override: the URL is not base_url.
        let provider = provider_at("http://127.0.0.1:9/v1", Some("sk-test"));
        let request =
            provider.authorize(reqwest::Client::new().post(format!("{mock_url}/chat/completions")));
        let response = send_to(&provider, request).await.expect("send");
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        crate::test_http::finish(server).await;
    }

    #[tokio::test]
    async fn send_chat_returns_err_when_nothing_listens() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
        drop(listener);
        let provider = provider_at(&base_url, None);
        let request = provider.chat_request(&reqwest::Client::new());
        assert!(send_to(&provider, request).await.is_err());
    }
}
