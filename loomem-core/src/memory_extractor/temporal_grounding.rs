//! Temporal grounding at extraction (`[knowledge_extraction] temporal_grounding`).
//!
//! Rejects relative time introduced by extraction rather than supported by the
//! input: a fact such as "The user visited the festival three weeks ago" from a
//! source that only says "I just got back", or an `event_date_context` the
//! source never contains. Off by default; with [`TemporalGrounding::Off`] the
//! prompt bytes, the admitted facts (same predicate, same order) and the logs
//! are identical to the pre-option pipeline.
//!
//! The check compares whitespace-normalized, lowercased text. It fails closed:
//! if the relative-cue pattern ever failed to compile, every fact would be
//! rejected while grounding is on.
//!
//! The cue pattern ([`RELATIVE_CUE_PATTERN`], kept verbatim from the original
//! port) covers only some relative expressions; a fact using any other form
//! passes the content check unexamined. Known forms it does not cover:
//! - English: "last night", "this morning", "next week", "in two weeks",
//!   "last Friday/weekend/summer", "a couple of weeks ago", "a few/several
//!   days ago", and number words above ten ("eleven days ago").
//! - Polish: "dziś", "przedwczoraj", "w zeszłym tygodniu",
//!   "tydzień/miesiąc/rok temu", and number words above five ("sześć dni
//!   temu" and up).

use super::ExtractedFact;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;
use tracing::debug;

/// How extraction treats relative time that the source does not support.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TemporalGrounding {
    /// No grounding check; prompt and admission are unchanged.
    #[default]
    Off,
    /// Drop facts whose relative cue or `event_date_context` is absent from
    /// the source chunk. The extraction prompt is unchanged.
    Filter,
    /// [`TemporalGrounding::Filter`], plus a TEMPORAL GROUNDING notice
    /// prepended to the extraction system prompt (changes LLM prompt bytes).
    PromptAndFilter,
}

const NOTICE: &str = "TEMPORAL GROUNDING: Preserve the source's temporal wording. Never introduce a relative duration (such as 'three weeks ago') or convert an absolute date to a relative expression. Copy event_date_context verbatim from the source, or leave it null. Resolve an absolute event_date only from a source cue and the supplied conversation_date. 'Just got back' does not establish an exact number of days or weeks. Keep unknown dates unknown. Never use the current date, the reader's question date, or information absent from this transcript.";

/// Relative-time cues checked in a fact's content. Not exhaustive: see the
/// module docs for the forms it does not match.
const RELATIVE_CUE_PATTERN: &str = r"(?i)\b(?:(?:\d+|one|two|three|four|five|six|seven|eight|nine|ten|a|an)\s+(?:days?|weeks?|months?|years?|hours?)\s+ago|yesterday|tomorrow|today|last\s+(?:week|month|year)|(?:\d+|jeden|dwa|trzy|cztery|pięć)\s+(?:dni|tygodnie|tygodni|miesiące|miesięcy|lata|lat)\s+temu|wczoraj|dzisiaj|jutro)\b";

/// Prepend the grounding notice to a prompt body under
/// [`TemporalGrounding::PromptAndFilter`]; the body is returned unchanged
/// (moved, not copied) under `Off` and `Filter`.
pub(super) fn with_notice(mode: TemporalGrounding, body: String) -> String {
    match mode {
        TemporalGrounding::PromptAndFilter => format!("{NOTICE}\n\n{body}"),
        TemporalGrounding::Off | TemporalGrounding::Filter => body,
    }
}

/// Facts admitted from one chunk's run: the confidence filter (`>= 0.5`),
/// plus the grounding check against `source` unless `mode` is `Off`. Order is
/// preserved. The second value counts confident facts rejected by grounding:
/// `None` when `mode` is `Off` (no check ran), otherwise `Some(n)`.
pub(super) fn admissible(
    facts: Vec<ExtractedFact>,
    source: &str,
    mode: TemporalGrounding,
) -> (Vec<ExtractedFact>, Option<usize>) {
    let confident = facts.into_iter().filter(|f| f.confidence >= 0.5);
    if mode == TemporalGrounding::Off {
        return (confident.collect(), None);
    }
    let grounding = Grounding::new(source);
    let (kept, rejected): (Vec<_>, Vec<_>) = confident.partition(|f| grounding.admits(f));
    if !rejected.is_empty() {
        debug!("temporal grounding rejected {} fact(s)", rejected.len());
    }
    (kept, Some(rejected.len()))
}

fn normalized(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn relative_cue_pattern() -> Option<&'static Regex> {
    static RELATIVE: OnceLock<Option<Regex>> = OnceLock::new();
    RELATIVE
        .get_or_init(|| Regex::new(RELATIVE_CUE_PATTERN).ok())
        .as_ref()
}

/// A source chunk, normalized once, against which facts are checked.
struct Grounding {
    source: String,
}

impl Grounding {
    fn new(source: &str) -> Self {
        Self {
            source: normalized(source),
        }
    }

    fn admits(&self, fact: &ExtractedFact) -> bool {
        if fact
            .event_date_context
            .as_deref()
            .is_some_and(|cue| !cue.trim().is_empty() && !self.supports(cue))
        {
            return false;
        }
        let Some(pattern) = relative_cue_pattern() else {
            return false;
        };
        pattern
            .find_iter(&fact.content)
            .all(|cue| self.supports(widened_cue(&fact.content, cue.start(), cue.end())))
    }

    /// The source contains `cue` as a complete expression (see
    /// [`contains_phrase`]), so "3 weeks ago" is not supported by
    /// "13 weeks ago", "2-3 weeks ago" or "1.5 weeks ago".
    fn supports(&self, cue: &str) -> bool {
        contains_phrase(&self.source, &normalized(cue))
    }
}

/// The cue at `content[start..end]`, widened back over the decimal or range
/// its leading number belongs to: the pattern starts a numeric cue at the
/// last digit run, so "1.5 weeks ago" or "2-3 weeks ago" would otherwise be
/// checked as "5 weeks ago" or "3 weeks ago", which [`glued`] never finds in
/// a source that says the same thing.
fn widened_cue(content: &str, start: usize, end: usize) -> &str {
    let cue = content.get(start..end).unwrap_or("");
    if !cue.starts_with(|c: char| c.is_ascii_digit()) {
        return cue;
    }
    let numeric = |c: &char| c.is_ascii_digit() || matches!(c, '.' | ',' | '-');
    let head = content.get(..start).unwrap_or("");
    let prefix: usize = head
        .chars()
        .rev()
        .take_while(numeric)
        .map(char::len_utf8)
        .sum();
    content
        .get(start.saturating_sub(prefix)..end)
        .unwrap_or(cue)
        .trim_start_matches(['.', ',', '-'])
}

/// `needle` occurs in `haystack` with neither alphanumeric edge glued to the
/// surrounding text (see [`glued`]).
fn contains_phrase(haystack: &str, needle: &str) -> bool {
    haystack.match_indices(needle).any(|(start, _)| {
        let end = start + needle.len();
        let mut before = haystack.get(..start).unwrap_or("").chars().rev();
        let mut after = haystack.get(end..).unwrap_or("").chars();
        let first = needle.chars().next();
        let last = needle.chars().next_back();
        !glued(first, before.next(), before.next()) && !glued(last, after.next(), after.next())
    })
}

/// A match edge `edge` is glued to the text outside it when the adjacent
/// character `outer` is a letter or digit, or when `outer` is `.`, `,` or `-`
/// with a digit `beyond` it (a decimal, a thousands group or a range such as
/// "1.5", "1,5" or "2-3"). Non-alphanumeric edges never glue.
fn glued(edge: Option<char>, outer: Option<char>, beyond: Option<char>) -> bool {
    if !edge.is_some_and(char::is_alphanumeric) {
        return false;
    }
    match outer {
        Some(c) if c.is_alphanumeric() => true,
        Some('.' | ',' | '-') => beyond.is_some_and(|c| c.is_ascii_digit()),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn admits(fact: &ExtractedFact, source: &str) -> bool {
        Grounding::new(source).admits(fact)
    }

    fn fact(content: &str, cue: Option<&str>) -> ExtractedFact {
        ExtractedFact {
            content: content.into(),
            fact_type: "event".into(),
            subject: None,
            event_date: None,
            event_date_context: cue.map(str::to_owned),
            confidence: 0.9,
            attributed_to: Some("user".into()),
        }
    }

    fn with_confidence(mut f: ExtractedFact, confidence: f64) -> ExtractedFact {
        f.confidence = confidence;
        f
    }

    fn contents(facts: &[ExtractedFact]) -> Vec<&str> {
        facts.iter().map(|f| f.content.as_str()).collect()
    }

    const SOURCE: &str = "user: I just got back from the festival. I met Anna yesterday.";

    fn mixed() -> Vec<ExtractedFact> {
        vec![
            fact("The user visited the festival three weeks ago.", None),
            with_confidence(fact("The user likes festivals.", None), 0.4),
            fact("The user met Anna yesterday.", Some("yesterday")),
            fact("The user went to the festival.", Some("last month")),
            with_confidence(fact("The user is back home.", None), 0.5),
        ]
    }

    #[test]
    fn rejects_invented_duration_even_without_context_field() {
        assert!(!admits(
            &fact("The user visited the festival three weeks ago.", None),
            "user: I just got back from the festival."
        ));
    }

    #[test]
    fn keeps_verbatim_relative_cues_and_absolute_dates() {
        assert!(admits(
            &fact("The user attended two weeks ago.", Some("two weeks ago")),
            "user: I attended TWO   weeks ago."
        ));
        assert!(admits(
            &fact("The user attended on 2023-07-15.", None),
            "user: I attended on July 15."
        ));
    }

    #[test]
    fn rejects_unsupported_context_and_polish_duration() {
        assert!(!admits(
            &fact("The user attended.", Some("yesterday")),
            "I attended."
        ));
        assert!(!admits(
            &fact("Byłem trzy tygodnie temu.", None),
            "Właśnie wróciłem."
        ));
    }

    /// A cue must match a complete expression in the source, not a substring
    /// of a different one ("3 weeks ago" inside "13 weeks ago").
    #[test]
    fn rejects_a_different_number_that_contains_the_cue() {
        let source = "user: I visited the festival 13 weeks ago.";
        assert!(!admits(
            &fact("The user visited the festival 3 weeks ago.", None),
            source
        ));
        assert!(!admits(
            &fact("The user visited the festival.", Some("3 weeks ago")),
            source
        ));
        assert!(admits(
            &fact(
                "The user visited the festival 13 weeks ago.",
                Some("13 weeks ago")
            ),
            source
        ));
        assert!(!admits(
            &fact("The user left 2 days ago.", None),
            "user: I left 12 days ago, back in 2 days."
        ));
    }

    #[test]
    fn phrase_match_respects_alphanumeric_edges_only() {
        assert!(contains_phrase("i left yesterday.", "yesterday"));
        assert!(contains_phrase("(yesterday)", "yesterday"));
        assert!(contains_phrase("i met anna, yesterday", ", yesterday"));
        assert!(!contains_phrase("dayyesterday", "yesterday"));
        assert!(!contains_phrase("yesterdays", "yesterday"));
        assert!(contains_phrase("byłem tam 2 dni temu", "2 dni temu"));
        assert!(!contains_phrase("byłem tam 12 dni temu", "2 dni temu"));
        assert!(contains_phrase("a 12 x, then 2 x", "2 x"));
    }

    /// A decimal, a digit group or a numeric range glues the cue to the
    /// number before or after it; the same punctuation next to words does not.
    #[test]
    fn decimals_and_ranges_do_not_support_a_different_number() {
        let source = "user: it was 1.5 weeks ago, or 2-3 weeks ago, not 4,5 weeks ago.";
        for cue in ["5 weeks ago", "3 weeks ago"] {
            assert!(!contains_phrase(&normalized(source), cue), "{cue}");
            assert!(!admits(
                &fact(&format!("The user left {cue}."), None),
                source
            ));
            assert!(!admits(&fact("The user left.", Some(cue)), source));
        }
        assert!(!contains_phrase("about 2.5", "2"));
        assert!(!contains_phrase("2-3 days", "2"));
        assert!(contains_phrase("i was there - 3 weeks ago.", "3 weeks ago"));
        assert!(contains_phrase("we met.yesterday", "yesterday"));
        assert!(contains_phrase("two weeks ago-ish", "two weeks ago"));
        assert!(contains_phrase("3 weeks ago, then 2", "3 weeks ago"));
        // The same decimal or range in the fact and the source is supported.
        assert!(admits(
            &fact("The user left 1.5 weeks ago.", None),
            "user: I left 1.5 weeks ago."
        ));
        assert!(admits(
            &fact("The user left 2-3 weeks ago.", Some("2-3 weeks ago")),
            "user: I left 2-3 weeks ago."
        ));
        assert!(!admits(
            &fact("The user left 1.5 weeks ago.", None),
            "user: I left 2.5 weeks ago."
        ));
    }

    #[test]
    fn widened_cue_covers_the_whole_number() {
        let cue = |content: &str, needle: &str| {
            let start = content.find(needle).unwrap();
            widened_cue(content, start, start + needle.len()).to_string()
        };
        assert_eq!(cue("left 1.5 weeks ago", "5 weeks ago"), "1.5 weeks ago");
        assert_eq!(cue("left 2-3 weeks ago", "3 weeks ago"), "2-3 weeks ago");
        assert_eq!(cue("left ,-3 weeks ago", "3 weeks ago"), "3 weeks ago");
        assert_eq!(cue("left 3 weeks ago", "3 weeks ago"), "3 weeks ago");
        assert_eq!(cue("2-yesterday", "yesterday"), "yesterday");
    }

    #[test]
    fn pattern_compiles() {
        assert!(Regex::new(RELATIVE_CUE_PATTERN).is_ok());
        assert!(relative_cue_pattern().is_some());
    }

    #[test]
    fn off_admits_exactly_the_confidence_filter() {
        let reference: Vec<ExtractedFact> = mixed()
            .into_iter()
            .filter(|f| f.confidence >= 0.5)
            .collect();
        let (off, rejected) = admissible(mixed(), SOURCE, TemporalGrounding::Off);
        assert_eq!(contents(&off), contents(&reference));
        assert_eq!(off.len(), 4);
        assert_eq!(rejected, None);
    }

    #[test]
    fn filter_rejects_invented_cue() {
        for mode in [
            TemporalGrounding::Filter,
            TemporalGrounding::PromptAndFilter,
        ] {
            let (kept, rejected) = admissible(mixed(), SOURCE, mode);
            assert_eq!(
                contents(&kept),
                ["The user met Anna yesterday.", "The user is back home."]
            );
            // The 0.4-confidence fact is dropped by the confidence filter and
            // is not counted as a grounding rejection.
            assert_eq!(rejected, Some(2));
        }
    }

    #[test]
    fn with_notice_is_identity_for_off_and_filter() {
        for mode in [TemporalGrounding::Off, TemporalGrounding::Filter] {
            assert_eq!(with_notice(mode, "body".to_string()), "body");
        }
    }

    #[test]
    fn with_notice_prepends_for_prompt_and_filter() {
        let prompt = with_notice(TemporalGrounding::PromptAndFilter, "body".to_string());
        assert_eq!(prompt, format!("{NOTICE}\n\nbody"));
        assert!(prompt.starts_with("TEMPORAL GROUNDING: "));
    }

    #[test]
    fn serde_uses_snake_case_values() {
        for (text, mode) in [
            ("\"off\"", TemporalGrounding::Off),
            ("\"filter\"", TemporalGrounding::Filter),
            ("\"prompt_and_filter\"", TemporalGrounding::PromptAndFilter),
        ] {
            let parsed: TemporalGrounding = serde_json::from_str(text).unwrap();
            assert_eq!(parsed, mode);
            assert_eq!(serde_json::to_string(&mode).unwrap(), text);
        }
        assert!(serde_json::from_str::<TemporalGrounding>("\"bogus\"").is_err());
    }
}
