//! New consolidated chunks publish only sanitized, generation-bound graph evidence.
use crate::{
    entity_extractor::{EntityExtractor, EntityType, Relation},
    graph::{GraphContributionContext, GraphStore},
    pii_filter::PiiFilter,
    storage::{Chunk, RocksDbStore},
};
use std::sync::Arc;

pub(super) struct Input<'a> {
    pub chunk: &'a Chunk,
    pub entities: &'a [(String, EntityType)],
    pub relations: &'a [Relation],
    pub extractor: Option<&'a EntityExtractor>,
    pub pii: &'a PiiFilter,
}
pub(super) fn publish(
    storage: &Arc<RocksDbStore>,
    graph: Option<&Arc<GraphStore>>,
    input: Input<'_>,
) {
    if input.entities.is_empty() && input.relations.is_empty() {
        return;
    }
    let emit_graph = graph.is_some();
    let graph = graph
        .cloned()
        .unwrap_or_else(|| Arc::new(GraphStore::new(storage.clone())));
    if let Err(error) = publish_current(&graph, &input, emit_graph) {
        tracing::warn!(%error,"Consolidated graph publication rejected");
    }
}
fn publish_current(graph: &GraphStore, input: &Input<'_>, emit_graph: bool) -> anyhow::Result<()> {
    let chunk = input.chunk;
    let target = graph.capture_contribution_target(&chunk.id, &chunk.stream, &chunk.content)?;
    let entities: Vec<_> = input
        .entities
        .iter()
        .map(|(name, kind)| (name.clone(), kind.to_string()))
        .collect();
    let relations: Vec<_> = input
        .relations
        .iter()
        .map(|relation| {
            (
                relation.subject.clone(),
                relation.relation.clone(),
                relation.object.clone(),
            )
        })
        .collect();
    graph.publish_dictionary_metadata(&target, &entities, &relations, input.pii)?;
    // Existing graph-disabled callers still keep guarded recovery metadata.
    let Some(extractor) = input.extractor.filter(|_| emit_graph) else {
        return Ok(());
    };
    let context = GraphContributionContext {
        stream: &chunk.stream,
        chunk_id: &chunk.id,
        expected_content: &chunk.content,
        pii: input.pii,
    };
    for (name, kind) in input.entities {
        let aliases = extractor.get_aliases_for(name);
        graph.link_entity_if_current(context.entity(name, &kind.to_string(), &aliases), &target)?;
    }
    for relation in input.relations {
        let source = graph.get_live_entity_by_name(
            &input.pii.redact_for_sink(&relation.subject),
            &chunk.stream,
        )?;
        let destination = graph
            .get_live_entity_by_name(&input.pii.redact_for_sink(&relation.object), &chunk.stream)?;
        if let (Some(source), Some(destination)) = (source, destination) {
            graph.link_relation_if_current(
                context.relation(&source.id, &destination.id, &relation.relation),
                &target,
            )?;
        }
    }
    Ok(())
}
