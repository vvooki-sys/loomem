//! `[search] content_dedup`: how `filter_candidates` collapses duplicate
//! results. The dedup pass itself lives in the server
//! (`handlers/search/prefix_dedup.rs`); this module only holds the mode.

use serde::{Deserialize, Serialize};

/// Result dedup rule. Unknown values are rejected when the config loads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentDedup {
    /// Drop a result whose first 150 lowercase chars share a leading run
    /// longer than 80% of the shorter of the two (both at least 30 bytes),
    /// except fragments of different sources and a fragment against its own
    /// source (the previous behaviour, byte-identical).
    #[default]
    Prefix,
    /// Collapse only byte-identical plain chunks of at least 30 bytes and
    /// never fragments, so a negation or update after a shared prefix
    /// survives.
    Exact,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Deserialize)]
    struct Holder {
        #[serde(default)]
        content_dedup: ContentDedup,
    }

    fn parse(toml_text: &str) -> Result<ContentDedup, toml::de::Error> {
        toml::from_str::<Holder>(toml_text).map(|holder| holder.content_dedup)
    }

    #[test]
    fn default_is_prefix() {
        assert_eq!(ContentDedup::default(), ContentDedup::Prefix);
        assert_eq!(parse("").unwrap(), ContentDedup::Prefix);
    }

    #[test]
    fn snake_case_values_round_trip() {
        for (text, mode) in [
            ("prefix", ContentDedup::Prefix),
            ("exact", ContentDedup::Exact),
        ] {
            assert_eq!(parse(&format!("content_dedup = \"{text}\"")).unwrap(), mode);
            assert_eq!(serde_json::to_value(mode).unwrap(), text);
        }
    }

    #[test]
    fn unknown_value_is_rejected() {
        assert!(parse("content_dedup = \"bogus\"").is_err());
        assert!(parse("content_dedup = \"Exact\"").is_err());
    }
}
