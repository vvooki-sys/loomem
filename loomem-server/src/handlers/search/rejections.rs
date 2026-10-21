//! Stage rejections — why a ranked candidate was dropped before the response.
//!
//! The post-rank filters are `Vec::retain` passes; this collector wraps them
//! so every dropped id is recorded with the filter that removed it. It is
//! inert unless the request asked for `trace: true`: a disabled log runs the
//! same predicates and allocates nothing, so the untraced path is unchanged.
use loomem_core::HybridSearchResult;

use crate::handlers::types::{Rejection, RejectionReason};

/// `evidence` stage names a dropped id is first absent from.
pub(super) const STAGE_SOFT_DELETE: &str = "after_soft_delete";
pub(super) const STAGE_FILTERS: &str = "after_filters_and_dedup";
pub(super) const STAGE_RESPONSE: &str = "response";

#[derive(Debug, Default)]
pub(super) struct RejectionLog {
    entries: Option<Vec<Rejection>>,
}

impl RejectionLog {
    pub(super) fn new(enabled: bool) -> Self {
        Self {
            entries: enabled.then(Vec::new),
        }
    }

    fn push(&mut self, chunk_id: &str, stage: &'static str, reason: RejectionReason) {
        if let Some(entries) = self.entries.as_mut() {
            entries.push(Rejection {
                chunk_id: chunk_id.to_string(),
                stage,
                reason,
            });
        }
    }

    /// `Vec::retain` that records every dropped id under one `reason`.
    pub(super) fn retain(
        &mut self,
        results: &mut Vec<HybridSearchResult>,
        stage: &'static str,
        reason: RejectionReason,
        mut keep: impl FnMut(&HybridSearchResult) -> bool,
    ) {
        self.retain_explained(results, stage, |result| (!keep(result)).then_some(reason));
    }

    /// `Vec::retain` whose predicate names the reason it drops a result
    /// (`None` keeps it).
    pub(super) fn retain_explained(
        &mut self,
        results: &mut Vec<HybridSearchResult>,
        stage: &'static str,
        mut reject: impl FnMut(&HybridSearchResult) -> Option<RejectionReason>,
    ) {
        results.retain(|result| match reject(result) {
            None => true,
            Some(reason) => {
                self.push(&result.id, stage, reason);
                false
            }
        });
    }

    /// Ids present before a filter this log cannot wrap; feed the result to
    /// [`Self::record_dropped`] afterwards. `None` while disabled.
    pub(super) fn snapshot(&self, results: &[HybridSearchResult]) -> Option<Vec<String>> {
        self.entries
            .as_ref()
            .map(|_| results.iter().map(|result| result.id.clone()).collect())
    }

    /// Record every id of `before` that `after` no longer holds.
    pub(super) fn record_dropped(
        &mut self,
        before: Option<Vec<String>>,
        after: &[HybridSearchResult],
        stage: &'static str,
        reason: RejectionReason,
    ) {
        let Some(before) = before else { return };
        for id in before
            .iter()
            .filter(|id| !after.iter().any(|result| result.id == **id))
        {
            self.push(id, stage, reason);
        }
    }

    /// Record the tail cut by `results.truncate(keep)`.
    pub(super) fn record_truncated(&mut self, results: &[HybridSearchResult], keep: usize) {
        for result in results.iter().skip(keep) {
            self.push(&result.id, STAGE_RESPONSE, RejectionReason::TruncatedTopK);
        }
    }

    /// Move entries from `other` behind this log's own, preserving stage order.
    pub(super) fn append(&mut self, other: Vec<Rejection>) {
        if let Some(entries) = self.entries.as_mut() {
            entries.extend(other);
        }
    }

    pub(super) fn into_entries(self) -> Vec<Rejection> {
        self.entries.unwrap_or_default()
    }
}

/// Which extraction-metadata filter rejects a chunk, if any. The predicate
/// is the pre-existing `fact_type` / `subject` / `min_confidence` gate, split
/// so each rejection names the filter that fired; a chunk without
/// `extraction_meta` is excluded whenever any such filter is set.
pub(super) fn extraction_filter_rejection(
    meta: Option<&loomem_core::storage::ExtractionMeta>,
    payload: &crate::handlers::types::SearchRequest,
) -> Option<RejectionReason> {
    let Some(meta) = meta else {
        return Some(RejectionReason::ExtractionMetaMissing);
    };
    let type_ok = payload.fact_type.as_ref().is_none_or(|ft| {
        // Match the built-in enum string, or an operator-configured custom
        // topic preserved in `meta.topic` (collapses to Fact in the enum, so
        // the raw key is the only way to filter it).
        fact_type_str(&meta.fact_type) == ft.as_str() || meta.topic.as_deref() == Some(ft.as_str())
    });
    if !type_ok {
        return Some(RejectionReason::FactTypeFilter);
    }
    let subject_ok = payload.subject.as_ref().is_none_or(|s| {
        meta.subject
            .as_ref()
            .is_some_and(|ms| ms.to_lowercase().contains(&s.to_lowercase()))
    });
    if !subject_ok {
        return Some(RejectionReason::SubjectFilter);
    }
    let conf_ok = payload
        .min_confidence
        .is_none_or(|mc| meta.confidence >= mc);
    (!conf_ok).then_some(RejectionReason::MinConfidence)
}

fn fact_type_str(fact_type: &loomem_core::storage::FactType) -> &'static str {
    use loomem_core::storage::FactType;
    match fact_type {
        FactType::PreferenceOrDecision => "preference_or_decision",
        FactType::ProjectState => "project_state",
        FactType::Fact => "fact",
        FactType::Event => "event",
        FactType::Experience => "experience",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(id: &str) -> HybridSearchResult {
        HybridSearchResult {
            id: id.into(),
            content: id.into(),
            user_id: String::new(),
            app_id: String::new(),
            level: 0,
            timestamp: 0,
            score: 1.0,
            bm25_score: 1.0,
            vector_score: 0.0,
            time_decay_factor: 1.0,
        }
    }

    #[test]
    fn disabled_log_filters_identically_and_records_nothing() {
        let mut log = RejectionLog::new(false);
        let mut results = vec![result("a"), result("b"), result("c")];
        log.retain(
            &mut results,
            STAGE_FILTERS,
            RejectionReason::Superseded,
            |r| r.id != "b",
        );
        assert!(log.snapshot(&results).is_none());
        log.record_dropped(None, &results, STAGE_FILTERS, RejectionReason::Superseded);
        log.record_truncated(&results, 1);
        assert_eq!(
            results.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            ["a", "c"]
        );
        assert!(log.into_entries().is_empty());
    }

    #[test]
    fn enabled_log_records_each_drop_with_its_stage_and_reason() {
        let mut log = RejectionLog::new(true);
        let mut results = vec![result("a"), result("b"), result("c"), result("d")];
        log.retain(
            &mut results,
            STAGE_SOFT_DELETE,
            RejectionReason::SoftDeleted,
            |r| r.id != "a",
        );
        let before = log.snapshot(&results);
        results.retain(|r| r.id != "b");
        log.record_dropped(before, &results, STAGE_FILTERS, RejectionReason::Superseded);
        log.retain_explained(&mut results, STAGE_FILTERS, |r| {
            (r.id == "c").then_some(RejectionReason::MinConfidence)
        });
        log.record_truncated(&results, 0);
        let entries = log.into_entries();
        let expected = [
            ("a", STAGE_SOFT_DELETE, RejectionReason::SoftDeleted),
            ("b", STAGE_FILTERS, RejectionReason::Superseded),
            ("c", STAGE_FILTERS, RejectionReason::MinConfidence),
            ("d", STAGE_RESPONSE, RejectionReason::TruncatedTopK),
        ];
        let got: Vec<_> = entries
            .iter()
            .map(|e| (e.chunk_id.as_str(), e.stage, e.reason))
            .collect();
        assert_eq!(got, expected);
        assert_eq!(
            serde_json::to_value(&entries[3]).unwrap(),
            serde_json::json!({"chunk_id": "d", "stage": "response", "reason": "truncated_top_k"})
        );
    }
}
