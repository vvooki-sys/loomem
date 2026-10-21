use super::*;

/// Walk `dream_resp.contradictions` through the trust guard for one subject.
///
/// Extracted from `apply_dream_response_for_subject` to reduce orchestrator
/// NLOC and CC (cycle /46 fixup).
pub(super) fn resolve_dream_contradictions(
    store: &RocksDbStore,
    ctx: &DreamApplyContext<'_>,
    new_id: &str,
    contradictions: &[LlmContradiction],
) -> Result<usize> {
    let mut resolved = 0;
    for contradiction in contradictions
        .iter()
        .filter(|c| ctx.chunks.iter().any(|s| s.id == c.old_uuid))
    {
        let Ok(Some(old_chunk)) = store.get_chunk(&contradiction.old_uuid) else {
            continue;
        };
        if !eligible_source(&old_chunk, ctx.stream) {
            continue;
        }
        // Cycle /40a: apply valid_until + superseded_by in the same
        // store_chunk call via extra_old_mutator to close the R-M-W window.
        let applied = crate::contradiction::try_supersede_with_guard(
            store,
            &old_chunk,
            new_id,
            Some(derived_trust(ctx.chunks.iter())),
            "dream",
            Some(&|c: &mut Chunk| {
                c.valid_until = Some(now_secs());
                if let Some(ref mut meta) = c.extraction_meta {
                    meta.superseded_by = Some("dream-consolidated".to_string());
                }
            }),
        )?;
        if applied {
            resolved += 1;
            debug!("Dream: superseded chunk {}", contradiction.old_uuid);
        } else {
            debug!(
                "Dream: supersede blocked by trust guard for {}",
                contradiction.old_uuid
            );
        }
    }
    Ok(resolved)
}

fn eligible_source(chunk: &Chunk, stream: &str) -> bool {
    chunk.is_latest && chunk.deleted_at.is_none() && chunk.stream == stream
}
