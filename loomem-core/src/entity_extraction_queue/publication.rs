//! Queued extraction cannot publish labels or metadata for an obsolete source.
use super::{ExtractRequest, Worker};
use crate::graph::GraphContributionContext;
use crate::llm_ner::ChunkExtraction;
use anyhow::Result;

pub(super) async fn publish(
    worker: &Worker,
    request: &ExtractRequest,
    result: &ChunkExtraction,
) -> Result<()> {
    let pii = &worker.context.pii_filter;
    let mut entities = request.dict_entities.clone();
    entities.extend(
        result
            .entities
            .iter()
            .map(|entity| (entity.name.clone(), entity.entity_type.clone())),
    );
    let relations: Vec<_> = result
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
    // This is the first write after the provider returns. It checks the generation
    // captured by enqueue and sanitizes every auxiliary string under the same lock.
    worker
        .graph
        .publish_extraction_metadata(&request.target, &entities, &relations, pii)?;
    let context = GraphContributionContext {
        stream: &request.stream,
        chunk_id: &request.chunk_id,
        expected_content: &request.content,
        pii,
    };
    for entity in &result.entities {
        worker.graph.link_entity_if_current(
            context.entity(&entity.name, &entity.entity_type, &entity.aliases),
            &request.target,
        )?;
    }
    for relation in &result.relations {
        publish_relation(worker, request, &context, relation)?;
    }
    refresh_index(worker, request).await
}

fn publish_relation(
    worker: &Worker,
    request: &ExtractRequest,
    context: &GraphContributionContext<'_>,
    relation: &crate::llm_ner::LlmRelation,
) -> Result<()> {
    let pii = &worker.context.pii_filter;
    let source = worker
        .graph
        .get_live_entity_by_name(&pii.redact_for_sink(&relation.subject), &request.stream)?;
    let target = worker
        .graph
        .get_live_entity_by_name(&pii.redact_for_sink(&relation.object), &request.stream)?;
    if let (Some(source), Some(target)) = (source, target) {
        worker.graph.link_relation_if_current(
            context.relation(&source.id, &target.id, &relation.relation),
            &request.target,
        )?;
    }
    Ok(())
}

async fn refresh_index(worker: &Worker, request: &ExtractRequest) -> Result<()> {
    let mut index = worker.tantivy.lock().await;
    worker
        .graph
        .reindex_extraction_if_current(&request.target, &mut index)
}
