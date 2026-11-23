//! Content dedup pass of `filter_candidates`, selected by
//! `[search] content_dedup`. Both modes keep the higher-ranked result and
//! record each drop as `content_prefix_dedup`.
//!
//! **`prefix`** (default): collapses near-identical results (first 150
//! lowercase chars, >80% shared prefix) to absorb repeated ingests of the
//! same daily log. Source fragments break that assumption in two ways: two
//! conversations can hold a byte-identical turn and each is distinct
//! evidence (its own source, its own date), so fragments of *different*
//! sources are never collapsed into each other; and a source's content
//! begins with its first turn, so the source and its fragment `f0` share a
//! prefix whenever that turn is long enough — a fragment is never collapsed
//! against its own source (nor the source against it), whichever ranks
//! higher. Every other pairing — two plain chunks, two fragments of one
//! source, a fragment against an unrelated non-fragment — keeps the
//! pre-existing rule.
//!
//! **`exact`**: a shared prefix is not evidence of duplication: a later
//! clause can change a preference, negate a claim, or describe another
//! event. Plain chunks only collapse when their complete content is
//! byte-identical and at least 30 bytes long. Distinct fragment ids always
//! survive, including identical text at different source offsets. Dedup
//! borrows text rather than cloning the pool.
use loomem_core::search::ContentDedup;
use loomem_core::{HybridSearchResult, RocksDbStore};
use std::collections::HashSet;

use super::rejections::RejectionLog;
use crate::handlers::types::RejectionReason;

/// Dedup key of one ranked candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Fingerprint {
    /// Chunk id of the candidate.
    id: String,
    /// First 150 chars of the content, lowercased.
    prefix: String,
    /// `Chunk::fragment.source_id` when the candidate is a fragment.
    fragment_source: Option<String>,
}

impl Fingerprint {
    pub(super) fn new(id: &str, content: &str, fragment_source: Option<&str>) -> Self {
        Self {
            id: id.to_owned(),
            prefix: content.chars().take(150).collect::<String>().to_lowercase(),
            fragment_source: fragment_source.map(str::to_owned),
        }
    }

    /// Whether `existing` (already kept, higher-ranked) makes this candidate
    /// a duplicate: both prefixes are at least 30 bytes and share more than
    /// 80% of the shorter one — unless both are fragments of different
    /// sources, or one is a fragment of the other, which are distinct
    /// evidence by definition.
    pub(super) fn dominated_by(&self, existing: &Self) -> bool {
        if self.is_source_of(existing) || existing.is_source_of(self) {
            return false;
        }
        if let (Some(mine), Some(theirs)) = (&self.fragment_source, &existing.fragment_source) {
            if mine != theirs {
                return false;
            }
        }
        let min_len = self.prefix.len().min(existing.prefix.len());
        if min_len < 30 {
            return false;
        }
        let common = self
            .prefix
            .chars()
            .zip(existing.prefix.chars())
            .take_while(|(a, b)| a == b)
            .count();
        // `common / min_len > 0.8` in exact integer arithmetic.
        common * 5 > min_len * 4
    }

    /// Whether `other` is a fragment persisted from this candidate's chunk.
    fn is_source_of(&self, other: &Self) -> bool {
        other.fragment_source.as_deref() == Some(self.id.as_str())
    }
}

/// The source id of `chunk_id` when it is persisted as a fragment; `None`
/// for any other chunk or when the chunk cannot be read.
pub(super) fn fragment_source(store: &RocksDbStore, chunk_id: &str) -> Option<String> {
    store
        .get_chunk(chunk_id)
        .ok()
        .flatten()?
        .fragment
        .map(|fragment| fragment.source_id)
}

/// The content dedup pass of `filter_candidates`: drop duplicate results in
/// rank order (the higher-ranked one is kept) under the configured `mode`,
/// recording each dropped id as `content_prefix_dedup`.
pub(super) fn apply(
    results: &mut Vec<HybridSearchResult>,
    store: &RocksDbStore,
    mode: ContentDedup,
    log: &mut RejectionLog,
    stage: &'static str,
) {
    match mode {
        ContentDedup::Prefix => prefix(results, store, log, stage),
        ContentDedup::Exact => exact(results, store, log, stage),
    }
}

/// Deduplicate near-identical results: keep highest-scored, collapse similar content.
/// Uses first 150 chars as fingerprint — catches duplicate ingest from daily logs.
/// Fragments of different sources are distinct evidence and never collapse,
/// nor does a fragment collapse against its own source.
fn prefix(
    results: &mut Vec<HybridSearchResult>,
    store: &RocksDbStore,
    log: &mut RejectionLog,
    stage: &'static str,
) {
    let mut seen_fingerprints: Vec<Fingerprint> = Vec::new();
    let reason = RejectionReason::ContentPrefixDedup;
    log.retain(results, stage, reason, |r| {
        let source = fragment_source(store, &r.id);
        let fp = Fingerprint::new(&r.id, &r.content, source.as_deref());
        let dominated = seen_fingerprints
            .iter()
            .any(|existing| fp.dominated_by(existing));
        if dominated {
            tracing::debug!("Dedup: dropped duplicate result {}", r.id);
            false
        } else {
            seen_fingerprints.push(fp);
            true
        }
    });
}

/// Exact-content dedup: drop a candidate only when `keep_candidates` marks
/// its position as a duplicate.
fn exact(
    results: &mut Vec<HybridSearchResult>,
    store: &RocksDbStore,
    log: &mut RejectionLog,
    stage: &'static str,
) {
    let mut keep = keep_candidates(results, store).into_iter();
    let reason = RejectionReason::ContentPrefixDedup;
    log.retain(results, stage, reason, |r| {
        let retain = keep.next().unwrap_or(true);
        if !retain {
            tracing::debug!("Dedup: dropped duplicate result {}", r.id);
        }
        retain
    });
}

/// Compute one retention flag per candidate, preserving the first occurrence
/// of a repeated id. Flags are positional: a set of rejected ids would also
/// remove the first occurrence. Only the small flag vector escapes this call;
/// the temporary sets borrow ids and content from `results`.
fn keep_candidates(results: &[HybridSearchResult], store: &RocksDbStore) -> Vec<bool> {
    retention_mask(results.iter().map(|result| {
        let is_fragment = store
            .get_chunk(&result.id)
            .ok()
            .flatten()
            .is_some_and(|chunk| chunk.fragment.is_some());
        (result.id.as_str(), result.content.as_str(), is_fragment)
    }))
}

fn retention_mask<'a>(records: impl Iterator<Item = (&'a str, &'a str, bool)>) -> Vec<bool> {
    let mut seen_ids = HashSet::new();
    let mut plain_contents = HashSet::new();
    records
        .map(|(id, content, is_fragment)| {
            if !seen_ids.insert(id) {
                return false;
            }
            // Keep the existing short-text exemption. Identical brief facts
            // can be separate dated events, and text alone cannot prove that
            // they are duplicates. Fragments retain every distinct occurrence.
            is_fragment || content.len() < 30 || plain_contents.insert(content)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const TURN: &str = "user: Turn 2 is about copper boilers; the lighthouse keeper logged it. \
                        More notes on copper boilers follow for turn 2.";

    fn plain(id: &str, content: &str) -> Fingerprint {
        Fingerprint::new(id, content, None)
    }

    fn fragment(id: &str, content: &str, source: &str) -> Fingerprint {
        Fingerprint::new(id, content, Some(source))
    }

    #[test]
    fn plain_chunks_keep_the_prefix_rule() {
        let kept = plain("a", TURN);
        assert!(plain("b", TURN).dominated_by(&kept));
        assert!(plain("c", &TURN.to_uppercase()).dominated_by(&kept));
        assert!(!plain("d", "short").dominated_by(&kept));
        assert!(
            !plain("e", "assistant: Turn 3 is about something else entirely").dominated_by(&kept)
        );
    }

    #[test]
    fn exact_eighty_percent_overlap_is_not_a_duplicate() {
        // 40 shared chars of a 50-char prefix is exactly 0.8: keep both.
        let base = "a".repeat(40);
        let kept = plain("a", &format!("{base}bbbbbbbbbb"));
        assert!(!plain("b", &format!("{base}cccccccccc")).dominated_by(&kept));
        // 41 of 50 is over the line.
        let kept = plain("a", &format!("{base}bbbbbbbbbb"));
        assert!(plain("b", &format!("{base}bccccccccc")).dominated_by(&kept));
    }

    #[test]
    fn identical_fragments_of_different_sources_both_survive() {
        let kept = fragment("src-1:f0", TURN, "src-1");
        assert!(!fragment("src-2:f0", TURN, "src-2").dominated_by(&kept));
    }

    #[test]
    fn a_fragment_and_its_own_source_both_survive_in_either_order() {
        // Source ranked higher: its first fragment is not a duplicate.
        let source = plain("src-1", TURN);
        assert!(!fragment("src-1:f0", TURN, "src-1").dominated_by(&source));
        // Fragment ranked higher: its own source is not a duplicate either.
        let first = fragment("src-1:f0", TURN, "src-1");
        assert!(!plain("src-1", TURN).dominated_by(&first));
        // An unrelated chunk with the same prefix still collapses against both.
        assert!(plain("other", TURN).dominated_by(&source));
        assert!(plain("other", TURN).dominated_by(&first));
    }

    #[test]
    fn fragments_of_one_source_and_mixed_pairs_still_collapse() {
        let kept = fragment("src-1:f0", TURN, "src-1");
        assert!(fragment("src-1:f1", TURN, "src-1").dominated_by(&kept));
        assert!(plain("other", TURN).dominated_by(&kept));
        let unrelated = plain("other", TURN);
        assert!(fragment("src-2:f0", TURN, "src-2").dominated_by(&unrelated));
    }

    fn keep(records: &[(&str, &str, bool)]) -> Vec<bool> {
        retention_mask(records.iter().copied())
    }

    #[test]
    fn exact_plain_duplicates_collapse() {
        assert_eq!(
            keep(&[("a", TURN, false), ("b", TURN, false)]),
            [true, false]
        );
    }

    #[test]
    fn repeated_ids_keep_only_the_first_position() {
        assert_eq!(
            keep(&[
                ("a", TURN, false),
                ("a", TURN, false),
                ("b", "short", false)
            ]),
            [true, false, true]
        );
        assert_eq!(
            keep(&[("src:f0", TURN, true), ("src:f0", TURN, true)]),
            [true, false]
        );
    }

    #[test]
    fn short_identical_events_on_different_dates_survive() {
        // Timestamp metadata differs between these records; content alone is
        // insufficient to collapse repeated brief events.
        assert_eq!(
            keep(&[
                ("2026-01-01", "Ran five miles.", false),
                ("2026-01-02", "Ran five miles.", false)
            ]),
            [true, true]
        );
        let below_threshold = "a".repeat(29);
        let at_threshold = "a".repeat(30);
        assert_eq!(
            keep(&[
                ("a", &below_threshold, false),
                ("b", &below_threshold, false)
            ]),
            [true, true]
        );
        assert_eq!(
            keep(&[("a", &at_threshold, false), ("b", &at_threshold, false)]),
            [true, false]
        );
    }

    #[test]
    fn case_can_change_meaning() {
        assert_eq!(
            keep(&[
                ("a", "The support team is preparing a report for US.", false),
                ("b", "The support team is preparing a report for us.", false)
            ]),
            [true, true]
        );
    }

    #[test]
    fn negation_after_the_old_prefix_limit_survives() {
        let prefix = "For our autumn project planning conversation, I want to record my current \
                      travel preference for the upcoming conference and the accommodation arrangements. ";
        assert!(prefix.chars().count() > 150);
        let old = format!("{prefix}I want a lively hotel.");
        let updated = format!("{prefix}I do not want a lively hotel.");
        assert_eq!(
            keep(&[("a", &old, false), ("b", &updated, false)]),
            [true, true]
        );
        assert_eq!(
            keep(&[("b", &updated, false), ("a", &old, false)]),
            [true, true]
        );
    }

    #[test]
    fn a_shared_prefix_does_not_prove_duplication() {
        let updated = format!("{TURN} The decision has changed.");
        assert_eq!(
            keep(&[
                ("a", TURN, false),
                ("b", &updated, false),
                ("c", &TURN[..80], false)
            ]),
            [true, true, true]
        );
    }

    #[test]
    fn identical_fragments_at_distinct_offsets_and_sources_survive() {
        assert_eq!(
            keep(&[
                ("src-1:f0", TURN, true),
                ("src-1:f1", TURN, true),
                ("src-2:f0", TURN, true)
            ]),
            [true, true, true]
        );
    }

    #[test]
    fn distinct_event_values_in_one_source_both_survive() {
        let prefix = "During this monthly procurement review for the same renovation project, \
                      we recorded the invoice from the same building supplier for ordered materials. ";
        let first = format!("{prefix}The amount paid was 100.");
        let second = format!("{prefix}The amount paid was 200.");
        assert_eq!(
            keep(&[("src-1:f0", &first, true), ("src-1:f1", &second, true)]),
            [true, true]
        );
    }

    #[test]
    fn fragment_and_plain_chunk_preserve_their_provenance_in_either_order() {
        for id in ["src-1", "unrelated"] {
            assert_eq!(
                keep(&[("src-1:f0", TURN, true), (id, TURN, false)]),
                [true, true]
            );
            assert_eq!(
                keep(&[(id, TURN, false), ("src-1:f0", TURN, true)]),
                [true, true]
            );
        }
    }
}

#[cfg(test)]
mod http_tests;
