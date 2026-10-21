//! Caller relations preserve canonical source attribution and configured PII policy.
use super::{CallerRelation, MAX_CALLER_RELATIONS};
use crate::AppState;
use loomem_core::graph::GraphContributionContext;
use std::sync::Arc;

pub(super) fn inject_caller_relations(
    state: &Arc<AppState>,
    chunk_id: &str,
    stream: &str,
    relations: &[CallerRelation],
) {
    if relations.len() > MAX_CALLER_RELATIONS {
        tracing::warn!("Caller relation limit exceeded; excess relations skipped");
    }
    let Ok(Some(chunk)) = state.store.get_chunk(chunk_id) else {
        return;
    };
    if chunk.stream != stream {
        return;
    }
    let Ok(target) = state
        .graph
        .capture_contribution_target(chunk_id, stream, &chunk.content)
    else {
        return;
    };
    let context = GraphContributionContext {
        stream,
        chunk_id,
        expected_content: &chunk.content,
        pii: &state.pii_filter,
    };
    for relation in relations.iter().take(MAX_CALLER_RELATIONS) {
        if publish(state, &context, &target, relation).is_err() {
            tracing::warn!("Caller relation publication skipped");
        }
    }
}

fn publish(
    state: &AppState,
    context: &GraphContributionContext<'_>,
    target: &loomem_core::graph::GraphContributionTarget,
    relation: &CallerRelation,
) -> anyhow::Result<()> {
    let aliases = state.entity_extractor.get_aliases_for(&relation.subject);
    let subject = state.graph.link_entity_if_current(
        context.entity(
            &relation.subject,
            relation.subject_type.as_deref().unwrap_or("Concept"),
            &aliases,
        ),
        target,
    )?;
    let aliases = state.entity_extractor.get_aliases_for(&relation.object);
    let object = state.graph.link_entity_if_current(
        context.entity(
            &relation.object,
            relation.object_type.as_deref().unwrap_or("Concept"),
            &aliases,
        ),
        target,
    )?;
    state.graph.link_relation_if_current(
        context.relation(&subject.id, &object.id, &relation.relation),
        target,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caller_relations_require_persisted_source_and_publish_live_edges() {
        let (_, state) = crate::tests::make_test_app();
        let chunk: loomem_core::storage::Chunk = serde_json::from_value(serde_json::json!({
            "id":uuid::Uuid::new_v4().to_string(),"content":"synthetic caller source",
            "stream":"caller_graph","level":0,"score":1.0,"timestamp":1000,
            "consolidated":false,"dormant":false,"in_progress":false}))
        .unwrap();
        let relations: Vec<CallerRelation> = serde_json::from_value(serde_json::json!([{
            "subject":"Synthetic Pilot","relation":"works_at","object":"Synthetic Station"
        }]))
        .unwrap();
        inject_caller_relations(&state, &chunk.id, &chunk.stream, &relations);
        assert!(state
            .graph
            .get_live_entity_by_name("Synthetic Pilot", &chunk.stream)
            .unwrap()
            .is_none());
        state.store.store_chunk(&chunk).unwrap();
        inject_caller_relations(&state, &chunk.id, &chunk.stream, &relations);
        let entity = state
            .graph
            .get_live_entity_by_name("Synthetic Pilot", &chunk.stream)
            .unwrap()
            .unwrap();
        assert_eq!(state.graph.get_live_neighbors(&entity.id).unwrap().len(), 1);
    }
}
