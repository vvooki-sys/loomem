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
    /// Since the fact cap became per chunk (2026-11-22) every planned chunk
    /// is processed, so on `Ok` this equals `input_chunks_total`; the field
    /// stays for the receipt contract.
    pub input_chunks_processed: usize,
    /// Some chunk's admitted facts reached the per-chunk cap. True even when
    /// exactly the cap was returned without any discarded facts. This flag
    /// alone therefore does not imply incomplete extraction.
    pub fact_limit_reached: bool,
    /// Otherwise eligible facts discarded by the per-chunk fact cap, summed
    /// over chunks. Excludes low-confidence/ungrounded facts. Together with
    /// `fact_limit_reached`, this distinguishes an exact-cap result with no
    /// observed cap loss from cap loss.
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

    /// Admit one input chunk's (confidence-filtered) facts under the
    /// per-chunk cap and return how many were admitted. The excess is
    /// counted in `discarded_facts`; reaching the cap never stops later
    /// chunks and never issues extra calls. For a single-chunk transcript
    /// this is the former transcript-wide cap, fact for fact.
    pub(super) fn admit_chunk_facts(
        &mut self,
        facts: impl Iterator<Item = ExtractedFact>,
        max_facts: usize,
    ) -> usize {
        let mut admitted = 0usize;
        for fact in facts {
            if admitted < max_facts {
                self.facts.push(fact);
                admitted += 1;
            } else {
                self.discarded_facts += 1;
            }
        }
        if admitted >= max_facts {
            self.fact_limit_reached = true;
        }
        admitted
    }
}

#[cfg(test)]
mod tests;
