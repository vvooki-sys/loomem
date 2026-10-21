use super::*;

fn seed(state: &Arc<AppState>, id: &str) -> Chunk {
    let chunk = Chunk {
        id: id.to_string(),
        content: "fixture content".to_string(),
        stream: "test".to_string(),
        level: 0,
        score: 1.0,
        timestamp: 1_700_000_000,
        consolidated: false,
        dormant: false,
        in_progress: false,
        prompt_version: None,
        source_ids: None,
        last_decay: None,
        metadata: None,
        importance: None,
        persistent: false,
        last_implicit_boost: None,
        access_count: 0,
        source: None,
        created_by: None,
        updated_at: None,
        valid_from: None,
        valid_until: None,
        is_latest: true,
        superseded_by: None,
        supersedes_id: None,
        root_memory_id: None,
        version: 1,
        memory_type: None,
        extraction_meta: None,
        deleted_at: None,
        trust_level: None,
        ingester_user_id: None,
        alpha: 1.0,
        beta: 1.0,
        harmful_count: 0,
        n_ratings: 0,
        last_rated_at: None,
        provenance_role: loomem_core::storage::ProvenanceRole::Claim,
        fragment: None,
    };
    state.store.store_chunk(&chunk).unwrap();
    chunk
}

#[test]
fn stale_and_missing_provider_results_are_not_counted_as_embedded() {
    let (_, state) = crate::tests::make_test_app();
    let chunk = seed(&state, "deleted");
    let mut counts = Counts::default();
    let targets = capture_targets(&state.store, std::slice::from_ref(&chunk), &mut counts);
    publish(&state.store, targets, vec![], &mut counts);
    assert_eq!(counts.failed, 1);
    assert_eq!(counts.embedded, 0);
    assert!(state.store.get_embedding(&chunk.id).unwrap().is_none());

    let targets = capture_targets(&state.store, std::slice::from_ref(&chunk), &mut counts);
    let mut changed = chunk;
    changed.deleted_at = Some(1);
    state.store.store_chunk(&changed).unwrap();
    publish(&state.store, targets, vec![vec![1.0, 0.0]], &mut counts);
    assert_eq!(counts.stale, 1);
    assert_eq!(counts.embedded, 0);
    assert!(capture_targets(&state.store, &[changed], &mut counts).is_empty());
    assert_eq!(counts.stale, 2);
}

#[test]
fn partial_stale_batch_and_cardinality_errors_have_exact_counts() {
    let (_, state) = crate::tests::make_test_app();
    let first = seed(&state, "first");
    let second = seed(&state, "second");
    let batch = vec![first.clone(), second.clone()];
    let mut counts = Counts::default();
    let targets = capture_targets(&state.store, &batch, &mut counts);
    publish(&state.store, targets, vec![vec![1.0, 0.0]], &mut counts);
    assert_eq!((counts.embedded, counts.stale, counts.failed), (0, 0, 2));
    assert!(state.store.get_embedding(&first.id).unwrap().is_none());
    assert!(state.store.get_embedding(&second.id).unwrap().is_none());

    let targets = capture_targets(&state.store, &batch, &mut counts);
    let mut changed = first;
    changed.content = "edited during inference".into();
    state.store.store_chunk(&changed).unwrap();
    publish(
        &state.store,
        targets,
        vec![vec![1.0, 0.0], vec![0.0, 1.0]],
        &mut counts,
    );
    assert_eq!((counts.embedded, counts.stale, counts.failed), (1, 1, 2));
    assert!(state.store.get_embedding(&changed.id).unwrap().is_none());
    assert_eq!(
        state.store.get_embedding(&second.id).unwrap(),
        Some(vec![0.0, 1.0])
    );
}
