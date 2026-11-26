//! `[search] vector_fill_missing`: the fill helper, and fusion with the flag
//! off (byte-identical to the pre-fill code) and on.
use super::*;
use crate::hybrid_search::tests::{base_chunk, test_config};
use crate::hybrid_search::{
    HybridSearchEngine, HybridSearchResult, SearchCandidateBudget, VectorSearchOptions,
};
use crate::storage::RocksDbStore;

fn scale(max_vector: f32, floor: f64) -> LaneScale {
    LaneScale {
        max_vector,
        floor,
        lane_empty: false,
    }
}

fn hit(id: &str, score: f32) -> SearchResult {
    SearchResult {
        id: id.into(),
        content: format!("lexical {id}"),
        user_id: "default".into(),
        app_id: "default".into(),
        stream: "s".into(),
        level: 0,
        timestamp: 0,
        score,
    }
}

#[test]
fn filled_score_is_cosine_over_lane_max() {
    let score = filled_score(0.78, &scale(0.88, 0.0));
    assert!((score - 0.78 / 0.88).abs() < 1e-6);
}

#[test]
fn filled_score_respects_the_floor_like_lane_hits() {
    assert_eq!(filled_score(0.74, &scale(0.9, 0.75)), 0.0);
    let at_floor = filled_score(0.75, &scale(0.9, 0.75));
    assert!((at_floor - 0.75 / 0.9).abs() < 1e-6);
}

#[test]
fn filled_score_is_clamped_to_one_and_zero() {
    assert_eq!(filled_score(0.95, &scale(0.9, 0.0)), 1.0);
    assert_eq!(filled_score(-0.2, &scale(0.9, 0.0)), 0.0);
    assert_eq!(filled_score(f32::NAN, &scale(0.9, 0.0)), 0.0);
}

#[test]
fn empty_lane_or_non_positive_max_leaves_zero() {
    let empty = LaneScale {
        max_vector: 1.0,
        floor: 0.0,
        lane_empty: true,
    };
    assert_eq!(filled_score(0.8, &empty), 0.0);
    assert_eq!(filled_score(0.8, &scale(0.0, 0.0)), 0.0);
    assert_eq!(filled_score(0.8, &scale(-0.5, 0.0)), 0.0);
}

#[test]
fn missing_cosines_covers_only_bm25_ids_outside_the_lane_with_an_embedding() {
    let bm25 = [hit("lane", 3.0), hit("miss", 2.0), hit("noemb", 1.0)];
    let lane = vec![("lane".to_string(), 0.9_f32), ("dense".to_string(), 0.8)];
    let embeddings = vec![
        ("lane".to_string(), vec![1.0, 0.0]),
        ("dense".to_string(), vec![1.0, 0.1]),
        ("miss".to_string(), vec![1.0, 1.0]),
    ];
    let fill = missing_cosines(&bm25, &lane, &embeddings, &[1.0, 0.0]);
    assert_eq!(fill.len(), 1, "{fill:?}");
    let expected = cosine_similarity(&[1.0, 0.0], &[1.0, 1.0]);
    assert_eq!(fill.get("miss").copied(), Some(expected));
    assert!(missing_cosines(&bm25, &lane, &embeddings, &[]).is_empty());
}

#[test]
fn apply_never_overrides_lane_hits_or_adds_non_bm25_ids() {
    let mut bm25_map = Bm25Map::new();
    for id in ["lane", "miss", "low"] {
        bm25_map.insert(id.into(), (1.0, hit(id, 1.0)));
    }
    let mut vector_map = HashMap::from([("lane".to_string(), 1.0_f32)]);
    let fill = HashMap::from([
        ("lane".to_string(), 0.1_f32),
        ("miss".to_string(), 0.45),
        ("low".to_string(), 0.2),
        ("dense-only".to_string(), 0.5),
    ]);
    let filled = apply(&mut vector_map, &fill, &bm25_map, &scale(0.5, 0.3));
    assert_eq!(filled, 1);
    assert_eq!(vector_map.get("lane").copied(), Some(1.0));
    assert!((vector_map["miss"] - 0.9).abs() < 1e-6);
    assert!(!vector_map.contains_key("low"), "below floor stays 0");
    assert!(!vector_map.contains_key("dense-only"));
}

/// (id, score bits, bm25_score bits, vector_score bits).
type Row = (&'static str, u64, u32, u32);

fn rows(results: &[HybridSearchResult]) -> Vec<(String, u64, u32, u32)> {
    results
        .iter()
        .map(|r| {
            (
                r.id.clone(),
                r.score.to_bits(),
                r.bm25_score.to_bits(),
                r.vector_score.to_bits(),
            )
        })
        .collect()
}

fn golden(expected: &[Row]) -> Vec<(String, u64, u32, u32)> {
    expected
        .iter()
        .map(|(id, s, b, v)| ((*id).to_string(), *s, *b, *v))
        .collect()
}

struct Fixture {
    _dir: tempfile::TempDir,
    store: RocksDbStore,
    embeddings: Vec<(String, Vec<f32>)>,
}

/// 40 chunks `v00`..`v39` with vectors `[1, i/20]` (cosine to `[1, 0]`
/// falls with `i`); the default budget's vector lane keeps `v00`..`v19`.
fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let store = RocksDbStore::open(dir.path(), &test_config().storage.rocksdb).unwrap();
    let mut embeddings = Vec::new();
    for i in 0..40_u16 {
        let mut chunk = base_chunk();
        chunk.id = format!("v{i:02}");
        chunk.content = format!("vector fixture {i}");
        store.store_chunk(&chunk).unwrap();
        embeddings.push((chunk.id, vec![1.0, f32::from(i) / 20.0]));
    }
    Fixture {
        _dir: dir,
        store,
        embeddings,
    }
}

/// BM25 lane: two lane hits (`v05`, `v12`), three hits below the vector
/// lane's cut (`v25`, `v30`, `v35`) and one chunk without an embedding.
fn lexical() -> Vec<SearchResult> {
    [
        ("v05", 9.0_f32),
        ("v12", 7.5),
        ("v25", 6.0),
        ("v30", 4.0),
        ("v35", 2.5),
        ("noemb", 8.0),
    ]
    .iter()
    .map(|(id, score)| hit(id, *score))
    .collect()
}

fn fuse(fx: &Fixture, floor: f64, fill: bool) -> Vec<HybridSearchResult> {
    let mut config = test_config();
    config.search.decay.l0_lambda = 0.0;
    config.search.decay.l1_lambda = 0.0;
    config.search.vector_similarity_floor = floor;
    config.search.vector_fill_missing = fill;
    HybridSearchEngine::new(config)
        .search_with_vector_budget(
            lexical(),
            &fx.embeddings,
            &[1.0, 0.0],
            VectorSearchOptions {
                store: Some(&fx.store),
                stream_filter: None,
                guaranteed: None,
                budget: SearchCandidateBudget::new(10, 10).unwrap(),
            },
        )
        .unwrap()
}

/// Ids and score bits captured from the pre-fill code (origin/main
/// c39b754b) on this fixture, floor 0.0.
const GOLDEN_FLOOR_0: [Row; 24] = [
    ("v05", 0x3fef6d3ea0000000, 0x3f800000, 0x3f785b43),
    ("v12", 0x3feb216a73333334, 0x3f555555, 0x3f5b84a8),
    ("v00", 0x3fe3333333333333, 0x00000000, 0x3f800000),
    ("v01", 0x3fe32d114ccccccd, 0x00000000, 0x3f7fae3c),
    ("v02", 0x3fe31ace79999999, 0x00000000, 0x3f7ebac1),
    ("v03", 0x3fe2fcd1eccccccd, 0x00000000, 0x3f7d2aef),
    ("v04", 0x3fe2d3c00ccccccd, 0x00000000, 0x3f7b0756),
    ("v06", 0x3fe263e859999999, 0x00000000, 0x3f75341a),
    ("v07", 0x3fe21f40a0000000, 0x00000000, 0x3f71a0b3),
    ("v08", 0x3fe1d3a620000000, 0x00000000, 0x3f6db0a7),
    ("v09", 0x3fe18246cccccccd, 0x00000000, 0x3f6973b0),
    ("v10", 0x3fe12c49d9999999, 0x00000000, 0x3f64f92e),
    ("v11", 0x3fe0d2c6b9999999, 0x00000000, 0x3f604faf),
    ("v13", 0x3fe0191e53333333, 0x00000000, 0x3f56a43f),
    ("v14", 0x3fdf7560a6666666, 0x00000000, 0x3f51b92f),
    ("v15", 0x3fdeb851f3333333, 0x00000000, 0x3f4ccccd),
    ("v16", 0x3fddfc4099999999, 0x00000000, 0x3f47e704),
    ("v17", 0x3fdd422b00000000, 0x00000000, 0x3f430e74),
    ("v18", 0x3fdc8ae0e6666666, 0x00000000, 0x3f3e4886),
    ("v19", 0x3fdbd708cccccccd, 0x00000000, 0x3f399990),
    ("noemb", 0x3fd6c16c1999999a, 0x3f638e39, 0x00000000),
    ("v25", 0x3fd111111999999a, 0x3f2aaaab, 0x00000000),
    ("v30", 0x3fc6c16c1999999a, 0x3ee38e39, 0x00000000),
    ("v35", 0x3fbc71c733333334, 0x3e8e38e4, 0x00000000),
];

/// Same capture with `vector_similarity_floor = 0.95`.
const GOLDEN_FLOOR_095: [Row; 12] = [
    ("v05", 0x3fef6d3ea0000000, 0x3f800000, 0x3f785b43),
    ("v00", 0x3fe3333333333333, 0x00000000, 0x3f800000),
    ("v01", 0x3fe32d114ccccccd, 0x00000000, 0x3f7fae3c),
    ("v02", 0x3fe31ace79999999, 0x00000000, 0x3f7ebac1),
    ("v03", 0x3fe2fcd1eccccccd, 0x00000000, 0x3f7d2aef),
    ("v04", 0x3fe2d3c00ccccccd, 0x00000000, 0x3f7b0756),
    ("v06", 0x3fe263e859999999, 0x00000000, 0x3f75341a),
    ("noemb", 0x3fd6c16c1999999a, 0x3f638e39, 0x00000000),
    ("v12", 0x3fd555554ccccccd, 0x3f555555, 0x00000000),
    ("v25", 0x3fd111111999999a, 0x3f2aaaab, 0x00000000),
    ("v30", 0x3fc6c16c1999999a, 0x3ee38e39, 0x00000000),
    ("v35", 0x3fbc71c733333334, 0x3e8e38e4, 0x00000000),
];

#[test]
fn flag_off_fusion_is_byte_identical_to_the_pre_fill_code() {
    let fx = fixture();
    assert_eq!(rows(&fuse(&fx, 0.0, false)), golden(&GOLDEN_FLOOR_0));
    assert_eq!(rows(&fuse(&fx, 0.95, false)), golden(&GOLDEN_FLOOR_095));
}

fn vector_score(results: &[HybridSearchResult], id: &str) -> f32 {
    results
        .iter()
        .find(|r| r.id == id)
        .map(|r| r.vector_score)
        .unwrap()
}

fn rank(results: &[HybridSearchResult], id: &str) -> usize {
    results.iter().position(|r| r.id == id).unwrap()
}

#[test]
fn flag_on_fills_bm25_only_hits_with_their_real_cosine() {
    let fx = fixture();
    let off = fuse(&fx, 0.0, false);
    let on = fuse(&fx, 0.0, true);
    assert_eq!(on.len(), off.len());
    // Lane max is v00's cosine 1.0, so the fill is the raw cosine itself.
    for (id, i) in [("v25", 25.0_f32), ("v30", 30.0), ("v35", 35.0)] {
        let expected = cosine_similarity(&[1.0, 0.0], &[1.0, i / 20.0]);
        assert_eq!(vector_score(&on, id), expected, "{id}");
        assert!(rank(&on, id) < rank(&off, id), "{id} must move up");
    }
    assert_eq!(vector_score(&on, "noemb"), 0.0, "no embedding keeps 0");
    // Lane hits keep their exact scores.
    for r in on.iter().filter(|r| r.id.starts_with('v')) {
        if let Some(before) = off.iter().find(|o| o.id == r.id && o.vector_score > 0.0) {
            assert_eq!(r.score.to_bits(), before.score.to_bits(), "{}", r.id);
        }
    }
}

#[test]
fn flag_on_respects_the_floor_and_an_empty_lane() {
    let fx = fixture();
    // v12 (cosine ~0.857) is a lane hit dropped by the 0.95 floor: still 0.
    let on = fuse(&fx, 0.95, true);
    assert_eq!(rows(&on), golden(&GOLDEN_FLOOR_095));
    // A floor above every cosine empties the lane: nothing is filled.
    assert_eq!(rows(&fuse(&fx, 1.01, true)), rows(&fuse(&fx, 1.01, false)));
}
