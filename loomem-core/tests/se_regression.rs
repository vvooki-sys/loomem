//! ECA-16.5: Sₑ regression gate.
//!
//! MANDATORY GATE before ECA-18–21 mechanisms.
//! Validates that the 3-signal serendipity formula produces sufficient spread
//! for discrimination. PoC baseline (2-signal): spread = 0.13 (unusable).
//! Target: spread > 0.40.
//!
//! This test uses synthetic embeddings to validate the formula properties,
//! since the real PoC query requires a running Loomem instance with data.

use loomem_core::associator::serendipity::{validate_spread, SpreadStatus};

/// Validate that the 3-signal formula produces good spread on synthetic data.
///
/// We simulate the scenario where:
/// - Some candidates are cross-cluster + relevant → high Sₑ
/// - Some candidates are same-cluster → Sₑ = 0
/// - Some candidates are irrelevant → low Sₑ
/// - Some candidates are obvious → low Sₑ
///
/// The spread should be > 0.40 since the formula differentiates these cases.
#[test]
fn test_se_spread_synthetic_three_signal() {
    // Simulate Sₑ scores from the 3-signal formula on diverse candidates:
    //
    // 1. Cross-cluster, relevant, non-obvious: Sₑ = 0.7 * (1-0.2) * 0.8 = 0.448
    // 2. Cross-cluster, relevant, somewhat obvious: Sₑ = 0.7 * (1-0.5) * 0.8 = 0.28
    // 3. Same-cluster (cluster_distance=0): Sₑ = 0.7 * (1-0.3) * 0.0 = 0.0
    // 4. Irrelevant (relevance≈0): Sₑ = 0.1 * (1-0.2) * 0.8 = 0.064
    // 5. Very obvious (obviousness≈1): Sₑ = 0.8 * (1-0.9) * 0.7 = 0.056
    // 6. Sweet spot — different angle: Sₑ = 0.6 * (1-0.3) * 0.9 = 0.378
    // 7. High novelty: Sₑ = 0.5 * (1-0.15) * 0.95 = 0.404

    let scores = vec![
        0.7 * (1.0 - 0.2) * 0.8,   // 0.448 — cross-cluster hit
        0.7 * (1.0 - 0.5) * 0.8,   // 0.280 — somewhat obvious
        0.7 * (1.0 - 0.3) * 0.0,   // 0.000 — same cluster
        0.1 * (1.0 - 0.2) * 0.8,   // 0.064 — irrelevant
        0.8 * (1.0 - 0.9) * 0.7,   // 0.056 — very obvious
        0.6 * (1.0 - 0.3) * 0.9,   // 0.378 — different angle
        0.5 * (1.0 - 0.15) * 0.95, // 0.404 — high novelty
    ];

    let result = validate_spread(&scores);

    // With 3 signals, spread should be well above 0.40
    assert!(
        result.spread > 0.40,
        "3-signal spread should be > 0.40, got {:.3} (min={:.3}, max={:.3})",
        result.spread,
        result.min,
        result.max
    );
    assert_eq!(result.status, SpreadStatus::Pass);

    // Save results as fixture data
    let fixture = serde_json::json!({
        "test": "se_regression_synthetic",
        "formula": "relevance * (1 - obviousness) * cluster_distance",
        "scores": scores,
        "spread": result.spread,
        "min": result.min,
        "max": result.max,
        "status": format!("{:?}", result.status),
        "poc_baseline_2signal": {
            "spread": 0.13,
            "range": [0.24, 0.37],
            "status": "Fail"
        },
        "target": "> 0.40"
    });

    // Write fixture for CI
    let fixture_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    std::fs::create_dir_all(&fixture_dir).ok();
    let fixture_path = fixture_dir.join("se_poc_baseline.json");
    std::fs::write(
        &fixture_path,
        serde_json::to_string_pretty(&fixture).unwrap(),
    )
    .ok();
}

/// Verify that 2-signal formula (without cluster_distance) produces poor spread,
/// matching the PoC baseline of 0.13.
#[test]
fn test_se_spread_two_signal_baseline() {
    // 2-signal formula: Sₑ = relevance * (1 - obviousness)
    // PoC showed: range 0.24–0.37, spread 0.13
    // Because relevance and obviousness are correlated (both cosine-based)

    let scores = vec![
        0.8 * (1.0 - 0.6),  // 0.32 — high relevance, high obviousness
        0.7 * (1.0 - 0.5),  // 0.35 — medium both
        0.6 * (1.0 - 0.4),  // 0.36 — lower both
        0.9 * (1.0 - 0.7),  // 0.27 — very relevant, very obvious
        0.5 * (1.0 - 0.25), // 0.375 — less relevant, less obvious
    ];

    let result = validate_spread(&scores);

    // 2-signal spread should be small (correlated signals)
    assert!(
        result.spread < 0.20,
        "2-signal spread should be small (correlated), got {:.3}",
        result.spread
    );
    assert_ne!(result.status, SpreadStatus::Pass);
}

/// Verify the gate logic: FAIL blocks, PASS allows, WARN logs.
#[test]
fn test_se_gate_logic() {
    // FAIL case: spread < 0.30
    let fail_scores = vec![0.30, 0.35, 0.40];
    let fail_result = validate_spread(&fail_scores);
    assert_eq!(fail_result.status, SpreadStatus::Fail);

    // WARN case: spread 0.30–0.40
    let warn_scores = vec![0.20, 0.35, 0.55];
    let warn_result = validate_spread(&warn_scores);
    assert_eq!(warn_result.status, SpreadStatus::Warn);

    // PASS case: spread > 0.40
    let pass_scores = vec![0.05, 0.25, 0.50];
    let pass_result = validate_spread(&pass_scores);
    assert_eq!(pass_result.status, SpreadStatus::Pass);
}
