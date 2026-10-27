//! `[llm.providers.<name>]` end to end through `Config::load` on a real file:
//! an old OpenAI-only config keeps working unchanged, a keyless provider loads
//! and resolves, a keyed provider without its env var fails at resolution, and
//! an undefined provider or a bad `base_url` (no scheme, or credentials that
//! would otherwise reach the startup log) refuses to load.
//!
//! No test here mutates the process env (parallel tests would race); env var
//! names that must be absent are unique to this file.

use anyhow::Result;
use loomem_core::config::Config;
use loomem_core::llm_provider::{ProviderError, OPENAI_BASE_URL};

const SHIPPED: &str = include_str!("../../config.toml");
const MISSING_ENV: &str = "LOOMEM_TEST_PROVIDER_INTEGRATION_NO_SUCH_KEY_7d1f";

/// Shipped config with `provider` and `api_key_env` replaced and `extra`
/// appended (after the last table, so `[llm.providers.*]` stays valid TOML).
fn config_with(provider: &str, api_key_line: &str, extra: &str) -> String {
    let base = SHIPPED
        .replacen(
            "provider = \"openai\"",
            &format!("provider = \"{provider}\""),
            1,
        )
        .replacen("api_key_env = \"OPENAI_API_KEY\"", api_key_line, 1);
    format!("{base}\n{extra}\n")
}

fn load(content: &str) -> Result<Config> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("config.toml");
    std::fs::write(&path, content)?;
    Config::load(&path)
}

#[test]
fn old_openai_config_loads_and_resolves_builtin_provider() -> Result<()> {
    let cfg = load(&config_with(
        "openai",
        "api_key_env = \"OPENAI_API_KEY\"\napi_key = \"sk-test-inline\"",
        "",
    ))?;
    assert!(cfg.llm.providers.is_empty());
    let active = cfg.llm.active_provider()?;
    assert_eq!(active.name, "openai");
    assert_eq!(active.base_url, OPENAI_BASE_URL);
    assert_eq!(active.api_key.as_deref(), Some("sk-test-inline"));
    // The legacy accessor used by today's call sites agrees.
    assert_eq!(cfg.llm.get_api_key().as_deref(), Some("sk-test-inline"));
    assert!(cfg.llm.provider_summary().contains("auth=config:api_key"));
    assert!(!cfg.llm.provider_summary().contains("sk-test-inline"));
    Ok(())
}

#[test]
fn keyless_provider_loads_and_resolves_without_key() -> Result<()> {
    let cfg = load(&config_with(
        "ollama",
        &format!("api_key_env = \"{MISSING_ENV}\""),
        "[llm.providers.ollama]\nbase_url = \"http://localhost:11434/v1/\"",
    ))?;
    let active = cfg.llm.active_provider()?;
    assert_eq!(active.base_url, "http://localhost:11434/v1");
    assert_eq!(
        active.chat_completions_url(),
        "http://localhost:11434/v1/chat/completions"
    );
    assert_eq!(active.api_key, None);
    assert_eq!(
        cfg.llm.provider_summary(),
        "provider=ollama, base_url=http://localhost:11434/v1, auth=none"
    );
    Ok(())
}

#[test]
fn keyed_provider_without_env_var_fails_at_resolution() -> Result<()> {
    let cfg = load(&config_with(
        "router",
        "api_key_env = \"OPENAI_API_KEY\"",
        &format!(
            "[llm.providers.router]\nbase_url = \"https://router.example/v1\"\napi_key_env = \"{MISSING_ENV}\""
        ),
    ))?;
    assert_eq!(
        cfg.llm.active_provider().unwrap_err(),
        ProviderError::MissingKey {
            provider: "router".into(),
            env: MISSING_ENV.into(),
        }
    );
    assert!(cfg
        .llm
        .provider_summary()
        .ends_with(&format!("auth=env:{MISSING_ENV} (missing)")));
    Ok(())
}

#[test]
fn undefined_provider_refuses_to_load() {
    let err = load(&config_with(
        "anthropic",
        "api_key_env = \"OPENAI_API_KEY\"",
        "",
    ))
    .expect_err("undefined provider must not load");
    assert!(
        format!("{err:#}").contains("\"anthropic\" is not defined in [llm.providers]"),
        "unexpected error: {err:#}"
    );
}

#[test]
fn bad_base_url_refuses_to_load() {
    let err = load(&config_with(
        "ollama",
        "api_key_env = \"OPENAI_API_KEY\"",
        "[llm.providers.ollama]\nbase_url = \"localhost:11434/v1\"",
    ))
    .expect_err("base_url without scheme must not load");
    assert!(
        format!("{err:#}").contains("llm.providers.ollama.base_url must start with http"),
        "unexpected error: {err:#}"
    );
}

#[test]
fn base_url_with_credentials_refuses_to_load_and_never_logs_them() {
    let err = load(&config_with(
        "gateway",
        "api_key_env = \"OPENAI_API_KEY\"",
        "[llm.providers.gateway]\nbase_url = \"https://user:s3cret-value@gateway.example/v1\"",
    ))
    .expect_err("base_url with credentials must not load");
    let shown = format!("{err:#}");
    assert!(
        shown.contains("llm.providers.gateway.base_url must not carry credentials"),
        "unexpected error: {shown}"
    );
    assert!(
        !shown.contains("s3cret-value"),
        "credentials leaked: {shown}"
    );
}
