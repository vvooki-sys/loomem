use serde::{Deserialize, Deserializer, Serialize};

/// Structured source tag for multi-agent provenance tracking.
///
/// Identifies which agent wrote a chunk, through which channel,
/// and optionally in which session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SourceTag {
    /// Agent identifier: "legacy-agent" | "claude-code" | "claude-ai" | "mcp-remote" | "cursor" | "api"
    pub agent: String,
    /// Optional session identifier for traceability
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// Channel through which the chunk arrived: "telegram" | "mcp" | "http" | "cli"
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
}

impl SourceTag {
    /// Create a SourceTag with just an agent name (convenience)
    pub fn from_agent(agent: impl Into<String>) -> Self {
        Self {
            agent: agent.into(),
            session: None,
            channel: None,
        }
    }

    /// Default fallback for chunks without source info
    pub fn unknown() -> Self {
        Self::from_agent("unknown")
    }
}

impl Default for SourceTag {
    fn default() -> Self {
        Self::from_agent("api")
    }
}

impl std::fmt::Display for SourceTag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.agent)?;
        if let Some(ch) = &self.channel {
            write!(f, "/{}", ch)?;
        }
        if let Some(sess) = &self.session {
            write!(f, "@{}", sess)?;
        }
        Ok(())
    }
}

// ── Backward-compatible deserialization ──
// Old data has `source: "some string"` → deserialize as SourceTag { agent: "some string", .. }
// New data has `source: { agent: "...", session: "...", channel: "..." }`

/// Use this as a custom deserializer for the Chunk.source field:
///   #[serde(default, deserialize_with = "deserialize_source_compat")]
///   pub source: Option<SourceTag>,
pub fn deserialize_source_compat<'de, D>(deserializer: D) -> Result<Option<SourceTag>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum SourceCompat {
        Tag(SourceTag),
        LegacyString(String),
        Null,
    }

    // Option wrapper handles null/missing at the outer level
    let opt: Option<SourceCompat> = Option::deserialize(deserializer)?;
    match opt {
        None => Ok(None),
        Some(SourceCompat::Tag(tag)) => Ok(Some(tag)),
        Some(SourceCompat::LegacyString(s)) if s.is_empty() => Ok(None),
        Some(SourceCompat::LegacyString(s)) => Ok(Some(SourceTag::from_agent(s))),
        Some(SourceCompat::Null) => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_source_tag_display() {
        let tag = SourceTag {
            agent: "claude-code".into(),
            session: Some("sess-123".into()),
            channel: Some("mcp".into()),
        };
        assert_eq!(tag.to_string(), "claude-code/mcp@sess-123");
    }

    #[test]
    fn test_backward_compat_string() {
        // Simulates old JSON: { "source": "legacy-agent" }
        #[derive(Deserialize)]
        struct TestChunk {
            #[serde(default, deserialize_with = "deserialize_source_compat")]
            source: Option<SourceTag>,
        }

        let json = r#"{"source": "legacy-agent"}"#;
        let chunk: TestChunk = serde_json::from_str(json).unwrap();
        assert_eq!(chunk.source.unwrap().agent, "legacy-agent");
    }

    #[test]
    fn test_backward_compat_struct() {
        #[derive(Deserialize)]
        struct TestChunk {
            #[serde(default, deserialize_with = "deserialize_source_compat")]
            source: Option<SourceTag>,
        }

        let json = r#"{"source": {"agent": "claude-ai", "channel": "mcp"}}"#;
        let chunk: TestChunk = serde_json::from_str(json).unwrap();
        let src = chunk.source.unwrap();
        assert_eq!(src.agent, "claude-ai");
        assert_eq!(src.channel.as_deref(), Some("mcp"));
    }

    #[test]
    fn test_backward_compat_null() {
        #[derive(Deserialize)]
        struct TestChunk {
            #[serde(default, deserialize_with = "deserialize_source_compat")]
            source: Option<SourceTag>,
        }

        let json = r#"{"source": null}"#;
        let chunk: TestChunk = serde_json::from_str(json).unwrap();
        assert!(chunk.source.is_none());
    }

    #[test]
    fn test_backward_compat_missing() {
        #[derive(Deserialize)]
        struct TestChunk {
            #[serde(default, deserialize_with = "deserialize_source_compat")]
            source: Option<SourceTag>,
        }

        let json = r#"{}"#;
        let chunk: TestChunk = serde_json::from_str(json).unwrap();
        assert!(chunk.source.is_none());
    }
}
