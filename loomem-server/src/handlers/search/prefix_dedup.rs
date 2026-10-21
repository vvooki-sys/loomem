//! Content-prefix dedup key for `filter_candidates`.
//!
//! The dedup pass collapses near-identical results (first 150 lowercase
//! chars, >80% shared prefix) to absorb repeated ingests of the same daily
//! log. Source fragments break that assumption in two ways: two
//! conversations can hold a byte-identical turn and each is distinct
//! evidence (its own source, its own date), so fragments of *different*
//! sources are never collapsed into each other; and a source's content
//! begins with its first turn, so the source and its fragment `f0` share a
//! prefix whenever that turn is long enough — a fragment is never collapsed
//! against its own source (nor the source against it), whichever ranks
//! higher. Every other pairing — two plain chunks, two fragments of one
//! source, a fragment against an unrelated non-fragment — keeps the
//! pre-existing rule.
use loomem_core::RocksDbStore;

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
}
