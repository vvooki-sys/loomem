use super::{ingest_summary, ExtractionOutcome};

fn outcome(processed: usize, total: usize, discarded: usize) -> ExtractionOutcome {
    ExtractionOutcome {
        input_chunks_processed: processed,
        input_chunks_total: total,
        fact_limit_reached: true,
        discarded_facts: discarded,
        ..ExtractionOutcome::default()
    }
}

#[test]
fn cap_reports_unprocessed_input_without_guessing_omitted_facts() {
    let summary = ingest_summary(&outcome(1, 3, 0), 2, 0);
    assert!(summary.contains("processed 1/3 input chunks; discarded 0 generated facts"));
    assert!(summary.contains("Warning: extraction was capped"));
}

#[test]
fn cap_reports_discarded_facts_after_all_input_was_processed() {
    let summary = ingest_summary(&outcome(1, 1, 2), 2, 0);
    assert!(summary.contains("discarded 2 generated facts"));
    assert!(summary.contains("Warning: extraction was capped"));
}

#[test]
fn exact_final_cap_does_not_claim_observed_truncation() {
    let summary = ingest_summary(&outcome(1, 1, 0), 2, 0);
    assert!(summary.contains("Fact limit reached"));
    assert!(!summary.contains("extraction was capped"));
}

#[test]
fn uncapped_summary_remains_compatible() {
    assert_eq!(
        ingest_summary(&ExtractionOutcome::default(), 0, 0),
        "Extracted 0 facts from conversation, stored 0, skipped 0."
    );
}
