//! Content-prefix dedup key for `filter_candidates`.
//!
//! The dedup pass collapses near-identical results (first 150 lowercase
//! chars, >80% shared prefix) to absorb repeated ingests of the same daily
//! log. Source fragments break that assumption: two conversations can hold
//! a byte-identical turn and each is distinct evidence (its own source, its
//! own date), so fragments of *different* sources are never collapsed into
//! each other. Every other pairing — two plain chunks, two fragments of one
//! source, a fragment against a non-fragment — keeps the pre-existing rule.
use loomem_core::RocksDbStore;

/// Dedup key of one ranked candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Fingerprint {
    /// First 150 chars of the content, lowercased.
    prefix: String,
    /// `Chunk::fragment.source_id` when the candidate is a fragment.
    fragment_source: Option<String>,
}

impl Fingerprint {
    pub(super) fn new(content: &str, fragment_source: Option<&str>) -> Self {
        Self {
            prefix: content.chars().take(150).collect::<String>().to_lowercase(),
            fragment_source: fragment_source.map(str::to_owned),
        }
    }

    /// Whether `existing` (already kept, higher-ranked) makes this candidate
    /// a duplicate: both prefixes are at least 30 bytes and share more than
    /// 80% of the shorter one — unless both are fragments of different
    /// sources, which are distinct evidence by definition.
    pub(super) fn dominated_by(&self, existing: &Self) -> bool {
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

#[cfg(test)]
mod tests {
    use super::*;

    const TURN: &str = "user: Turn 2 is about copper boilers; the lighthouse keeper logged it. \
                        More notes on copper boilers follow for turn 2.";

    #[test]
    fn plain_chunks_keep_the_prefix_rule() {
        let kept = Fingerprint::new(TURN, None);
        assert!(Fingerprint::new(TURN, None).dominated_by(&kept));
        assert!(Fingerprint::new(&TURN.to_uppercase(), None).dominated_by(&kept));
        assert!(!Fingerprint::new("short", None).dominated_by(&kept));
        assert!(
            !Fingerprint::new("assistant: Turn 3 is about something else entirely", None)
                .dominated_by(&kept)
        );
    }

    #[test]
    fn exact_eighty_percent_overlap_is_not_a_duplicate() {
        // 40 shared chars of a 50-char prefix is exactly 0.8: keep both.
        let base = "a".repeat(40);
        let kept = Fingerprint::new(&format!("{base}bbbbbbbbbb"), None);
        assert!(!Fingerprint::new(&format!("{base}cccccccccc"), None).dominated_by(&kept));
        // 41 of 50 is over the line.
        let kept = Fingerprint::new(&format!("{base}bbbbbbbbbb"), None);
        assert!(Fingerprint::new(&format!("{base}bccccccccc"), None).dominated_by(&kept));
    }

    #[test]
    fn identical_fragments_of_different_sources_both_survive() {
        let kept = Fingerprint::new(TURN, Some("src-1"));
        assert!(!Fingerprint::new(TURN, Some("src-2")).dominated_by(&kept));
    }

    #[test]
    fn fragments_of_one_source_and_mixed_pairs_still_collapse() {
        let kept = Fingerprint::new(TURN, Some("src-1"));
        assert!(Fingerprint::new(TURN, Some("src-1")).dominated_by(&kept));
        assert!(Fingerprint::new(TURN, None).dominated_by(&kept));
        let plain = Fingerprint::new(TURN, None);
        assert!(Fingerprint::new(TURN, Some("src-2")).dominated_by(&plain));
    }
}
