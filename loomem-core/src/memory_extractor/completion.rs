//! Distinguish top-level input coverage from extraction calls and fact caps.

use super::chunking::ChunkRun;
use super::{ChunkFailure, ExtractedFact};

/// Result of an (at least partially) successful extraction run.
#[derive(Debug, Default)]
pub struct ExtractionOutcome {
    pub facts: Vec<ExtractedFact>,
    /// Final per-chunk failures when other calls succeeded. All-calls-failed
    /// remains [`super::ExtractionError::AllChunksFailed`] instead.
    pub failures: Vec<ChunkFailure>,
    /// Extraction calls issued, including split retries.
    pub chunks_attempted: usize,
    /// Calls that returned a complete, parseable reply.
    pub chunks_completed: usize,
    /// Calls whose completion was truncated (`finish_reason: length`),
    /// whether or not a split retry recovered them.
    pub chunks_truncated: usize,
    /// Original transcript chunks planned before extraction. Split retries
    /// do not increase this count.
    pub input_chunks_total: usize,
    /// Original transcript chunks attempted, including failed chunks. One
    /// original plus its split retries counts once, regardless of success.
    /// A count below `input_chunks_total` means the fact cap stopped input
    /// processing early; it does not estimate facts in the unprocessed input.
    pub input_chunks_processed: usize,
    /// The admitted facts reached the configured cap. True even when exactly
    /// the cap was returned from the final input chunk without any discarded
    /// facts. This flag alone therefore does not imply incomplete extraction.
    pub fact_limit_reached: bool,
    /// Otherwise eligible facts discarded by the transcript-wide fact cap.
    /// Excludes low-confidence/ungrounded facts and unknown facts from input
    /// chunks never processed. Together with `input_chunks_processed`, this
    /// distinguishes an exact-cap result with no observed cap loss from cap loss.
    pub discarded_facts: usize,
}

impl ExtractionOutcome {
    pub(super) fn for_input_chunks(total: usize) -> Self {
        Self {
            input_chunks_total: total,
            ..Self::default()
        }
    }

    pub(super) fn record_input_chunk(&mut self, run: &ChunkRun) {
        self.input_chunks_processed += 1;
        self.chunks_attempted += run.calls_attempted;
        self.chunks_completed += run.calls_completed;
        self.chunks_truncated += run.calls_truncated;
    }

    /// Apply the existing cap and stop condition without issuing extra calls.
    pub(super) fn apply_fact_limit(&mut self, max_facts: usize) -> bool {
        if self.facts.len() < max_facts {
            return false;
        }
        self.fact_limit_reached = true;
        self.discarded_facts += self.facts.len() - max_facts;
        self.facts.truncate(max_facts);
        true
    }
}

#[cfg(test)]
mod tests;
