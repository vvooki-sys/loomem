//! Named completion providers (`[llm.providers.<name>]`).
//!
//! Every provider speaks the OpenAI chat-completions wire format; it differs
//! only in `base_url` and in whether it needs a key. Local or remote does not
//! matter: Ollama, vLLM or a company gateway may run without a key, a hosted
//! API needs one. `llm.provider` picks the active entry. A config without a
//! `[llm.providers]` table keeps the built-in `openai` provider, which reads
//! the legacy top-level `llm.api_key` / `llm.api_key_env`.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;

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
}

/// The active provider after config and env lookup.
#[derive(Clone, PartialEq, Eq)]
pub struct ResolvedProvider {
    pub name: String,
    /// Normalized: no trailing `/`.
    pub base_url: String,
    /// `None` only for a provider configured without `api_key_env`.
    pub api_key: Option<String>,
}

impl ResolvedProvider {
    /// Full URL of the chat-completions endpoint.
    pub fn chat_completions_url(&self) -> String {
        format!("{}/chat/completions", self.base_url)
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
/// `llm.api_key_env`, used only by the built-in `openai` provider.
pub fn resolve(
    active: &str,
    providers: &BTreeMap<String, LlmProviderConfig>,
    legacy_key: Option<&str>,
    legacy_key_env: &str,
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<ResolvedProvider, ProviderError> {
    let non_empty = |env: &str| lookup(env).filter(|k| !k.is_empty());
    let missing = |env: &str| ProviderError::MissingKey {
        provider: active.to_string(),
        env: env.to_string(),
    };

    if let Some(p) = providers.get(active) {
        let api_key = match p.api_key_env.as_deref() {
            None => None,
            Some(env) => Some(non_empty(env).ok_or_else(|| missing(env))?),
        };
        return Ok(ResolvedProvider {
            name: active.to_string(),
            base_url: p.base_url.trim_end_matches('/').to_string(),
            api_key,
        });
    }

    if active != BUILTIN_OPENAI {
        return Err(ProviderError::Unknown(active.to_string()));
    }
    let api_key = legacy_key
        .filter(|k| !k.is_empty())
        .map(str::to_string)
        .or_else(|| non_empty(legacy_key_env))
        .ok_or_else(|| missing(legacy_key_env))?;
    Ok(ResolvedProvider {
        name: active.to_string(),
        base_url: OPENAI_BASE_URL.to_string(),
        api_key: Some(api_key),
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
    let (base_url, auth) = match providers.get(active) {
        Some(p) => (
            p.base_url.trim_end_matches('/').to_string(),
            p.api_key_env
                .as_deref()
                .map_or_else(|| "none".to_string(), presence),
        ),
        None if legacy_key.is_some_and(|k| !k.is_empty()) => {
            (OPENAI_BASE_URL.to_string(), "config:api_key".to_string())
        }
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
}
