use anyhow::{Context, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::path::Path;
use tracing::{debug, info};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PiiConfig {
    pub enabled: bool,
    pub redact_phones: bool,
    pub redact_emails: bool,
    pub redact_ids: bool,
    pub blocklist_file: String,
    pub audit_log: bool,
}

impl Default for PiiConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            redact_phones: true,
            redact_emails: true,
            redact_ids: true,
            blocklist_file: "pii_blocklist.txt".to_string(),
            audit_log: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PiiRedaction {
    pub redaction_type: String,
    pub original_length: usize,
    pub position: usize,
}

pub struct PiiFilter {
    config: PiiConfig,
    phone_regex: Regex,
    email_regex: Regex,
    id_regex: Regex,
    blocklist: HashSet<String>,
}

impl PiiFilter {
    pub fn new(config: PiiConfig) -> Result<Self> {
        // Phone regex: matches various Polish phone formats (+48, 0xx)
        let phone_regex = Regex::new(
            r"(?:\+48\s?)?(?:\d{3}[\s\-]?\d{3}[\s\-]?\d{3}|\d{2}[\s\-]?\d{3}[\s\-]?\d{2}[\s\-]?\d{2})"
        ).context("Failed to compile phone regex")?;

        // Email regex: basic email pattern
        let email_regex = Regex::new(r"\b[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Z|a-z]{2,}\b")
            .context("Failed to compile email regex")?;

        // ID regex: 11-digit PESEL
        let id_regex = Regex::new(r"\b\d{11}\b").context("Failed to compile ID regex")?;

        // Load blocklist from file
        let blocklist = if config.enabled && Path::new(&config.blocklist_file).exists() {
            let content = fs::read_to_string(&config.blocklist_file).with_context(|| {
                format!("Failed to read blocklist file: {}", config.blocklist_file)
            })?;

            let words: HashSet<String> = content
                .lines()
                .map(|line| line.trim().to_lowercase())
                .filter(|line| !line.is_empty() && !line.starts_with('#'))
                .collect();

            info!("Loaded {} words from PII blocklist", words.len());
            words
        } else {
            if config.enabled {
                info!(
                    "PII blocklist file not found: {}, proceeding without blocklist",
                    config.blocklist_file
                );
            }
            HashSet::new()
        };

        Ok(Self {
            config,
            phone_regex,
            email_regex,
            id_regex,
            blocklist,
        })
    }

    pub fn sanitize(&self, text: &str) -> (String, Vec<PiiRedaction>) {
        if !self.config.enabled {
            return (text.to_string(), Vec::new());
        }

        let mut sanitized = text.to_string();
        let mut redactions = Vec::new();

        // Redact phones
        if self.config.redact_phones {
            for mat in self.phone_regex.find_iter(text) {
                let original_len = mat.as_str().len();
                sanitized = sanitized.replace(mat.as_str(), "[PHONE]");
                redactions.push(PiiRedaction {
                    redaction_type: "phone".to_string(),
                    original_length: original_len,
                    position: mat.start(),
                });
            }
        }

        // Redact emails
        if self.config.redact_emails {
            for mat in self.email_regex.find_iter(text) {
                let original_len = mat.as_str().len();
                sanitized = sanitized.replace(mat.as_str(), "[EMAIL]");
                redactions.push(PiiRedaction {
                    redaction_type: "email".to_string(),
                    original_length: original_len,
                    position: mat.start(),
                });
            }
        }

        // Redact IDs (PESEL)
        if self.config.redact_ids {
            for mat in self.id_regex.find_iter(text) {
                let original_len = mat.as_str().len();
                sanitized = sanitized.replace(mat.as_str(), "[ID]");
                redactions.push(PiiRedaction {
                    redaction_type: "id".to_string(),
                    original_length: original_len,
                    position: mat.start(),
                });
            }
        }

        // Redact blocklist words
        if !self.blocklist.is_empty() {
            for word in &self.blocklist {
                // Case-insensitive replacement
                let word_lower = word.to_lowercase();
                let mut search_text = sanitized.to_lowercase();
                let mut offset = 0;

                while let Some(pos) = search_text.find(&word_lower) {
                    let actual_pos = offset + pos;
                    let end_pos = actual_pos + word.len();

                    // Replace in the original text
                    sanitized.replace_range(actual_pos..end_pos, "[REDACTED]");

                    redactions.push(PiiRedaction {
                        redaction_type: "blocklist".to_string(),
                        original_length: word.len(),
                        position: actual_pos,
                    });

                    // Update for next iteration
                    offset = actual_pos + "[REDACTED]".len();
                    search_text = sanitized[offset..].to_lowercase();
                }
            }
        }

        // Audit log
        if self.config.audit_log && !redactions.is_empty() {
            info!("PII redactions applied: {} items", redactions.len());
            for redaction in &redactions {
                debug!(
                    "Redacted {} at position {}",
                    redaction.redaction_type, redaction.position
                );
            }
        }

        (sanitized, redactions)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_pii_passthrough() {
        let config = PiiConfig {
            enabled: false,
            ..Default::default()
        };
        let filter = PiiFilter::new(config).expect("Failed to create filter");

        let text = "test@example.com +48 600 000 000";
        let (sanitized, redactions) = filter.sanitize(text);

        assert_eq!(sanitized, text);
        assert_eq!(redactions.len(), 0);
    }

    #[test]
    fn test_pii_filter_email() {
        let config = PiiConfig {
            enabled: true,
            redact_emails: true,
            blocklist_file: "nonexistent.txt".to_string(),
            ..Default::default()
        };
        let filter = PiiFilter::new(config).expect("Failed to create filter");

        let text = "Contact me at test@example.com";
        let (sanitized, redactions) = filter.sanitize(text);

        assert_eq!(sanitized, "Contact me at [EMAIL]");
        assert_eq!(redactions.len(), 1);
        assert_eq!(redactions[0].redaction_type, "email");
    }

    #[test]
    fn test_pii_filter_phone() {
        let config = PiiConfig {
            enabled: true,
            redact_phones: true,
            blocklist_file: "nonexistent.txt".to_string(),
            ..Default::default()
        };
        let filter = PiiFilter::new(config).expect("Failed to create filter");

        let text = "Call me at +48 600 000 000";
        let (sanitized, redactions) = filter.sanitize(text);

        assert_eq!(sanitized, "Call me at [PHONE]");
        assert_eq!(redactions.len(), 1);
        assert_eq!(redactions[0].redaction_type, "phone");
    }

    #[test]
    fn test_pii_filter_blocklist() -> Result<()> {
        let mut temp_file = NamedTempFile::new()?;
        writeln!(temp_file, "secret")?;
        writeln!(temp_file, "confidential")?;
        temp_file.flush()?;

        let config = PiiConfig {
            enabled: true,
            blocklist_file: temp_file.path().to_string_lossy().to_string(),
            ..Default::default()
        };
        let filter = PiiFilter::new(config)?;

        let text = "This is secret information";
        let (sanitized, redactions) = filter.sanitize(text);

        assert!(sanitized.contains("[REDACTED]"));
        assert!(redactions.iter().any(|r| r.redaction_type == "blocklist"));

        Ok(())
    }
}
