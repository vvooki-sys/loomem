//! ECA Integration Tests (ECA-32)
//!
//! Validates that all ECA components work together.
//! Focuses on data structures, pure functions, and business logic — no RocksDB required.

use loomem_core::advisor::{AdvisoryItem, AdvisoryPriority, AdvisoryType, OscillationAlert};
use loomem_core::associator::clustering::cosine_similarity;
use loomem_core::associator::dream::LatentAssociation;
use loomem_core::associator::serendipity::{validate_spread, SpreadStatus};
use loomem_core::associator::AssociationHealth;
use loomem_core::cost_tracker::CostBudgetStatus;
use loomem_core::event_log::{EventEntry, MemoryEvent};

// ---- Test 1: Full ECA event pipeline ----

#[test]
fn test_full_eca_event_pipeline() {
    // 1. Create all event types and verify serialization roundtrip
    let events: Vec<MemoryEvent> = vec![
        MemoryEvent::Search {
            query: "test query".into(),
            stream_id: "100".into(),
            top_scores: vec![0.9, 0.8, 0.7],
            latency_ms: 42,
            result_count: 3,
        },
        MemoryEvent::Store {
            content_len: 256,
            chunk_count: 2,
            stream_id: "100".into(),
            source: "api".into(),
        },
        MemoryEvent::Consolidation {
            input_count: 5,
            output_count: 2,
            dropped_ids: vec!["old-1".into(), "old-2".into()],
            cost_usd: 0.001,
        },
        MemoryEvent::CostEvent {
            tokens: 1500,
            model: "text-embedding-3-small".into(),
            operation: "embed".into(),
        },
        MemoryEvent::Association {
            query: "rust async".into(),
            mechanisms_used: vec!["graph_walk".into(), "temporal".into()],
            scores: vec![0.45, 0.38],
            surfaced_count: 2,
        },
        MemoryEvent::DreamCycle {
            stream_id: "100".into(),
            discoveries: 3,
            evictions: 1,
            latent_total: 42,
        },
    ];

    for (i, event) in events.into_iter().enumerate() {
        let entry = EventEntry {
            timestamp: 1700000000 + i as u64,
            event,
        };
        // Serialize
        let json = serde_json::to_string(&entry)
            .unwrap_or_else(|e| panic!("Failed to serialize event {}: {}", i, e));
        // Deserialize
        let parsed: EventEntry = serde_json::from_str(&json)
            .unwrap_or_else(|e| panic!("Failed to deserialize event {}: {}", i, e));
        assert_eq!(parsed.timestamp, entry.timestamp);
    }
}

// ---- Test 2: Clustering and serendipity integration ----

#[test]
fn test_clustering_and_serendipity_integration() {
    // Create synthetic embeddings in 3 clear clusters
    // Cluster A: near [1, 0, 0]
    let cluster_a = [
        vec![1.0, 0.0, 0.0],
        vec![0.95, 0.05, 0.0],
        vec![0.9, 0.1, 0.0],
    ];
    // Cluster B: near [0, 1, 0]
    let cluster_b = [
        vec![0.0, 1.0, 0.0],
        vec![0.05, 0.95, 0.0],
        vec![0.1, 0.9, 0.0],
    ];
    // Cluster C: near [0, 0, 1]
    let cluster_c = [
        vec![0.0, 0.0, 1.0],
        vec![0.0, 0.05, 0.95],
        vec![0.05, 0.0, 0.95],
    ];

    // Verify within-cluster similarity is high
    let intra_sim_a = cosine_similarity(&cluster_a[0], &cluster_a[1]);
    let intra_sim_b = cosine_similarity(&cluster_b[0], &cluster_b[1]);
    assert!(
        intra_sim_a > 0.9,
        "Intra-cluster A similarity should be high, got {:.3}",
        intra_sim_a
    );
    assert!(
        intra_sim_b > 0.9,
        "Intra-cluster B similarity should be high, got {:.3}",
        intra_sim_b
    );

    // Verify cross-cluster similarity is low
    let cross_sim_ab = cosine_similarity(&cluster_a[0], &cluster_b[0]);
    let cross_sim_ac = cosine_similarity(&cluster_a[0], &cluster_c[0]);
    assert!(
        cross_sim_ab < 0.1,
        "Cross-cluster A-B similarity should be low, got {:.3}",
        cross_sim_ab
    );
    assert!(
        cross_sim_ac < 0.1,
        "Cross-cluster A-C similarity should be low, got {:.3}",
        cross_sim_ac
    );

    // Compute Sₑ-like scores for cross-cluster vs same-cluster candidates
    // Using the formula: relevance * (1 - obviousness) * cluster_distance
    // Cross-cluster: high cluster_distance, moderate relevance, low obviousness
    let cross_score = 0.6 * (1.0 - 0.2) * 0.9; // = 0.432
                                               // Same-cluster: cluster_distance = 0
    let same_score = 0.7 * (1.0 - 0.3) * 0.0; // = 0.0

    assert!(
        cross_score > same_score,
        "Cross-cluster score should be higher"
    );
    assert!(
        cross_score > 0.40,
        "Cross-cluster Sₑ should be discriminative"
    );

    // Validate spread with diverse scores
    let scores = vec![
        same_score,              // 0.0   — same cluster
        0.1 * (1.0 - 0.2) * 0.8, // 0.064 — irrelevant
        0.8 * (1.0 - 0.9) * 0.7, // 0.056 — obvious
        0.6 * (1.0 - 0.3) * 0.9, // 0.378 — different angle
        cross_score,             // 0.432 — cross-cluster hit
    ];
    let result = validate_spread(&scores);
    assert_eq!(
        result.status,
        SpreadStatus::Pass,
        "Spread should pass with diverse scores, got {:.3}",
        result.spread
    );
    assert!(
        result.spread > 0.40,
        "Spread should exceed 0.40, got {:.3}",
        result.spread
    );
}

// ---- Test 3: Advisory detection types ----

#[test]
fn test_advisory_detection_types() {
    let now = 1700000000u64;

    // Create mock AdvisoryItems of each type
    let items = vec![
        AdvisoryItem {
            id: "adv-1".into(),
            advisory_type: AdvisoryType::RepeatedQuery,
            message: "Query 'rust async' searched 5 times".into(),
            suggested_action: Some("Store a pre-computed answer".into()),
            affected_chunk_ids: vec![],
            priority: AdvisoryPriority::Medium,
            created_at: now,
        },
        AdvisoryItem {
            id: "adv-2".into(),
            advisory_type: AdvisoryType::SearchIgnored,
            message: "10 searches with no feedback".into(),
            suggested_action: None,
            affected_chunk_ids: vec![],
            priority: AdvisoryPriority::Low,
            created_at: now,
        },
        AdvisoryItem {
            id: "adv-3".into(),
            advisory_type: AdvisoryType::StaleFact,
            message: "3 chunks stale for 30 days".into(),
            suggested_action: Some("Review and update".into()),
            affected_chunk_ids: vec!["chunk-1".into(), "chunk-2".into()],
            priority: AdvisoryPriority::Medium,
            created_at: now,
        },
        AdvisoryItem {
            id: "adv-4".into(),
            advisory_type: AdvisoryType::Contradiction,
            message: "Conflicting facts about project X".into(),
            suggested_action: Some("Reconcile".into()),
            affected_chunk_ids: vec!["chunk-a".into(), "chunk-b".into()],
            priority: AdvisoryPriority::High,
            created_at: now,
        },
        AdvisoryItem {
            id: "adv-5".into(),
            advisory_type: AdvisoryType::GapFilled,
            message: "2 gap-fills detected".into(),
            suggested_action: None,
            affected_chunk_ids: vec![],
            priority: AdvisoryPriority::Low,
            created_at: now,
        },
    ];

    // Verify serialization roundtrip for each
    for item in &items {
        let json = serde_json::to_string(item).expect("serialize advisory");
        let parsed: AdvisoryItem = serde_json::from_str(&json).expect("deserialize advisory");
        assert_eq!(parsed.id, item.id);
        assert_eq!(parsed.priority, item.priority);
    }

    // Verify priority ordering (High > Medium > Low)
    assert!(AdvisoryPriority::High > AdvisoryPriority::Medium);
    assert!(AdvisoryPriority::Medium > AdvisoryPriority::Low);
    assert!(AdvisoryPriority::High > AdvisoryPriority::Low);

    // Sort by priority descending — High should come first
    let mut sorted = items.clone();
    sorted.sort_by(|a, b| b.priority.cmp(&a.priority));
    assert_eq!(sorted[0].id, "adv-4", "High priority should sort first");
}

// ---- Test 4: Circuit breaker — AssociationHealth ----

#[test]
fn test_circuit_breaker_association_health() {
    // Test Healthy variant
    let healthy = AssociationHealth::Healthy;
    let json = serde_json::to_string(&healthy).unwrap();
    let parsed: AssociationHealth = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, AssociationHealth::Healthy);

    // Test Degraded variant
    let degraded = AssociationHealth::Degraded {
        mean_se: 0.28,
        days: 3,
    };
    let json = serde_json::to_string(&degraded).unwrap();
    let parsed: AssociationHealth = serde_json::from_str(&json).unwrap();
    match parsed {
        AssociationHealth::Degraded { mean_se, days } => {
            assert!((mean_se - 0.28).abs() < 0.001);
            assert_eq!(days, 3);
        }
        _ => panic!("Expected Degraded variant"),
    }

    // Test Disabled variant
    let disabled = AssociationHealth::Disabled {
        reason: "Mean Se below 0.35 for 7 days".into(),
    };
    let json = serde_json::to_string(&disabled).unwrap();
    let parsed: AssociationHealth = serde_json::from_str(&json).unwrap();
    match parsed {
        AssociationHealth::Disabled { reason } => {
            assert!(reason.contains("0.35"));
        }
        _ => panic!("Expected Disabled variant"),
    }

    // Verify that Disabled blocks computation (logic check)
    let should_run = !matches!(disabled, AssociationHealth::Disabled { .. });
    assert!(!should_run, "Disabled health should block associations");

    let should_run_healthy = !matches!(healthy, AssociationHealth::Disabled { .. });
    assert!(should_run_healthy, "Healthy should allow associations");

    let should_run_degraded = !matches!(degraded, AssociationHealth::Disabled { .. });
    assert!(
        should_run_degraded,
        "Degraded should still allow associations"
    );
}

// ---- Test 5: Cost budget tiers ----

#[test]
fn test_cost_budget_tiers() {
    // Test all enum variants exist and serialize
    let statuses = vec![
        CostBudgetStatus::Normal,
        CostBudgetStatus::RerankerDisabled,
        CostBudgetStatus::AssociationsDisabled,
        CostBudgetStatus::LogOnly,
    ];

    for status in &statuses {
        let json = serde_json::to_string(status).unwrap();
        let parsed: CostBudgetStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(&parsed, status);
    }

    // Verify tier progression: each tier is more restrictive
    assert!(CostBudgetStatus::Normal.allow_reranker());
    assert!(CostBudgetStatus::Normal.allow_associations());
    assert!(CostBudgetStatus::Normal.allow_llm_calls());

    assert!(!CostBudgetStatus::RerankerDisabled.allow_reranker());
    assert!(CostBudgetStatus::RerankerDisabled.allow_associations());
    assert!(CostBudgetStatus::RerankerDisabled.allow_llm_calls());

    assert!(!CostBudgetStatus::AssociationsDisabled.allow_reranker());
    assert!(!CostBudgetStatus::AssociationsDisabled.allow_associations());
    assert!(CostBudgetStatus::AssociationsDisabled.allow_llm_calls());

    assert!(!CostBudgetStatus::LogOnly.allow_reranker());
    assert!(!CostBudgetStatus::LogOnly.allow_associations());
    assert!(!CostBudgetStatus::LogOnly.allow_llm_calls());

    // Verify descriptions are non-empty
    for status in &statuses {
        assert!(!status.description().is_empty());
    }
}

// ---- Test 6: Oscillation alert ----

#[test]
fn test_oscillation_alert_serialization() {
    let alert = OscillationAlert {
        direction_changes: 4,
        recommended_freeze_value: -0.0125,
        window_days: 21,
    };

    let json = serde_json::to_string(&alert).unwrap();
    let parsed: OscillationAlert = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.direction_changes, 4);
    assert_eq!(parsed.window_days, 21);
    assert!((parsed.recommended_freeze_value - (-0.0125)).abs() < 1e-6);
}

#[test]
fn test_oscillation_direction_change_logic() {
    // Simulate the oscillation detection logic (pure, no RocksDB)
    // Adjustments: +0.05, -0.05, +0.05, -0.05 = 3 direction changes
    let adjustments: Vec<f64> = vec![0.05, -0.05, 0.05, -0.05];

    let mut direction_changes = 0usize;
    for i in 1..adjustments.len() {
        let prev_positive = adjustments[i - 1] >= 0.0;
        let curr_positive = adjustments[i] >= 0.0;
        if prev_positive != curr_positive {
            direction_changes += 1;
        }
    }

    assert_eq!(direction_changes, 3, "Should detect 3 direction changes");
    assert!(direction_changes >= 3, "Should trigger oscillation freeze");

    // Midpoint of oscillating adjustments should be near 0
    let sum: f64 = adjustments.iter().sum();
    let midpoint = sum / adjustments.len() as f64;
    assert!(
        midpoint.abs() < 0.01,
        "Midpoint of oscillation should be near 0, got {}",
        midpoint
    );
}

// ---- Test 7: Dream latent association lifecycle ----

#[test]
fn test_dream_latent_association_lifecycle() {
    // 1. Create LatentAssociation
    let latent = LatentAssociation {
        id: "dream-1700000000-0".into(),
        source_chunk_id: "chunk-src".into(),
        target_chunk_id: "chunk-tgt".into(),
        target_content: "An unexpected connection between rust and music theory".into(),
        score: 0.45,
        mechanism: "dream_discovery".into(),
        discovered_at: 1700000000,
        promoted: false,
        promoted_count: 0,
    };

    // 2. Verify serialization
    let json = serde_json::to_string(&latent).unwrap();
    let parsed: LatentAssociation = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.id, "dream-1700000000-0");
    assert_eq!(parsed.score, 0.45);
    assert!(!parsed.promoted);
    assert_eq!(parsed.promoted_count, 0);

    // 3. Simulate promotion
    let mut promoted = parsed;
    promoted.promoted = true;
    promoted.promoted_count += 1;
    assert!(promoted.promoted);
    assert_eq!(promoted.promoted_count, 1);

    // 4. Verify FIFO ordering by discovered_at
    let latents = vec![
        LatentAssociation {
            id: "dream-early".into(),
            source_chunk_id: "s1".into(),
            target_chunk_id: "t1".into(),
            target_content: "early".into(),
            score: 0.5,
            mechanism: "dream".into(),
            discovered_at: 1000,
            promoted: false,
            promoted_count: 0,
        },
        LatentAssociation {
            id: "dream-late".into(),
            source_chunk_id: "s2".into(),
            target_chunk_id: "t2".into(),
            target_content: "late".into(),
            score: 0.4,
            mechanism: "dream".into(),
            discovered_at: 2000,
            promoted: false,
            promoted_count: 0,
        },
        LatentAssociation {
            id: "dream-mid".into(),
            source_chunk_id: "s3".into(),
            target_chunk_id: "t3".into(),
            target_content: "mid".into(),
            score: 0.6,
            mechanism: "dream".into(),
            discovered_at: 1500,
            promoted: false,
            promoted_count: 0,
        },
    ];

    // Sort by discovered_at ascending (FIFO — oldest first for eviction)
    let mut sorted = latents.clone();
    sorted.sort_by_key(|l| l.discovered_at);
    assert_eq!(sorted[0].id, "dream-early");
    assert_eq!(sorted[1].id, "dream-mid");
    assert_eq!(sorted[2].id, "dream-late");

    // FIFO eviction: if max=2, evict oldest
    let max_count = 2;
    if sorted.len() > max_count {
        let evicted: Vec<_> = sorted.drain(..sorted.len() - max_count).collect();
        assert_eq!(evicted.len(), 1);
        assert_eq!(
            evicted[0].id, "dream-early",
            "Oldest should be evicted first"
        );
    }
    assert_eq!(sorted.len(), max_count);
}

// ---- Test 8: Cross-component integration ----

#[test]
fn test_eca_components_interplay() {
    // Verify that cost budget tiers correctly gate association health checks
    // Scenario: budget is at AssociationsDisabled level
    let budget = CostBudgetStatus::AssociationsDisabled;
    let health = AssociationHealth::Healthy;

    // Even though health is fine, cost budget should block associations
    let should_run =
        budget.allow_associations() && !matches!(health, AssociationHealth::Disabled { .. });
    assert!(
        !should_run,
        "AssociationsDisabled budget should block even healthy associations"
    );

    // Scenario: budget is Normal but health is Disabled
    let budget = CostBudgetStatus::Normal;
    let health = AssociationHealth::Disabled {
        reason: "low quality".into(),
    };
    let should_run =
        budget.allow_associations() && !matches!(health, AssociationHealth::Disabled { .. });
    assert!(
        !should_run,
        "Disabled health should block even with Normal budget"
    );

    // Scenario: both OK
    let budget = CostBudgetStatus::Normal;
    let health = AssociationHealth::Healthy;
    let should_run =
        budget.allow_associations() && !matches!(health, AssociationHealth::Disabled { .. });
    assert!(should_run, "Should run when both health and budget are OK");

    // Scenario: Degraded health + RerankerDisabled budget = associations still run
    let budget = CostBudgetStatus::RerankerDisabled;
    let health = AssociationHealth::Degraded {
        mean_se: 0.30,
        days: 2,
    };
    let should_run =
        budget.allow_associations() && !matches!(health, AssociationHealth::Disabled { .. });
    assert!(
        should_run,
        "Degraded health + RerankerDisabled budget should still allow associations"
    );
}
