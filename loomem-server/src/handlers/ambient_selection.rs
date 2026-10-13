//! Keeps evidence and diagnostics attached through synthesis and packing.
use super::TOP_N_SNIPPETS;
use loomem_core::ambient::cache::truncate_annotated_to_budget;
use loomem_core::ambient::{
    compute_agreement, compute_confidence, derive_tier, provenance_class_for_level,
    recency_from_breakdown, synthesize_snippet, AmbientDebug, AmbientSnippet, Tier,
};
use loomem_core::search::{fusion::FusionResult, SignalBreakdown};
use loomem_core::HybridSearchResult;

pub(super) fn pack_selected(
    candidates: &[HybridSearchResult],
    fusion: &FusionResult,
) -> anyhow::Result<(Vec<AmbientSnippet>, AmbientDebug)> {
    let selected = score_and_synthesize(candidates, fusion);
    let kept = truncate_annotated_to_budget(selected, None)?;
    let mut snippets = Vec::with_capacity(kept.len());
    let mut debug = AmbientDebug::default();
    for (snippet, (id, breakdown)) in kept {
        snippets.push(snippet);
        debug.trace_ids.push(id);
        debug.signal_breakdown_per_snippet.push(breakdown);
    }
    Ok((snippets, debug))
}

/// Per-candidate confidence + synthesis. Drops candidates that fail the
/// §10.5.3 plain-fact constraint (synthesis returns Err) or that exceed
/// the per-snippet token cap.
fn score_and_synthesize(
    candidates: &[HybridSearchResult],
    fusion: &loomem_core::search::fusion::FusionResult,
) -> Vec<(AmbientSnippet, (String, SignalBreakdown))> {
    // §10.5.1 expects `rrf_fused ∈ [0, 1]` (per memory-routing.md "Zakres
    // faktyczny RRF na produkcji: ~0.0–1.0 po normalizacji"). The raw
    // `search::fusion::fuse` output is the un-normalized RRF sum which
    // lives in [0, 1/(k+1)] ≈ [0, 0.0164] for k=60. Normalize by the
    // top-1 score within the result set so top-1 → 1.0 and the formula's
    // 0.50 weight on rrf_fused contributes meaningfully. Final τ_high /
    // τ_medium calibration is a /103a-pre item (§10.10 #6).
    let max_score = fusion.fused_scores.iter().copied().fold(0.0_f32, f32::max);
    let mut out = Vec::new();
    for &idx in fusion.fused_order.iter().take(TOP_N_SNIPPETS) {
        let Some(cand) = candidates.get(idx) else {
            continue;
        };
        let Some(breakdown) = fusion.breakdowns.get(idx) else {
            continue;
        };
        let raw_rrf = fusion.fused_scores.get(idx).copied().unwrap_or(0.0);
        let rrf_normalized = if max_score > 0.0 {
            (raw_rrf / max_score).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let provenance = provenance_class_for_level(cand.level);
        let recency = recency_from_breakdown(breakdown);
        let agreement = compute_agreement(candidates, idx);
        let c = compute_confidence(rrf_normalized, provenance, recency, agreement);
        let tier = derive_tier(c, false);
        if tier == Tier::Low {
            continue;
        }
        if let Ok(snippet) = synthesize_snippet(cand, c, tier) {
            out.push((snippet, (cand.id.clone(), breakdown.clone())));
        }
    }
    out
}
#[cfg(test)]
mod tests {
    use super::*;
    use loomem_core::search::signals::SignalScore;

    #[test]
    fn trace_matches_packed_evidence_after_reversed_fusion_and_rejections() {
        let contents = [
            "User's excluded sixth candidate.",
            "User's second choice.",
            "User's third choice.",
            &"word ".repeat(500),
            "According to your memory, discard this.",
            "I preferred blue. I now prefer red.",
        ];
        let candidates: Vec<_> = contents
            .iter()
            .enumerate()
            .map(|(i, text)| HybridSearchResult {
                id: format!("c{i}"),
                content: text.to_string(),
                user_id: "u".to_string(),
                app_id: "a".to_string(),
                level: 0,
                timestamp: 0,
                score: 0.0,
                bm25_score: 0.0,
                vector_score: 0.0,
                time_decay_factor: 1.0,
            })
            .collect();
        let scores = [0.01, 0.02, 0.03, 0.04, 0.05, 0.06];
        let breakdowns = scores
            .iter()
            .map(|&raw_score| {
                let empty = SignalScore {
                    rank: None,
                    raw_score: 0.0,
                };
                SignalBreakdown {
                    lexical: SignalScore {
                        rank: Some(1),
                        raw_score,
                    },
                    dense: empty,
                    entity_match: empty,
                    recency: empty,
                }
            })
            .collect();
        let fusion = FusionResult {
            fused_order: vec![5, 4, 3, 2, 1, 0],
            fused_scores: scores.to_vec(),
            breakdowns,
        };
        let (snippets, debug) = pack_selected(&candidates, &fusion).unwrap();
        assert_eq!(debug.trace_ids, ["c5", "c2", "c1"]);
        assert_eq!(snippets.len(), debug.trace_ids.len());
        assert_eq!(
            snippets[0].text,
            "User preferred blue. User now prefer red."
        );
        assert_eq!(snippets[1].text, contents[2]);
        assert_eq!(snippets[2].text, contents[1]);
        let actual: Vec<_> = debug
            .signal_breakdown_per_snippet
            .iter()
            .map(|b| b.lexical.raw_score)
            .collect();
        assert_eq!(actual, vec![scores[5], scores[2], scores[1]]);
    }
}
