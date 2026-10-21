//! Redact provider-only copies; source bytes remain unchanged for lifecycle binding.
use super::ChunkBatchEntry;
use crate::pii_filter::PiiFilter;

/// Keep the engine's opaque correlation ID and apply configured privacy policy
/// to all user-derived prompt content, including dictionary hints and types.
pub fn prepare_entry(
    id: &str,
    content: &str,
    hints: &[(String, String)],
    pii: &PiiFilter,
) -> ChunkBatchEntry {
    (
        id.to_string(),
        pii.redact_for_sink(content),
        hints
            .iter()
            .map(|(name, kind)| (pii.redact_for_sink(name), pii.redact_for_sink(kind)))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn provider_prompt_redacts_configured_content_and_hints_without_mutating_source() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("blocked.txt");
        std::fs::write(&file, "CedarHidden\n").unwrap();
        let pii = PiiFilter::new(crate::pii_filter::PiiConfig {
            blocklist_file: file.to_str().unwrap().to_string(),
            ..Default::default()
        })
        .unwrap();
        let content = "<b>synthetic@example.test</b> CedarHidden".to_string();
        let hints = vec![(
            "synthetic@example.test CedarHidden".into(),
            "<b>CedarHidden</b>".into(),
        )];
        let before = (content.clone(), hints.clone());
        let entry = prepare_entry("opaque-fixture-id", &content, &hints, &pii);
        let prompt = super::super::build_prompt(std::slice::from_ref(&entry));
        for raw in ["synthetic@example.test", "CedarHidden", "<b>"] {
            assert!(!prompt.contains(raw), "{raw}");
        }
        assert_eq!(entry.0, "opaque-fixture-id");
        assert_eq!((content, hints), before);
    }
}
