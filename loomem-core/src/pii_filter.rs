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

/// True when the `[start, end)` match is embedded inside a longer ASCII
/// alphanumeric token — i.e. the byte before `start` or the byte at `end` is
/// an ASCII letter or digit. Used to reject phone-regex matches that are
/// really digit runs inside hex SHAs, UUIDs, unix timestamps, or build
/// numbers (issue #49); the `regex` crate has no lookaround, so the guard
/// lives here instead of in the pattern. ASCII-only on purpose: the
/// identifier false-positive class is ASCII, and treating non-ASCII bytes as
/// boundaries keeps redaction maximal for natural-language text.
/// Issue #67: a phone-shaped match sitting inside a whitespace-separated
/// numeric series (`0 50 100 150 200 250` — chart data, measurement columns)
/// is almost never a phone number. If the token immediately before or after
/// the match (separated by a single ASCII space) consists solely of digits,
/// treat the match as data and leave it alone. Trade-off: a real phone number
/// directly neighboured by a bare number survives unredacted; that is the
/// cheaper error, because redaction destroys persisted content permanently.
fn adjacent_bare_number(text: &str, start: usize, end: usize) -> bool {
    let bytes = text.as_bytes();
    // Token before: ... <digits>' '<match>
    let before = start
        .checked_sub(1)
        .and_then(|i| (bytes.get(i) == Some(&b' ')).then_some(i))
        .is_some_and(|space| {
            let tok_end = space;
            let mut tok_start = tok_end;
            while tok_start > 0 && bytes[tok_start - 1].is_ascii_digit() {
                tok_start -= 1;
            }
            tok_start < tok_end && (tok_start == 0 || !bytes[tok_start - 1].is_ascii_alphanumeric())
        });
    if before {
        return true;
    }
    // Token after: <match>' '<digits> ...
    if bytes.get(end) != Some(&b' ') {
        return false;
    }
    let tok_start = end + 1;
    let mut tok_end = tok_start;
    while tok_end < bytes.len() && bytes[tok_end].is_ascii_digit() {
        tok_end += 1;
    }
    tok_end > tok_start && (tok_end == bytes.len() || !bytes[tok_end].is_ascii_alphanumeric())
}

fn embedded_in_token(text: &str, start: usize, end: usize) -> bool {
    let bytes = text.as_bytes();
    // The left boundary only matters when the match itself opens with an
    // alphanumeric byte: a match that opens with '+' (international prefix)
    // is already delimited — the preceding byte cannot extend it into a
    // longer token (`Call+48123456789` is a phone, not an identifier).
    let starts_alnum = bytes.get(start).is_some_and(|b| b.is_ascii_alphanumeric());
    let before_alnum = starts_alnum
        && start
            .checked_sub(1)
            .and_then(|i| bytes.get(i))
            .is_some_and(|b| b.is_ascii_alphanumeric());
    let after_alnum = bytes.get(end).is_some_and(|b| b.is_ascii_alphanumeric());
    before_alnum || after_alnum
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

        // Redact phones. Each match is validated against its surrounding
        // bytes (issue #49: digit runs inside hex SHAs / UUIDs / timestamps
        // must survive), and the output is rebuilt in a single pass —
        // `str::replace` on the raw match text would hit every identical
        // substring and desynchronize the recorded positions.
        if self.config.redact_phones {
            let mut rebuilt = String::with_capacity(sanitized.len());
            let mut last = 0usize;
            for mat in self.phone_regex.find_iter(&sanitized) {
                if embedded_in_token(&sanitized, mat.start(), mat.end())
                    || adjacent_bare_number(&sanitized, mat.start(), mat.end())
                {
                    continue;
                }
                rebuilt.push_str(&sanitized[last..mat.start()]);
                rebuilt.push_str("[PHONE]");
                redactions.push(PiiRedaction {
                    redaction_type: "phone".to_string(),
                    original_length: mat.as_str().len(),
                    position: mat.start(),
                });
                last = mat.end();
            }
            rebuilt.push_str(&sanitized[last..]);
            sanitized = rebuilt;
        }

        // Redact emails. Positional rebuild, same as the phone branch — a
        // global `str::replace` hits every identical substring and was the
        // failure class behind issue #67.
        if self.config.redact_emails {
            let mut rebuilt = String::with_capacity(sanitized.len());
            let mut last = 0usize;
            for mat in self.email_regex.find_iter(&sanitized) {
                rebuilt.push_str(&sanitized[last..mat.start()]);
                rebuilt.push_str("[EMAIL]");
                redactions.push(PiiRedaction {
                    redaction_type: "email".to_string(),
                    original_length: mat.as_str().len(),
                    position: mat.start(),
                });
                last = mat.end();
            }
            rebuilt.push_str(&sanitized[last..]);
            sanitized = rebuilt;
        }

        // Redact IDs (PESEL). Positional rebuild for the same reason.
        if self.config.redact_ids {
            let mut rebuilt = String::with_capacity(sanitized.len());
            let mut last = 0usize;
            for mat in self.id_regex.find_iter(&sanitized) {
                rebuilt.push_str(&sanitized[last..mat.start()]);
                rebuilt.push_str("[ID]");
                redactions.push(PiiRedaction {
                    redaction_type: "id".to_string(),
                    original_length: mat.as_str().len(),
                    position: mat.start(),
                });
                last = mat.end();
            }
            rebuilt.push_str(&sanitized[last..]);
            sanitized = rebuilt;
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

    /// Ingress redaction for any third-party / persistence sink: HTML &
    /// prompt-injection strip ([`crate::sanitizer::sanitize`]) followed by PII
    /// redaction ([`Self::sanitize`]). Returns only the redacted string,
    /// dropping the redaction list (callers that need it call `sanitize`
    /// directly). Idempotent: running it on already-redacted text is a no-op,
    /// so `persist_chunk` re-applying the same pipeline is safe.
    ///
    /// Use this at every write ingress (REST/MCP) before content reaches an
    /// embedding, contradiction, event-date, content-type, or extraction
    /// request, so raw caller text never leaves the process unredacted.
    pub fn redact_for_sink(&self, raw: &str) -> String {
        self.sanitize(&crate::sanitizer::sanitize(raw).content).0
    }

    /// Recursively redact every string position of a JSON value via
    /// [`Self::redact_for_sink`] — both object **keys** and string values, at
    /// every depth — preserving array order and non-string scalars (numbers,
    /// bools, null carry no free-text PII). Use on caller-supplied `metadata`
    /// before it is persisted into a `Chunk` or the legacy `event:` record, so
    /// PII in metadata is redacted the same way `content` is — not just the
    /// visible content field.
    ///
    /// `serde_json::Value` is a closed set of six variants, all handled here,
    /// so there is no "unsupported shape" to reject: redaction is total and
    /// cannot be silently bypassed. Note that two distinct PII keys can redact
    /// to the same placeholder and collide (last wins) — acceptable, since
    /// PII-in-keys is already pathological and the goal is non-disclosure, not
    /// preservation. Depth is bounded in practice by the caller's metadata size
    /// limit.
    pub fn sanitize_json(&self, value: &serde_json::Value) -> serde_json::Value {
        use serde_json::Value;
        match value {
            Value::String(s) => Value::String(self.redact_for_sink(s)),
            Value::Array(items) => {
                Value::Array(items.iter().map(|v| self.sanitize_json(v)).collect())
            }
            Value::Object(map) => Value::Object(
                map.iter()
                    .map(|(k, v)| (self.redact_for_sink(k), self.sanitize_json(v)))
                    .collect(),
            ),
            // Number / Bool / Null: no free-text to redact.
            scalar => scalar.clone(),
        }
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

    #[test]
    fn test_redact_for_sink_redacts_and_is_idempotent() {
        let config = PiiConfig {
            enabled: true,
            blocklist_file: "nonexistent.txt".to_string(),
            ..Default::default()
        };
        let filter = PiiFilter::new(config).expect("Failed to create filter");

        let raw = "Reach me at test@example.com or +48 600 000 000";
        let redacted = filter.redact_for_sink(raw);

        // No raw PII survives to a third-party / persistence sink.
        assert!(!redacted.contains("test@example.com"));
        assert!(!redacted.contains("600 000 000"));
        assert!(redacted.contains("[EMAIL]"));
        assert!(redacted.contains("[PHONE]"));

        // Idempotent: re-running over already-redacted text is a no-op, so
        // `persist_chunk` re-applying the same pipeline cannot corrupt content.
        assert_eq!(filter.redact_for_sink(&redacted), redacted);
    }

    #[test]
    fn test_sanitize_json_redacts_string_leaves_recursively() {
        let config = PiiConfig {
            enabled: true,
            blocklist_file: "nonexistent.txt".to_string(),
            ..Default::default()
        };
        let filter = PiiFilter::new(config).expect("Failed to create filter");

        let input = serde_json::json!({
            "created_by": "agent-7",
            "note": "ping test@example.com",
            "nested": { "phone": "+48 600 000 000" },
            "tags": ["plain", "id 12345678901"],
            "owner@example.com": "last seen",
            "count": 42,
            "active": true,
            "missing": null
        });
        let out = filter.sanitize_json(&input);

        // String leaves at every depth are redacted; raw PII never survives —
        // including PII embedded in object keys.
        let s = out.to_string();
        assert!(!s.contains("test@example.com"));
        assert!(!s.contains("600 000 000"));
        assert!(!s.contains("12345678901"));
        assert!(!s.contains("owner@example.com"));
        assert_eq!(out["note"], serde_json::json!("ping [EMAIL]"));
        assert_eq!(out["nested"]["phone"], serde_json::json!("[PHONE]"));
        assert_eq!(out["tags"][0], serde_json::json!("plain"));

        // The PII key is redacted to a placeholder; the raw key is gone.
        assert!(out.get("owner@example.com").is_none());
        assert_eq!(out["[EMAIL]"], serde_json::json!("last seen"));

        // Structure and non-string scalars are preserved.
        assert_eq!(out["created_by"], serde_json::json!("agent-7"));
        assert_eq!(out["count"], serde_json::json!(42));
        assert_eq!(out["active"], serde_json::json!(true));
        assert_eq!(out["missing"], serde_json::Value::Null);
    }

    #[test]
    fn test_pii_filter_preserves_identifiers_with_digit_runs() {
        // Issue #49: digit runs inside hex SHAs, UUIDs, unix timestamps, and
        // build numbers must round-trip unchanged instead of being redacted
        // as phone numbers.
        let config = PiiConfig {
            enabled: true,
            blocklist_file: "nonexistent.txt".to_string(),
            ..Default::default()
        };
        let filter = PiiFilter::new(config).expect("Failed to create filter");

        let cases = [
            "commit a3f8b2c1d4123456789e5f6a7b8c9d0e1f2a3b4 pushed",
            "chunk id 550e8400-e29b-41d4-a716-446655440000",
            "seen at 1751234567 (unix)",
            "release v0.5.1 build 202607051330 finished",
        ];
        for case in cases {
            let (sanitized, redactions) = filter.sanitize(case);
            assert_eq!(sanitized, case, "identifier corrupted in: {case}");
            assert!(
                redactions.is_empty(),
                "spurious redaction in: {case} -> {redactions:?}"
            );
        }
    }

    #[test]
    fn test_pii_filter_phone_variants_still_redact() {
        // The issue-#49 boundary guard must not weaken real phone redaction
        // (hard rule 1): standalone numbers in every supported format still
        // redact, including at punctuation and string boundaries.
        let config = PiiConfig {
            enabled: true,
            redact_phones: true,
            blocklist_file: "nonexistent.txt".to_string(),
            ..Default::default()
        };
        let filter = PiiFilter::new(config).expect("Failed to create filter");

        for phone in ["+48 600 000 000", "600 000 000", "600-000-000", "600000000"] {
            let text = format!("tel: {phone}!");
            let (sanitized, redactions) = filter.sanitize(&text);
            assert_eq!(sanitized, "tel: [PHONE]!", "not redacted: {text}");
            assert_eq!(redactions.len(), 1, "expected 1 redaction in: {text}");
            assert_eq!(redactions[0].redaction_type, "phone");
        }
    }

    #[test]
    fn test_pii_filter_plus_prefixed_phone_glued_to_text_still_redacts() {
        // Greptile P1 on PR #50: a '+'-prefixed phone directly after ASCII
        // text is still a standalone phone — the '+' breaks the alphanumeric
        // token, so the left-boundary guard must not treat it as embedded.
        let config = PiiConfig {
            enabled: true,
            redact_phones: true,
            blocklist_file: "nonexistent.txt".to_string(),
            ..Default::default()
        };
        let filter = PiiFilter::new(config).expect("Failed to create filter");

        let (sanitized, redactions) = filter.sanitize("Call+48123456789 now");
        assert_eq!(sanitized, "Call[PHONE] now");
        // Only the output text and the presence of a phone redaction are
        // asserted: `id_regex` also matches the 11-digit tail in the
        // *original* text and records a no-op entry — the pre-existing
        // scan-original/replace-in-sanitized wart deferred in #49.
        assert!(redactions.iter().any(|r| r.redaction_type == "phone"));
    }

    #[test]
    fn test_pii_filter_duplicate_phones_redact_each_with_correct_position() {
        // The single-pass rebuild records one redaction per occurrence with
        // positions in the pre-redaction text (the old `str::replace` path
        // replaced globally per match and drifted positions).
        let config = PiiConfig {
            enabled: true,
            redact_phones: true,
            blocklist_file: "nonexistent.txt".to_string(),
            ..Default::default()
        };
        let filter = PiiFilter::new(config).expect("Failed to create filter");

        let text = "600000000 oraz 600000000";
        let (sanitized, redactions) = filter.sanitize(text);

        assert_eq!(sanitized, "[PHONE] oraz [PHONE]");
        assert_eq!(redactions.len(), 2);
        assert_eq!(redactions[0].position, 0);
        assert_eq!(redactions[1].position, 15);
    }

    // ---- issue #67 regression coverage ----

    fn filter_all_on() -> PiiFilter {
        let config = PiiConfig {
            enabled: true,
            redact_phones: true,
            redact_emails: true,
            redact_ids: true,
            ..Default::default()
        };
        PiiFilter::new(config).expect("Failed to create filter")
    }

    #[test]
    fn numeric_series_survives_unredacted() {
        // `100 150 200` is phone-shaped, but its bare-number neighbours mark
        // it as a whitespace-separated series (chart data), not a phone.
        let filter = filter_all_on();
        let text = "0 50 100 150 200 250";
        let (sanitized, redactions) = filter.sanitize(text);
        assert_eq!(sanitized, text);
        assert!(redactions.is_empty(), "series redacted: {redactions:?}");
    }

    #[test]
    fn hyphenated_hotline_survives_unredacted() {
        // `800-772-121` matches the phone shape but is embedded in the longer
        // hotline token — the trailing digit disqualifies it.
        let filter = filter_all_on();
        let text = "Call 1-800-772-1213 for social security help";
        let (sanitized, redactions) = filter.sanitize(text);
        assert_eq!(sanitized, text);
        assert!(redactions.is_empty(), "hotline redacted: {redactions:?}");
    }

    #[test]
    fn long_digit_run_survives_unredacted() {
        // 13 contiguous digits: too long for a phone (embedded guard) and not
        // an 11-digit PESEL.
        let filter = filter_all_on();
        let text = "order 1234567890123 confirmed";
        let (sanitized, redactions) = filter.sanitize(text);
        assert_eq!(sanitized, text);
        assert!(redactions.is_empty(), "digit run redacted: {redactions:?}");
    }

    #[test]
    fn real_phone_still_redacted_next_to_words() {
        // The series guard must not weaken ordinary phone redaction.
        let filter = filter_all_on();
        let (sanitized, redactions) = filter.sanitize("zadzwo\u{144} pod 123 456 789 wieczorem");
        assert_eq!(sanitized, "zadzwo\u{144} pod [PHONE] wieczorem");
        assert_eq!(redactions.len(), 1);
    }

    #[test]
    fn duplicate_emails_redacted_positionally() {
        // The email branch used a global str::replace before — the failure
        // class of issue #67. Both copies must go, with distinct positions.
        let filter = filter_all_on();
        let text = "a@b.com wrote to a@b.com";
        let (sanitized, redactions) = filter.sanitize(text);
        assert_eq!(sanitized, "[EMAIL] wrote to [EMAIL]");
        assert_eq!(redactions.len(), 2);
        assert_ne!(redactions[0].position, redactions[1].position);
    }
}
