//! Dictionary graph publication follows canonical chunk persistence.
use crate::AppState;
use anyhow::Result;
use loomem_core::{
    entity_extractor::{EntityType, Relation},
    graph::GraphContributionContext,
    storage::Chunk,
};

pub(crate) fn publish_dictionary(
    state: &AppState,
    chunk: &Chunk,
    entities: &[(String, EntityType)],
    relations: &[Relation],
) -> Result<(usize, usize)> {
    let target =
        state
            .graph
            .capture_contribution_target(&chunk.id, &chunk.stream, &chunk.content)?;
    let typed: Vec<_> = entities
        .iter()
        .map(|(name, kind)| (name.clone(), kind.to_string()))
        .collect();
    let triples: Vec<_> = relations
        .iter()
        .map(|relation| {
            (
                relation.subject.clone(),
                relation.relation.clone(),
                relation.object.clone(),
            )
        })
        .collect();
    state
        .graph
        .publish_dictionary_metadata(&target, &typed, &triples, &state.pii_filter)?;
    let context = GraphContributionContext {
        stream: &chunk.stream,
        chunk_id: &chunk.id,
        expected_content: &chunk.content,
        pii: &state.pii_filter,
    };
    let mut published_entities = 0;
    let mut published_edges = 0;
    for (name, kind) in entities {
        let aliases = state.entity_extractor.get_aliases_for(name);
        state
            .graph
            .link_entity_if_current(context.entity(name, &kind.to_string(), &aliases), &target)?;
        published_entities += 1;
    }
    for relation in relations {
        if let (Some(source), Some(destination)) = (
            state.graph.get_live_entity_by_name(
                &state.pii_filter.redact_for_sink(&relation.subject),
                &chunk.stream,
            )?,
            state.graph.get_live_entity_by_name(
                &state.pii_filter.redact_for_sink(&relation.object),
                &chunk.stream,
            )?,
        ) {
            state.graph.link_relation_if_current(
                context.relation(&source.id, &destination.id, &relation.relation),
                &target,
            )?;
            published_edges += 1;
        }
    }
    Ok((published_entities, published_edges))
}
