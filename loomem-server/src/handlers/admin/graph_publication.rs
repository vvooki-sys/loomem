//! Bind asynchronous graph outputs to the original pre-request canonical source.
use anyhow::Result;
use loomem_core::{
    graph::{GraphContributionContext, GraphContributionTarget, GraphStore},
    llm_ner::ChunkExtraction,
    storage::Chunk,
    PiiFilter, TantivyIndex,
};
use tokio::sync::Mutex;

pub(super) struct BatchSource {
    chunk: Chunk,
    target: GraphContributionTarget,
    dictionary: Vec<(String, String)>,
}

impl BatchSource {
    pub(super) fn capture(
        graph: &GraphStore,
        chunk: &Chunk,
        dictionary: Vec<(String, String)>,
    ) -> Result<Self> {
        let target = graph.capture_contribution_target(&chunk.id, &chunk.stream, &chunk.content)?;
        Ok(Self {
            chunk: chunk.clone(),
            target,
            dictionary,
        })
    }
}

pub(super) async fn publish_extraction(
    graph: &GraphStore,
    pii: &PiiFilter,
    index: &Mutex<TantivyIndex>,
    source: &BatchSource,
    extraction: &ChunkExtraction,
) -> Result<u32> {
    anyhow::ensure!(
        extraction.chunk_id == source.chunk.id,
        "Extraction source mismatch"
    );
    let mut merged = source.dictionary.clone();
    for entity in &extraction.entities {
        if !merged
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case(&entity.name))
        {
            merged.push((entity.name.clone(), entity.entity_type.clone()));
        }
    }
    let triples: Vec<_> = extraction
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
    graph.publish_extraction_metadata(&source.target, &merged, &triples, pii)?;
    let context = GraphContributionContext {
        stream: &source.chunk.stream,
        chunk_id: &source.chunk.id,
        expected_content: &source.chunk.content,
        pii,
    };
    let published = publish_entities(graph, &context, source, extraction, &merged)?;
    publish_relations(graph, &context, source, extraction)?;
    let mut index = index.lock().await;
    graph.reindex_extraction_if_current(&source.target, &mut index)?;
    Ok(published)
}

fn publish_entities(
    graph: &GraphStore,
    context: &GraphContributionContext<'_>,
    source: &BatchSource,
    extraction: &ChunkExtraction,
    merged: &[(String, String)],
) -> Result<u32> {
    let mut published = 0;
    for (name, kind) in merged {
        let aliases = extraction
            .entities
            .iter()
            .find(|entity| entity.name.eq_ignore_ascii_case(name))
            .map(|entity| entity.aliases.as_slice())
            .unwrap_or(&[]);
        graph.link_entity_if_current(context.entity(name, kind, aliases), &source.target)?;
        if !source
            .dictionary
            .iter()
            .any(|(dictionary, _)| dictionary.eq_ignore_ascii_case(name))
        {
            published += 1;
        }
    }
    Ok(published)
}

fn publish_relations(
    graph: &GraphStore,
    context: &GraphContributionContext<'_>,
    source: &BatchSource,
    extraction: &ChunkExtraction,
) -> Result<()> {
    for relation in &extraction.relations {
        if let (Some(left), Some(right)) = (
            graph.get_live_entity_by_name(
                &context.pii.redact_for_sink(&relation.subject),
                &source.chunk.stream,
            )?,
            graph.get_live_entity_by_name(
                &context.pii.redact_for_sink(&relation.object),
                &source.chunk.stream,
            )?,
        ) {
            graph.link_relation_if_current(
                context.relation(&left.id, &right.id, &relation.relation),
                &source.target,
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn chunk() -> Chunk {
        serde_json::from_value(json!({"id":uuid::Uuid::new_v4().to_string(),
            "content":"synthetic graph source","stream":"graph_publication",
            "level":0,"score":1.0,"timestamp":1000,"consolidated":false,
            "dormant":false,"in_progress":false}))
        .unwrap()
    }

    fn response(chunk: &Chunk) -> ChunkExtraction {
        serde_json::from_value(json!({"chunk_id":chunk.id,"entities":[
            {"name":"Synthetic Harbor","entity_type":"Place","confidence":0.9,
             "aliases":["Synthetic Port"]}],"relations":[]}))
        .unwrap()
    }

    #[tokio::test]
    async fn current_extraction_publishes_live_labels_and_completion() {
        let (_, state) = crate::tests::make_test_app();
        let chunk = chunk();
        state.store.store_chunk(&chunk).unwrap();
        let source = BatchSource::capture(&state.graph, &chunk, vec![]).unwrap();
        assert_eq!(
            publish_extraction(
                &state.graph,
                &state.pii_filter,
                &state.tantivy,
                &source,
                &response(&chunk)
            )
            .await
            .unwrap(),
            1
        );
        assert!(state
            .graph
            .get_live_entity_by_name("Synthetic Port", &chunk.stream)
            .unwrap()
            .is_some());
        assert_eq!(
            state
                .store
                .get(format!("llm_entities:{}", chunk.id).as_bytes())
                .unwrap(),
            Some(b"1".to_vec())
        );
    }

    #[tokio::test]
    async fn changed_source_rejects_pre_request_output_without_completion() {
        let (_, state) = crate::tests::make_test_app();
        let mut chunk = chunk();
        state.store.store_chunk(&chunk).unwrap();
        let source = BatchSource::capture(&state.graph, &chunk, vec![]).unwrap();
        let extraction = response(&chunk);
        chunk.content = "replacement canonical content".into();
        state.store.store_chunk(&chunk).unwrap();
        assert!(publish_extraction(
            &state.graph,
            &state.pii_filter,
            &state.tantivy,
            &source,
            &extraction
        )
        .await
        .is_err());
        assert!(state
            .graph
            .get_live_entity_by_name("Synthetic Port", &chunk.stream)
            .unwrap()
            .is_none());
        assert!(state
            .store
            .get(format!("llm_entities:{}", chunk.id).as_bytes())
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn dictionary_requires_canonical_source_and_does_not_mark_llm_complete() {
        let (_, state) = crate::tests::make_test_app();
        let chunk = chunk();
        let entities = vec![(
            "Synthetic Harbor".into(),
            loomem_core::entity_extractor::EntityType::Place,
        )];
        assert!(
            crate::handlers::ingest::graph::publish_dictionary(&state, &chunk, &entities, &[])
                .is_err()
        );
        state.store.store_chunk(&chunk).unwrap();
        assert_eq!(
            crate::handlers::ingest::graph::publish_dictionary(&state, &chunk, &entities, &[])
                .unwrap(),
            (1, 0)
        );
        assert!(state
            .graph
            .get_live_entity_by_name("Synthetic Harbor", &chunk.stream)
            .unwrap()
            .is_some());
        assert!(state
            .store
            .get(format!("llm_entities:{}", chunk.id).as_bytes())
            .unwrap()
            .is_none());
    }
}
