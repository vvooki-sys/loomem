//! Per-feature LLM models inherit `[llm] compression_model` when unset.
//!
//! A section such as `[contradiction]` can name its own `model`. When the key
//! is missing or empty (or the whole section is missing), the feature uses the
//! main completion model. Then a switch to another provider, for example local
//! Ollama, needs only one model setting.
use super::Config;

impl Config {
    /// Set every unset (empty) per-feature `model` to `[llm] compression_model`.
    /// An explicit model in a section always wins.
    pub fn inherit_feature_models(&mut self) {
        let inherited = self.llm.compression_model.clone();
        for model in [
            &mut self.knowledge_extraction.model,
            &mut self.entity_extraction.model,
            &mut self.memory_generator.model,
            &mut self.contradiction.model,
            &mut self.profile.model,
            &mut self.manifest.model,
            &mut self.dream.model,
            &mut self.content_type.model,
        ] {
            if model.trim().is_empty() {
                model.clone_from(&inherited);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shipped_config() -> String {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../config.toml");
        std::fs::read_to_string(path).expect("read config.toml")
    }

    fn models(cfg: &Config) -> [&str; 8] {
        [
            &cfg.knowledge_extraction.model,
            &cfg.entity_extraction.model,
            &cfg.memory_generator.model,
            &cfg.contradiction.model,
            &cfg.profile.model,
            &cfg.manifest.model,
            &cfg.dream.model,
            &cfg.content_type.model,
        ]
        .map(String::as_str)
    }

    #[test]
    fn shipped_config_features_inherit_compression_model() {
        let content = shipped_config().replace(
            r#"compression_model = "gpt-4.1-mini""#,
            r#"compression_model = "qwen2.5:3b""#,
        );
        let mut cfg: Config = toml::from_str(&content).expect("parse config.toml");
        cfg.inherit_feature_models();
        assert_eq!(models(&cfg), ["qwen2.5:3b"; 8]);
    }

    #[test]
    fn explicit_section_model_wins_and_empty_inherits() {
        let content = shipped_config()
            .replace(
                "[contradiction]\n",
                "[contradiction]\nmodel = \"llama3.2:3b\"\n",
            )
            .replace("[profile]\n", "[profile]\nmodel = \" \"\n");
        let mut cfg: Config = toml::from_str(&content).expect("parse config.toml");
        cfg.inherit_feature_models();
        assert_eq!(cfg.contradiction.model, "llama3.2:3b");
        assert_eq!(cfg.profile.model, cfg.llm.compression_model);
        assert_eq!(cfg.dream.model, cfg.llm.compression_model);
    }
}
