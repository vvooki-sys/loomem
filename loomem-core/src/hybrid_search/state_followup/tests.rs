//! `[search] state_followup_*`: seed selection, the hit merge, and the full
//! fusion pipeline with the step off, byte-identical to origin/main.
use super::*;
use crate::config::Config;
use crate::hybrid_search::tests::{base_chunk, test_config};
use crate::hybrid_search::{
    HybridSearchEngine, HybridSearchResult, SearchCandidateBudget, VectorSearchOptions,
};
use crate::storage::{ExtractionMeta, FactType, RocksDbStore};
use crate::tantivy_index::SearchResult;
use crate::vector_search::vector_search;

const QUERY: [f32; 3] = [1.0, 0.0, 0.0];
/// Cosine 0.83 to the query: inside the vector lane (between `c13` and
/// `c14`), so it is a fused candidate the seed selection can see.
const SEED_A: [f32; 3] = [0.83, 0.0, 0.5578];
/// Cosine 0.6 to the query (below the lane's cut), 0.944 to `SEED_A`.
const UPDATE_A: [f32; 3] = [0.6, 0.0, 0.8];

fn meta(fact_type: FactType) -> ExtractionMeta {
    let name = serde_json::to_value(fact_type).unwrap();
    serde_json::from_value(serde_json::json!({"fact_type": name, "confidence": 1.0})).unwrap()
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

struct Fixture {
    _dir: tempfile::TempDir,
    store: RocksDbStore,
    embeddings: Vec<(String, Vec<f32>)>,
}

impl Fixture {
    fn put(&mut self, chunk: crate::storage::Chunk, vector: &[f32]) {
        self.store.store_chunk(&chunk).unwrap();
        self.embeddings.push((chunk.id, vector.to_vec()));
    }

    fn embedding(&self, id: &str) -> &[f32] {
        &self.embeddings.iter().find(|(e, _)| e == id).unwrap().1
    }
}

/// 40 chunks `c00`..`c39` with vectors `[1, i/20, 0]` (cosine to the query
/// falls with `i`; the default budget's vector lane keeps `c00`..`c18` plus
/// `seed-a`), typed by `i % 4`: project_state, event, fact, none. `c04` is
/// superseded, `c08` is not latest. `seed-a` (project_state) is close to
/// `update-a` (event, out of every lane) and to `noise-a` (untyped,
/// orthogonal to the query).
fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let store = RocksDbStore::open(dir.path(), &test_config().storage.rocksdb).unwrap();
    let mut fx = Fixture {
        _dir: dir,
        store,
        embeddings: Vec::new(),
    };
    for i in 0..40_u16 {
        let mut chunk = base_chunk();
        chunk.id = format!("c{i:02}");
        chunk.content = format!("state fixture {i}");
        chunk.extraction_meta = match i % 4 {
            0 => Some(meta(FactType::ProjectState)),
            1 => Some(meta(FactType::Event)),
            2 => Some(meta(FactType::Fact)),
            _ => None,
        };
        if i == 4 {
            chunk.superseded_by = Some("c05".into());
        }
        if i == 8 {
            chunk.is_latest = false;
        }
        fx.put(chunk, &[1.0, f32::from(i) / 20.0, 0.0]);
    }
    let mut seed = base_chunk();
    seed.id = "seed-a".into();
    seed.content = "ashdown is on the waiting list for an ottoman".into();
    seed.extraction_meta = Some(meta(FactType::ProjectState));
    fx.put(seed, &SEED_A);
    let mut update = base_chunk();
    update.id = "update-a".into();
    update.content = "ashdown's ottoman was finished and collected".into();
    update.extraction_meta = Some(meta(FactType::Event));
    fx.put(update, &UPDATE_A);
    let mut noise = base_chunk();
    noise.id = "noise-a".into();
    noise.content = "unrelated note".into();
    fx.put(noise, &[0.0, 0.0, 1.0]);
    fx
}

/// BM25 lane: the seed (lane hit), `c05` (lane hit), `c25` (below the
/// vector lane's cut) and a chunk without an embedding.
fn lexical() -> Vec<SearchResult> {
    [
        ("seed-a", 9.0_f32),
        ("c05", 7.0),
        ("c25", 5.0),
        ("noemb", 3.0),
    ]
    .iter()
    .map(|(id, score)| hit(id, *score))
    .collect()
}

/// Time decay off so the wall clock cannot reorder the fixture.
fn config(floor: f64) -> Config {
    let mut config = test_config();
    config.search.decay.l0_lambda = 0.0;
    config.search.decay.l1_lambda = 0.0;
    config.search.vector_similarity_floor = floor;
    config
}

fn budget() -> SearchCandidateBudget {
    SearchCandidateBudget::new(10, 10).unwrap()
}

fn fuse(fx: &Fixture, engine: &HybridSearchEngine) -> Vec<HybridSearchResult> {
    engine
        .search_with_vector_budget(
            lexical(),
            &fx.embeddings,
            &QUERY,
            VectorSearchOptions {
                store: Some(&fx.store),
                stream_filter: None,
                guaranteed: None,
                budget: budget(),
            },
        )
        .unwrap()
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

/// Ids and score bits of `fuse` on this fixture, captured by running the
/// same fixture on unmodified origin/main (920edd00, before `finish_score`
/// was extracted from `fuse_lanes` and before this module existed), floor
/// 0.0: 20 vector-lane hits plus `c25` and `noemb` from the BM25 lane.
const GOLDEN_FLOOR_0: [Row; 22] = [
    ("seed-a", 0x3fecbc552ccccccd, 0x3f800000, 0x3f5479c5),
    ("c05", 0x3fec951120000000, 0x3f471c72, 0x3f785b43),
    ("c00", 0x3fe3333333333333, 0x00000000, 0x3f800000),
    ("c01", 0x3fe32d114ccccccd, 0x00000000, 0x3f7fae3c),
    ("c02", 0x3fe31ace79999999, 0x00000000, 0x3f7ebac1),
    ("c03", 0x3fe2fcd1eccccccd, 0x00000000, 0x3f7d2aef),
    ("c04", 0x3fe2d3c00ccccccd, 0x00000000, 0x3f7b0756),
    ("c06", 0x3fe263e859999999, 0x00000000, 0x3f75341a),
    ("c07", 0x3fe21f40a0000000, 0x00000000, 0x3f71a0b3),
    ("c08", 0x3fe1d3a620000000, 0x00000000, 0x3f6db0a7),
    ("c09", 0x3fe18246cccccccd, 0x00000000, 0x3f6973b0),
    ("c10", 0x3fe12c49d9999999, 0x00000000, 0x3f64f92e),
    ("c11", 0x3fe0d2c6b9999999, 0x00000000, 0x3f604faf),
    ("c12", 0x3fe076bfcccccccd, 0x00000000, 0x3f5b84a8),
    ("c13", 0x3fe0191e53333333, 0x00000000, 0x3f56a43f),
    ("c14", 0x3fdf7560a6666666, 0x00000000, 0x3f51b92f),
    ("c15", 0x3fdeb851f3333333, 0x00000000, 0x3f4ccccd),
    ("c16", 0x3fddfc4099999999, 0x00000000, 0x3f47e704),
    ("c17", 0x3fdd422b00000000, 0x00000000, 0x3f430e74),
    ("c18", 0x3fdc8ae0e6666666, 0x00000000, 0x3f3e4886),
    ("c25", 0x3fcc71c733333334, 0x3f0e38e4, 0x00000000),
    ("noemb", 0x3fc111111999999a, 0x3eaaaaab, 0x00000000),
];

/// Same capture with `vector_similarity_floor = 0.75` (`c18`, cosine
/// 0.743, leaves the vector lane).
const GOLDEN_FLOOR_075: [Row; 21] = [
    ("seed-a", 0x3fecbc552ccccccd, 0x3f800000, 0x3f5479c5),
    ("c05", 0x3fec951120000000, 0x3f471c72, 0x3f785b43),
    ("c00", 0x3fe3333333333333, 0x00000000, 0x3f800000),
    ("c01", 0x3fe32d114ccccccd, 0x00000000, 0x3f7fae3c),
    ("c02", 0x3fe31ace79999999, 0x00000000, 0x3f7ebac1),
    ("c03", 0x3fe2fcd1eccccccd, 0x00000000, 0x3f7d2aef),
    ("c04", 0x3fe2d3c00ccccccd, 0x00000000, 0x3f7b0756),
    ("c06", 0x3fe263e859999999, 0x00000000, 0x3f75341a),
    ("c07", 0x3fe21f40a0000000, 0x00000000, 0x3f71a0b3),
    ("c08", 0x3fe1d3a620000000, 0x00000000, 0x3f6db0a7),
    ("c09", 0x3fe18246cccccccd, 0x00000000, 0x3f6973b0),
    ("c10", 0x3fe12c49d9999999, 0x00000000, 0x3f64f92e),
    ("c11", 0x3fe0d2c6b9999999, 0x00000000, 0x3f604faf),
    ("c12", 0x3fe076bfcccccccd, 0x00000000, 0x3f5b84a8),
    ("c13", 0x3fe0191e53333333, 0x00000000, 0x3f56a43f),
    ("c14", 0x3fdf7560a6666666, 0x00000000, 0x3f51b92f),
    ("c15", 0x3fdeb851f3333333, 0x00000000, 0x3f4ccccd),
    ("c16", 0x3fddfc4099999999, 0x00000000, 0x3f47e704),
    ("c17", 0x3fdd422b00000000, 0x00000000, 0x3f430e74),
    ("c25", 0x3fcc71c733333334, 0x3f0e38e4, 0x00000000),
    ("noemb", 0x3fc111111999999a, 0x3eaaaaab, 0x00000000),
];

/// The server's step with the flag off: fusion, then nothing (the gate in
/// `state_followup_seeds` / `merge_state_followup` returns before any
/// store read). Must equal the pre-module capture bit for bit.
#[test]
fn flag_off_pipeline_is_byte_identical_to_origin_main() {
    let fx = fixture();
    for (floor, expected) in [(0.0, &GOLDEN_FLOOR_0[..]), (0.75, &GOLDEN_FLOOR_075[..])] {
        let engine = HybridSearchEngine::new(config(floor));
        let mut pool = fuse(&fx, &engine);
        let seeds = engine.state_followup_seeds(&pool, 10, &fx.store);
        assert!(seeds.is_empty(), "disabled: no seeds");
        let hits = vec![seed_hits(&fx, "seed-a")];
        let outcome = engine.merge_state_followup(&mut pool, hits, followup_pool(&fx, None));
        assert_eq!(outcome, StateFollowupOutcome::default());
        assert_eq!(rows(&pool), golden(expected), "floor {floor}");
    }
}

fn enabled(floor: f64) -> Config {
    let mut config = config(floor);
    config.search.state_followup.enabled = true;
    config
}

fn followup_pool<'a>(fx: &'a Fixture, guaranteed: Option<&'a HashSet<String>>) -> FollowupPool<'a> {
    FollowupPool {
        store: &fx.store,
        budget: budget(),
        guaranteed,
    }
}

/// What the server computes per seed: the seed's own embedding ranked
/// against the fixture's embedding list (`hits_per_seed` 60 covers all 43).
fn seed_hits(fx: &Fixture, seed_id: &str) -> SeedHits {
    SeedHits {
        seed_id: seed_id.to_string(),
        hits: vector_search(&fx.embeddings, fx.embedding(seed_id), 60),
    }
}

fn ids(seeds: &[Seed]) -> Vec<&str> {
    seeds.iter().map(|s| s.id.as_str()).collect()
}

fn rank(results: &[HybridSearchResult], id: &str) -> usize {
    results.iter().position(|r| r.id == id).unwrap()
}

fn find<'a>(results: &'a [HybridSearchResult], id: &str) -> &'a HybridSearchResult {
    results.iter().find(|r| r.id == id).unwrap()
}

#[test]
fn seeds_follow_rank_order_type_latest_and_superseded() {
    let fx = fixture();
    let mut config = enabled(0.0);
    let pool = fuse(&fx, &HybridSearchEngine::new(config.clone()));
    // Top 10: seed-a, c05, c00, c01, c02, c03, c04, c06, c07, c08.
    assert_eq!(rank(&pool, "c08"), 9);

    let engine = HybridSearchEngine::new(config.clone());
    let seeds = engine.state_followup_seeds(&pool, 10, &fx.store);
    assert_eq!(
        ids(&seeds),
        ["seed-a", "c05", "c00"],
        "max_seeds 3, rank order"
    );
    assert_eq!(
        seeds[0].content,
        "ashdown is on the waiting list for an ottoman"
    );

    config.search.state_followup.max_seeds = 10;
    let engine = HybridSearchEngine::new(config.clone());
    // c02/c06 are facts, c03/c07 untyped, c04 superseded, c08 not latest.
    assert_eq!(
        ids(&engine.state_followup_seeds(&pool, 10, &fx.store)),
        ["seed-a", "c05", "c00", "c01"]
    );
    assert_eq!(
        ids(&engine.state_followup_seeds(&pool, 2, &fx.store)),
        ["seed-a", "c05"],
        "only the requested top-k is scanned"
    );

    config.search.state_followup.fact_types = vec![FactType::ProjectState];
    let engine = HybridSearchEngine::new(config);
    assert_eq!(
        ids(&engine.state_followup_seeds(&pool, 10, &fx.store)),
        ["seed-a", "c00"]
    );
}

#[test]
fn merge_adds_the_seeds_neighbours_and_recuts_the_pool() {
    let fx = fixture();
    let engine = HybridSearchEngine::new(enabled(0.0));
    let off = fuse(&fx, &engine);
    assert_eq!(off.len(), 22);
    let mut on = off.clone();
    let outcome = engine.merge_state_followup(
        &mut on,
        vec![seed_hits(&fx, "seed-a")],
        followup_pool(&fx, None),
    );
    assert_eq!(outcome.seeds, ["seed-a"]);
    // 22 + 22 neighbours (update-a, noise-a, c19..c39 minus c25), cut to 30.
    assert_eq!(on.len(), 30);
    assert!(
        on.iter().all(|r| r.id != "c25" && r.id != "noemb"),
        "pushed out"
    );
    // update-a is the seed's best non-seed neighbour: normalized 1.0, so
    // its score is the full vector weight, above every lane hit but c00.
    let update = find(&on, "update-a");
    let cosine = crate::vector_search::cosine_similarity(&SEED_A, &UPDATE_A);
    assert_eq!(update.vector_score, cosine);
    assert_eq!(update.bm25_score, 0.0);
    assert_eq!(update.score.to_bits(), (0.6_f64 * 1.0 * 1.0).to_bits());
    assert_eq!(
        update.content,
        "ashdown's ottoman was finished and collected"
    );
    assert!(rank(&on, "update-a") < rank(&on, "c01"));
    // noise-a: 0.558 / 0.944 of the vector weight.
    let noise = find(&on, "noise-a");
    let normalized = noise.vector_score / cosine;
    assert_eq!(
        noise.score.to_bits(),
        (0.6_f64 * f64::from(normalized) * 1.0).to_bits()
    );
    // Existing candidates keep their exact scores; the pool stays sorted.
    for r in &off {
        if let Some(after) = on.iter().find(|o| o.id == r.id) {
            assert_eq!(after.score.to_bits(), r.score.to_bits(), "{}", r.id);
            assert_eq!(after.vector_score.to_bits(), r.vector_score.to_bits());
        }
    }
    assert!(on.windows(2).all(|w| w[0].score >= w[1].score));
    // injected_ids: the added ids that survived the cut, in merge order.
    let mut expected: Vec<&str> = on
        .iter()
        .map(|r| r.id.as_str())
        .filter(|id| off.iter().all(|r| r.id != *id))
        .collect();
    let mut injected: Vec<&str> = outcome.injected_ids.iter().map(String::as_str).collect();
    // 30 kept - 20 surviving old candidates (c25 and noemb fell out).
    assert_eq!(injected.len(), 10);
    expected.sort_unstable();
    injected.sort_unstable();
    assert_eq!(injected, expected);
}

#[test]
fn merge_skips_the_seed_existing_ids_and_cosines_below_the_floor() {
    let fx = fixture();
    // Floor 0.7 to the seed: only update-a (0.944) and lane hits c00..c06
    // (already in the pool) qualify; noise-a (0.558) and c19+ do not.
    let engine = HybridSearchEngine::new(enabled(0.7));
    let mut pool = fuse(&fx, &engine);
    let before = rows(&pool);
    let outcome = engine.merge_state_followup(
        &mut pool,
        vec![seed_hits(&fx, "seed-a")],
        followup_pool(&fx, None),
    );
    assert_eq!(outcome.injected_ids, ["update-a"]);
    assert_eq!(pool.len(), before.len() + 1);
    assert_eq!(pool.iter().filter(|r| r.id == "seed-a").count(), 1);
    let kept: Vec<_> = rows(&pool)
        .into_iter()
        .filter(|(id, ..)| id != "update-a")
        .collect();
    assert_eq!(kept, before);
}

#[test]
fn merge_applies_the_weight_to_the_vector_term_only() {
    let fx = fixture();
    let mut config = enabled(0.0);
    config.search.state_followup.weight = 0.5;
    let engine = HybridSearchEngine::new(config);
    let mut pool = fuse(&fx, &engine);
    let seed_a_before = find(&pool, "seed-a").score.to_bits();
    engine.merge_state_followup(
        &mut pool,
        vec![seed_hits(&fx, "seed-a")],
        followup_pool(&fx, None),
    );
    let update = find(&pool, "update-a");
    assert_eq!(update.score.to_bits(), (0.6_f64 * 1.0 * 0.5).to_bits());
    assert_eq!(update.time_decay_factor, 1.0);
    assert_eq!(find(&pool, "seed-a").score.to_bits(), seed_a_before);
}

#[test]
fn merge_adds_a_chunk_once_across_seeds_and_honours_guaranteed_ids() {
    let fx = fixture();
    let engine = HybridSearchEngine::new(enabled(0.0));
    let mut pool = fuse(&fx, &engine);
    let guaranteed: HashSet<String> = ["noemb".to_string()].into();
    let outcome = engine.merge_state_followup(
        &mut pool,
        vec![seed_hits(&fx, "seed-a"), seed_hits(&fx, "c00")],
        followup_pool(&fx, Some(&guaranteed)),
    );
    assert_eq!(outcome.seeds, ["seed-a", "c00"]);
    let mut unique = outcome.injected_ids.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), outcome.injected_ids.len());
    assert_eq!(pool.iter().filter(|r| r.id == "update-a").count(), 1);
    // update-a was added by seed-a (first seed), so it is scored against
    // seed-a's scale, not c00's.
    let cosine = crate::vector_search::cosine_similarity(&SEED_A, &UPDATE_A);
    assert_eq!(find(&pool, "update-a").vector_score, cosine);
    // A guaranteed id survives the cut beyond the pool size.
    assert_eq!(pool.len(), 31);
    assert_eq!(pool.last().map(|r| r.id.as_str()), Some("noemb"));
}

#[test]
fn merge_without_hits_or_with_an_unknown_seed_scale_is_a_no_op() {
    let fx = fixture();
    let engine = HybridSearchEngine::new(enabled(0.0));
    let mut pool = fuse(&fx, &engine);
    let before = rows(&pool);
    assert_eq!(
        engine.merge_state_followup(&mut pool, Vec::new(), followup_pool(&fx, None)),
        StateFollowupOutcome::default()
    );
    // A seed whose hit list holds only itself (or NaN cosines) has no
    // scale: its seed id is recorded, nothing is injected.
    let lonely = SeedHits {
        seed_id: "seed-a".into(),
        hits: vec![("seed-a".into(), 1.0), ("ghost".into(), f32::NAN)],
    };
    let outcome = engine.merge_state_followup(&mut pool, vec![lonely], followup_pool(&fx, None));
    assert_eq!(outcome.seeds, ["seed-a"]);
    assert!(outcome.injected_ids.is_empty());
    assert_eq!(rows(&pool), before);
}

#[test]
fn seed_scale_excludes_the_seed_and_respects_the_floor() {
    let seed = SeedHits {
        seed_id: "s".into(),
        hits: vec![("s".into(), 1.0), ("a".into(), 0.9), ("b".into(), 0.6)],
    };
    assert_eq!(seed_scale(&seed, 0.0), Some(0.9));
    assert_eq!(seed_scale(&seed, 0.95), None);
    assert!(passes_floor(0.6, 0.6));
    assert!(!passes_floor(0.59, 0.6));
    assert!(!passes_floor(f32::NAN, 0.0));
}

#[test]
fn is_seed_requires_type_latest_and_not_superseded() {
    let types = [FactType::ProjectState, FactType::Event];
    let mut chunk = base_chunk();
    assert!(!is_seed(&chunk, &types), "untyped");
    chunk.extraction_meta = Some(meta(FactType::Fact));
    assert!(!is_seed(&chunk, &types), "wrong type");
    chunk.extraction_meta = Some(meta(FactType::Event));
    assert!(is_seed(&chunk, &types));
    chunk.superseded_by = Some("x".into());
    assert!(!is_seed(&chunk, &types), "superseded");
    chunk.superseded_by = None;
    chunk.is_latest = false;
    assert!(!is_seed(&chunk, &types), "not latest");
    chunk.is_latest = true;
    chunk.deleted_at = Some(1);
    assert!(!is_seed(&chunk, &types), "soft-deleted");
}

/// A soft-deleted neighbour (its delete chain may have left the embedding
/// behind) is never injected; a soft-deleted state fact is never a seed.
#[test]
fn merge_skips_soft_deleted_neighbours_and_seeds() {
    let mut fx = fixture();
    let mut deleted = base_chunk();
    deleted.id = "deleted-a".into();
    deleted.content = "ottoman order cancelled".into();
    deleted.extraction_meta = Some(meta(FactType::Event));
    deleted.deleted_at = Some(1_700_000_000);
    // Cosine 0.70 to the query (outside the vector lane, no BM25 hit) and
    // 0.98 to seed-a, closer than update-a: it would be the top hit.
    const DELETED_A: [f32; 3] = [0.7, 0.0, 0.7141];
    fx.put(deleted, &DELETED_A);
    let engine = HybridSearchEngine::new(enabled(0.0));
    let mut pool = fuse(&fx, &engine);
    let hits = seed_hits(&fx, "seed-a");
    assert_eq!(
        hits.hits[1].0, "deleted-a",
        "second only to the seed itself"
    );
    let outcome = engine.merge_state_followup(&mut pool, vec![hits], followup_pool(&fx, None));
    assert!(!outcome.injected_ids.iter().any(|id| id == "deleted-a"));
    assert!(pool.iter().all(|r| r.id != "deleted-a"));
    assert!(outcome.injected_ids.iter().any(|id| id == "update-a"));
    // The deleted chunk still set the seed's scale (it is a hit), so
    // update-a is normalized against it, not against itself.
    let cosine = |v: &[f32]| crate::vector_search::cosine_similarity(&SEED_A, v);
    let update = find(&pool, "update-a");
    let normalized = cosine(&UPDATE_A) / cosine(&DELETED_A);
    assert_eq!(
        update.score.to_bits(),
        (0.6_f64 * f64::from(normalized) * 1.0).to_bits()
    );
    // A soft-deleted state fact in the top-k is not a seed.
    let mut stale = fx.store.get_chunk("seed-a").unwrap().unwrap();
    stale.deleted_at = Some(1_700_000_000);
    fx.store.store_chunk(&stale).unwrap();
    assert_eq!(
        ids(&engine.state_followup_seeds(&pool, 10, &fx.store)),
        ["c05", "c00", "c01"]
    );
}

/// A `[search]` table without any `state_followup_*` key loads the
/// off/neutral defaults; a partial table keeps the rest at default.
#[test]
fn config_defaults_when_keys_are_missing() {
    let empty: StateFollowupConfig = toml::from_str("").unwrap();
    assert_eq!(empty.enabled, false);
    assert_eq!(empty.max_seeds, 3);
    assert_eq!(empty.hits_per_seed, 60);
    assert_eq!(empty.fact_types, [FactType::ProjectState, FactType::Event]);
    assert_eq!(empty.weight, 1.0);
    assert!(!empty.with_date_filter);
    let partial: StateFollowupConfig =
        toml::from_str("state_followup_enabled = true\nstate_followup_fact_types = [\"fact\"]\n")
            .unwrap();
    assert!(partial.enabled);
    assert_eq!(partial.fact_types, [FactType::Fact]);
    assert_eq!(partial.max_seeds, 3);
    assert!(
        toml::from_str::<StateFollowupConfig>("state_followup_fact_types = [\"bogus\"]").is_err()
    );
}
