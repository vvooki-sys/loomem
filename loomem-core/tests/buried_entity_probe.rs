//! Cycle/012 — buried-entity probe + incident fixture + rare-term lane
//! measurements.
//!
//! Production incident (2026-07-07): a fact about person B, mentioned once
//! inside a long consolidated profile of person A, is practically
//! unfindable through `memory_search` — fresh, loosely-matching chunks about
//! a *different* person sharing B's first name outrank the single strong
//! lexical match. This eval reproduces the incident shape with fully
//! synthetic data (no production content — PII blockers from open-source
//! prep apply) and measures R@5 / R@10 with the rare-term lane OFF
//! (baseline) and ON.
//!
//! Deterministic: fixed-seed xorshift generator, synthetic embeddings with
//! designed cosine structure, no LLM, no network (pattern:
//! `forgetting_eval.rs`, cycle/010). The probe exercises the loomem-core
//! retrieval primitives directly (Tantivy BM25 → synthetic vector channel →
//! `fuse_with_vector[_guaranteed]`), i.e. the fusion stage of the pipeline;
//! server-side graph/level/tier boosts are out of scope here and covered by
//! the handler's own tests. Baseline numbers are written back to
//! `tests/fixtures/buried_entity_baseline.json` (same write-back convention
//! as `forgetting_eval.rs`).
//!
//! Run: `cargo test -p loomem-core --test buried_entity_probe -- --nocapture`

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use loomem_core::config::{RocksDbConfig, TantivyConfig};
use loomem_core::search::rare_term::RareTermLaneConfig;
use loomem_core::search::{rare_df_threshold, select_rare_tokens};
use loomem_core::storage::Chunk;
use loomem_core::{HybridSearchEngine, RocksDbStore, TantivyIndex, TextDocument};
use serde::Serialize;
use tempfile::TempDir;

// ─── deterministic rng (no new deps — CLAUDE.md §dependencies) ──────────────

/// xorshift64* — deterministic, dependency-free PRNG. NOT cryptographic;
/// used only to vary synthetic fixture content reproducibly.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed.max(1))
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    /// Uniform in `[0, n)`.
    fn below(&mut self, n: usize) -> usize {
        debug_assert!(n > 0);
        usize::try_from(self.next_u64() % (n as u64)).unwrap_or(0)
    }
    /// Uniform f64 in `[lo, hi)`.
    fn range_f64(&mut self, lo: f64, hi: f64) -> f64 {
        let unit = (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64;
        lo + unit * (hi - lo)
    }
}

// ─── synthetic names (brief §krok 2: built-in first-name↔diminutive list) ───

const NAME_PAIRS: &[(&str, &str)] = &[
    ("Agnieszka", "Aga"),
    ("Bartosz", "Bartek"),
    ("Katarzyna", "Kasia"),
    ("Tomasz", "Tomek"),
    ("Magdalena", "Magda"),
    ("Aleksandra", "Ola"),
    ("Krzysztof", "Krzysiek"),
    ("Małgorzata", "Gosia"),
    ("Stanisław", "Staszek"),
    ("Elżbieta", "Ela"),
    ("Zuzanna", "Zuzia"),
    ("Franciszek", "Franek"),
];

/// Synthetic, collision-free surname per case: two syllables + rare suffix.
/// 20×20 combinations cover 50 cases without repeats; none of the syllables
/// appear in the noise vocabulary, so DF(surname) == needle mentions only.
fn surname_for_case(case: usize) -> String {
    const A: &[&str] = &[
        "Wrzo", "Grze", "Skro", "Ple", "Mru", "Chwa", "Dzwo", "Krze", "Szro", "Brzy", "Gwo",
        "Trzna", "Zgrze", "Pstro", "Kwo", "Smre", "Drwe", "Chro", "Szczy", "Brwi",
    ];
    const B: &[&str] = &[
        "sik", "wik", "czak", "nis", "bor", "gis", "lec", "mir", "dun", "zut", "kosz", "wąs",
        "pik", "rud", "gon", "mysz", "łak", "cur", "bek", "zor",
    ];
    format!("{}{}", A[case % A.len()], B[(case / A.len()) % B.len()])
}

const COMMON_SURNAMES: &[&str] = &["Nowak", "Kowalska", "Wiśniewski", "Zielińska"];

const NOISE_TOPICS: &[&str] = &[
    "Sprint review covered the payment gateway migration and the retry queue backlog.",
    "The quarterly planning session moved the analytics dashboard rollout to March.",
    "Deployment pipeline for the mobile app now runs integration tests in parallel.",
    "Client workshop about the loyalty program produced twelve feature ideas.",
    "The design team finalized the color system for the new onboarding flow.",
    "Infrastructure costs dropped after moving the staging cluster to spot instances.",
    "The content calendar for the product blog is planned through the summer.",
    "Support ticket volume spiked after the notification service outage on Monday.",
    "The data warehouse sync job was rewritten to use incremental snapshots.",
    "Legal review of the new vendor contract is expected to finish this week.",
];

// ─── fixture construction ────────────────────────────────────────────────────

#[derive(Clone)]
struct CaseSpec {
    stream: String,
    needle_id: String,
    /// (full first name, diminutive, synthetic surname) of buried person B.
    person_b: (String, String, String),
    chunks: Vec<(String, String, i64, i32)>, // (id, content, age_days, level)
    /// Designed cosine similarity per chunk id (query ↔ chunk).
    sims: Vec<(String, f64)>,
}

/// One case mirrors the incident: a long L1 profile of person A holding a
/// single line about person B (the needle), a few fresh short chunks about
/// person C who shares B's *first* name, and topical noise.
fn build_case(case: usize, rng: &mut Rng) -> CaseSpec {
    let (first, dim) = NAME_PAIRS[case % NAME_PAIRS.len()];
    let surname = surname_for_case(case);
    let stream = format!("case{case:03}");
    let needle_id = format!("c{case:03}-needle");

    let person_a_first = NAME_PAIRS[(case + 5) % NAME_PAIRS.len()].0;
    let person_a_surname = COMMON_SURNAMES[case % COMMON_SURNAMES.len()];

    // ~3600-char profile of person A with ONE line about B (brief §krok 1).
    let mut profile = format!(
        "PROFIL OSOBY: {person_a_first} {person_a_surname}. Rola: dyrektor programu \
         lojalnościowego. Zakres: strategia produktu, budżet, relacje z partnerami. "
    );
    while profile.len() < 3300 {
        profile.push_str(NOISE_TOPICS[rng.below(NOISE_TOPICS.len())]);
        profile.push(' ');
    }
    profile.push_str(&format!(
        "POPRZEDNICZKI NA STANOWISKU: {first} {surname} oraz inne osoby z zespołu — \
         odeszły do firmy zewnętrznej. "
    ));
    while profile.len() < 3600 {
        profile.push_str(NOISE_TOPICS[rng.below(NOISE_TOPICS.len())]);
        profile.push(' ');
    }

    let mut chunks: Vec<(String, String, i64, i32)> = Vec::new();
    let mut sims: Vec<(String, f64)> = Vec::new();

    // Needle: old consolidated profile (L1, 45 days) — semantically about A,
    // so its similarity to a query about B is low.
    chunks.push((needle_id.clone(), profile, 45, 1));
    sims.push((needle_id.clone(), rng.range_f64(0.22, 0.32)));

    // 4 fresh short chunks about person C (same first name, common surname).
    let c_surname = COMMON_SURNAMES[(case + 1) % COMMON_SURNAMES.len()];
    for k in 0..4 {
        let id = format!("c{case:03}-fresh{k}");
        let content = format!(
            "{first} {c_surname} prowadziła wczoraj warsztat o nowym cenniku. \
             Umówione follow-upy: {first} przygotuje podsumowanie i wyśle notatkę do zespołu. {}",
            NOISE_TOPICS[rng.below(NOISE_TOPICS.len())]
        );
        chunks.push((id.clone(), content, 1 + i64::from(k), 0));
        // Fresh chunks genuinely mention the queried first name → highest sims.
        sims.push((id, rng.range_f64(0.55, 0.72)));
    }

    // 30 topical-noise chunks, mixed ages.
    for k in 0..30 {
        let id = format!("c{case:03}-noise{k:02}");
        let content = format!(
            "{} {}",
            NOISE_TOPICS[rng.below(NOISE_TOPICS.len())],
            NOISE_TOPICS[rng.below(NOISE_TOPICS.len())]
        );
        let age = 3 + i64::try_from(rng.below(28)).unwrap_or(0);
        chunks.push((id.clone(), content, age, 0));
        sims.push((id, rng.range_f64(0.05, 0.35)));
    }

    CaseSpec {
        stream,
        needle_id,
        person_b: (first.to_string(), dim.to_string(), surname),
        chunks,
        sims,
    }
}

// ─── shared env (single index + store, per-case streams) ────────────────────

fn rocksdb_config() -> RocksDbConfig {
    RocksDbConfig {
        max_open_files: 50,
        compression: "lz4".to_string(),
        write_buffer_size: 4 * 1024 * 1024,
        max_write_buffer_number: 2,
    }
}

fn tantivy_config() -> TantivyConfig {
    TantivyConfig {
        enabled: true,
        heap_size_mb: 32,
        drift_warn_pct: 5.0,
        auto_rebuild_on_drift: false,
    }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(0))
        .unwrap_or(0)
}

fn base_chunk(id: &str, stream: &str, content: &str, ts: i64, level: i32) -> Chunk {
    Chunk {
        id: id.to_string(),
        content: content.to_string(),
        stream: stream.to_string(),
        level,
        score: 1.0,
        timestamp: u64::try_from(ts).unwrap_or(0),
        consolidated: level == 1,
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
    }
}

struct Env {
    _temp: TempDir,
    tantivy: TantivyIndex,
    store: RocksDbStore,
    engine: HybridSearchEngine,
    top_k: usize,
}

fn probe_config() -> loomem_core::config::Config {
    loomem_core::config::Config {
        storage: loomem_core::config::StorageConfig {
            data_dir: "./data".into(),
            rocksdb: rocksdb_config(),
            tantivy: tantivy_config(),
            vector_enabled: true,
            intent_log: loomem_core::config::IntentLogConfig::default(),
        },
        search: loomem_core::config::SearchConfig {
            top_k: 10,
            surprise_boost: 1.5,
            hybrid_weights: loomem_core::config::HybridWeightsConfig {
                vector: 0.6,
                bm25: 0.4,
            },
            decay: loomem_core::config::DecayConfig {
                l0_lambda: 0.05,
                l1_lambda: 0.03,
            },
            synonyms_file: "synonyms.toml".to_string(),
            entities_file: "entities.toml".to_string(),
            stem_polish: true,
            rerank_enabled: false,
            rerank_candidates: 10,
            rerank_model_dir: None,
            multi_query_enabled: false,
            vector_multi_query: false,
            counting_l0_preference: false,
            importance: loomem_core::config::ImportanceConfig::default(),
            cache: loomem_core::query_cache::QueryCacheConfig::default(),
            graph: loomem_core::config::GraphSearchConfig::default(),
            complexity: loomem_core::config::ComplexityConfig::default(),
            implicit_access_boost_weight: 0.0,
            user_state_boost: 1.0,
            agent_fact_damp: 1.0,
            rare_term_lane: RareTermLaneConfig::default(),
            vector_similarity_floor: 0.0,
            decay_match_relief: 0.0,
            tier1: loomem_core::search::tier1::Tier1Config::default(),
        },
        advisor: loomem_core::config::AdvisorConfig::default(),
        worker: loomem_core::config::WorkerConfig::default(),
        scheduler: loomem_core::config::SchedulerConfig { enabled: false },
        llm: loomem_core::config::LlmConfig::default(),
        server: loomem_core::config::ServerConfig {
            host: "127.0.0.1".into(),
            port: 3030,
            auth_token_env: String::new(),
            honor_caller_trust_source: false,
        },
        resource_guards: loomem_core::config::ResourceGuardsConfig::default(),
        streams: loomem_core::config::StreamsConfig::default(),
        namespaces: std::collections::HashMap::new(),
        pii: loomem_core::config::PiiConfig::default(),
        cost: loomem_core::config::CostConfig::default(),
        memory_generator: loomem_core::config::MemoryGeneratorConfig::default(),
        entity_extraction: loomem_core::config::EntityExtractionConfig::default(),
        contradiction: loomem_core::config::ContradictionConfig::default(),
        knowledge_extraction: loomem_core::config::KnowledgeExtractionConfig::default(),
        profile: loomem_core::config::ProfileConfig::default(),
        manifest: loomem_core::config::ManifestConfig::default(),
        retention: loomem_core::config::RetentionConfig::default(),
        event_log: loomem_core::config::EventLogConfig::default(),
        associator: loomem_core::config::AssociatorConfig::default(),
        feedback: loomem_core::config::FeedbackConfig::default(),
        content_type: loomem_core::config::ContentTypeConfig::default(),
        access_audit: loomem_core::config::AccessAuditConfig::default(),
        rate_limit: loomem_core::config::RateLimitConfig::default(),
        mcp: loomem_core::config::McpConfig::default(),
        dream: loomem_core::config::DreamConfig::default(),
    }
}

/// Cycle/014 WS-1 sweep: swap the fusion engine in place, keeping the (much
/// more expensive) Tantivy index + RocksDB store built by `build_env`. Both
/// knobs under test live purely in the fusion stage, so a single fixture
/// build can carry the whole floor × relief grid.
fn set_search_knobs(env: &mut Env, floor: f64, relief: f64) {
    let mut config = probe_config();
    config.search.vector_similarity_floor = floor;
    config.search.decay_match_relief = relief;
    env.top_k = config.search.top_k;
    env.engine = HybridSearchEngine::new(config);
}

fn build_env(cases: &[CaseSpec]) -> Result<Env> {
    let temp = TempDir::new().context("tempdir")?;
    let mut tantivy = TantivyIndex::open(temp.path().join("tantivy"), &tantivy_config())?;
    let store = RocksDbStore::open(temp.path().join("rocks"), &rocksdb_config())?;
    let now = now_secs();

    for case in cases {
        for (id, content, age_days, level) in &case.chunks {
            let ts = now - age_days * 86_400;
            store.store_chunk(&base_chunk(id, &case.stream, content, ts, *level))?;
            tantivy.index_document(TextDocument {
                id: id.clone(),
                content: content.clone(),
                user_id: "default".to_string(),
                app_id: "default".to_string(),
                level: *level,
                timestamp: ts,
                stream: case.stream.clone(),
                entities: None,
                relations: None,
                event_date: None,
                source_agent: None,
            })?;
        }
    }
    tantivy.commit()?;

    let config = probe_config();
    let top_k = config.search.top_k;
    let engine = HybridSearchEngine::new(config);
    Ok(Env {
        _temp: temp,
        tantivy,
        store,
        engine,
        top_k,
    })
}

// ─── probe pipeline (BM25 → synthetic vector → fusion, lane OFF/ON) ─────────

fn lane_cfg_on() -> RareTermLaneConfig {
    RareTermLaneConfig {
        enabled: true,
        ..RareTermLaneConfig::default()
    }
}

/// Vector channel: designed similarities, top `fetch_k` by score — the same
/// shape `search_with_vector` produces from real embeddings.
fn vector_channel(case: &CaseSpec, fetch_k: usize) -> Vec<(String, f32)> {
    let mut sims: Vec<(String, f32)> = case
        .sims
        .iter()
        .map(|(id, s)| {
            let clamped = s.clamp(-1.0, 1.0);
            // truncation intentional: designed similarity fits f32 exactly
            // enough for ranking purposes (probe-only data).
            (id.clone(), clamped as f32)
        })
        .collect();
    sims.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    sims.truncate(fetch_k);
    sims
}

struct RunOutput {
    ranked_ids: Vec<String>,
    needle_in_pool: bool,
}

/// One retrieval run over a case. `lane` None == baseline (pre-cycle path).
fn run_case(
    env: &Env,
    case: &CaseSpec,
    query: &str,
    lane: Option<&RareTermLaneConfig>,
) -> Result<RunOutput> {
    // BM25 channel — same limits as `bm25_retrieve` (limit * 2).
    let mut bm25 = env
        .tantivy
        .search_with_stream(query, &case.stream, env.top_k * 2)?;

    // Rare-term lane: token DF → mandatory posting-list candidates.
    let mut guaranteed: HashSet<String> = HashSet::new();
    if let Some(cfg) = lane {
        let n_docs = env.tantivy.count_stream(&case.stream)?;
        let threshold = rare_df_threshold(n_docs, cfg);
        let tokens = env.tantivy.tokenize_content(query)?;
        // Stream-scoped DF, mirroring the handler (Greptile PR#53 P1):
        // rarity is judged against the corpus the query searches.
        let rare = select_rare_tokens(&tokens, threshold, |t| {
            env.tantivy.doc_freq_content_in_stream(t, &case.stream)
        })?;
        if !rare.is_empty() {
            let rare_tokens: Vec<String> = rare.into_iter().map(|r| r.token).collect();
            let candidates =
                env.tantivy
                    .term_candidates(&rare_tokens, Some(&case.stream), cfg.candidate_cap)?;
            let existing: HashSet<String> = bm25.iter().map(|r| r.id.clone()).collect();
            for cand in candidates {
                guaranteed.insert(cand.id.clone());
                if !existing.contains(&cand.id) {
                    bm25.push(cand);
                }
            }
        }
    }

    let vector = vector_channel(case, env.top_k * 2);
    let fused = if guaranteed.is_empty() {
        env.engine
            .fuse_with_vector(bm25, vector, Some(&env.store))?
    } else {
        env.engine
            .fuse_with_vector_guaranteed(bm25, vector, Some(&env.store), Some(&guaranteed))?
    };

    Ok(RunOutput {
        needle_in_pool: fused.iter().any(|r| r.id == case.needle_id),
        ranked_ids: fused.into_iter().map(|r| r.id).collect(),
    })
}

fn recall_at(ranked: &[String], needle: &str, k: usize) -> bool {
    ranked.iter().take(k).any(|id| id == needle)
}

#[derive(Debug, Serialize, Clone, Copy)]
struct CategoryMetrics {
    cases: usize,
    r_at_5: f64,
    r_at_10: f64,
}

fn measure(
    env: &Env,
    cases: &[CaseSpec],
    diminutive: bool,
    lane: Option<&RareTermLaneConfig>,
) -> Result<CategoryMetrics> {
    let mut hit5 = 0usize;
    let mut hit10 = 0usize;
    for case in cases {
        let (full, dim, surname) = &case.person_b;
        let query = if diminutive {
            format!("{dim} {surname}")
        } else {
            format!("{full} {surname}")
        };
        let out = run_case(env, case, &query, lane)?;
        if recall_at(&out.ranked_ids, &case.needle_id, 5) {
            hit5 += 1;
        }
        if recall_at(&out.ranked_ids, &case.needle_id, 10) {
            hit10 += 1;
        }
    }
    let n = cases.len().max(1);
    Ok(CategoryMetrics {
        cases: cases.len(),
        r_at_5: hit5 as f64 / n as f64,
        r_at_10: hit10 as f64 / n as f64,
    })
}

#[derive(Debug, Serialize)]
struct BaselineReport {
    eval: &'static str,
    seed: u64,
    cases: usize,
    lane_off_full_name: CategoryMetrics,
    lane_off_diminutive: CategoryMetrics,
    lane_on_full_name: CategoryMetrics,
    lane_on_diminutive: CategoryMetrics,
}

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

const SEED: u64 = 0x0012_C0DE;
const N_CASES: usize = 50;

fn build_cases() -> Vec<CaseSpec> {
    let mut rng = Rng::new(SEED);
    (0..N_CASES).map(|i| build_case(i, &mut rng)).collect()
}

// ─── tests ───────────────────────────────────────────────────────────────────

/// D2: ≥50 deterministic cases, R@5/R@10 per category, lane OFF baseline +
/// lane ON, written back to fixtures. AC-2 determinism: a second OFF run
/// must produce identical rankings.
#[test]
fn buried_entity_probe_baseline_and_lane() -> Result<()> {
    let cases = build_cases();
    let env = build_env(&cases)?;
    let lane_on = lane_cfg_on();

    let off_full = measure(&env, &cases, false, None)?;
    let off_dim = measure(&env, &cases, true, None)?;
    let on_full = measure(&env, &cases, false, Some(&lane_on))?;
    let on_dim = measure(&env, &cases, true, Some(&lane_on))?;

    // Determinism: identical rankings on a repeat OFF run (same env).
    for case in cases.iter().take(10) {
        let (full, _, surname) = &case.person_b;
        let q = format!("{full} {surname}");
        let a = run_case(&env, case, &q, None)?;
        let b = run_case(&env, case, &q, None)?;
        assert_eq!(
            a.ranked_ids, b.ranked_ids,
            "non-deterministic ranking for {q}"
        );
    }

    // Lane mechanics: with the lane ON the needle must be in the fused pool
    // for the full-name query (the surname is rare by construction).
    for case in &cases {
        let (full, _, surname) = &case.person_b;
        let q = format!("{full} {surname}");
        let out = run_case(&env, case, &q, Some(&lane_on))?;
        assert!(
            out.needle_in_pool,
            "lane ON but needle {} missing from fused pool (query '{q}')",
            case.needle_id
        );
    }

    let report = BaselineReport {
        eval: "buried_entity_probe",
        seed: SEED,
        cases: N_CASES,
        lane_off_full_name: off_full,
        lane_off_diminutive: off_dim,
        lane_on_full_name: on_full,
        lane_on_diminutive: on_dim,
    };
    let out = fixture_dir().join("buried_entity_baseline.json");
    std::fs::write(&out, serde_json::to_string_pretty(&report)?)
        .with_context(|| format!("writing {}", out.display()))?;

    println!("── buried-entity probe ({N_CASES} cases, seed {SEED:#x}) ──");
    println!("cat            |  R@5 off |  R@5 on | R@10 off | R@10 on");
    println!(
        "full name      |    {:.2}  |   {:.2}  |    {:.2}  |   {:.2}",
        off_full.r_at_5, on_full.r_at_5, off_full.r_at_10, on_full.r_at_10
    );
    println!(
        "diminutive     |    {:.2}  |   {:.2}  |    {:.2}  |   {:.2}",
        off_dim.r_at_5, on_dim.r_at_5, off_dim.r_at_10, on_dim.r_at_10
    );
    println!("baseline written to {}", out.display());
    Ok(())
}

/// D1 evidence at the fusion stage: the incident fixture ("Celina Wrzosik"
/// buried in another person's profile) with per-channel tables for the three
/// incident queries. Assertions cover channel mechanics; the AC-3 verdict
/// (top-5 or not) is *printed* for the cycle results, not hard-asserted —
/// a REFUTED outcome is a valid cycle result (brief §5 AC-3).
#[test]
fn incident_fixture_per_channel_tables() -> Result<()> {
    let mut rng = Rng::new(SEED ^ 0xFEED);
    let mut case = build_case(0, &mut rng);
    // Fix the incident names for the printed tables (synthetic, per brief).
    case.person_b = (
        "Celina".to_string(),
        "Cela".to_string(),
        "Wrzosik".to_string(),
    );
    let needle_line_owner = case.needle_id.clone();
    for (id, content, _, _) in case.chunks.iter_mut() {
        if *id == needle_line_owner {
            *content = content.replace("Agnieszka", "Celina");
            // Replace the synthetic surname of case 0 with the fixed one.
            *content = content.replace(&surname_for_case(0), "Wrzosik");
        }
        if id.contains("fresh") {
            *content = content.replace("Agnieszka", "Celina");
        }
    }
    let env = build_env(std::slice::from_ref(&case))?;
    let lane_on = lane_cfg_on();

    for (label, query) in [
        ("(a) full name", "Celina Wrzosik".to_string()),
        ("(b) surname only", "Wrzosik".to_string()),
        ("(c) diminutive", "Cela Wrzosik".to_string()),
    ] {
        let bm25 = env
            .tantivy
            .search_with_stream(&query, &case.stream, env.top_k * 2)?;
        let vector = vector_channel(&case, env.top_k * 2);

        println!("\n── incident query {label}: '{query}' ──");
        let needle_bm25 = bm25.iter().position(|r| r.id == case.needle_id);
        println!("BM25 channel (top {}):", bm25.len().min(20));
        for (i, r) in bm25.iter().take(20).enumerate() {
            println!("  {:>2}. {}  score={:.3}", i + 1, r.id, r.score);
        }
        println!(
            "  needle rank in BM25: {}",
            needle_bm25.map_or("out of top-20".to_string(), |p| format!("{}", p + 1))
        );
        let needle_vec = vector.iter().position(|(id, _)| *id == case.needle_id);
        println!(
            "  needle rank in vector: {}",
            needle_vec.map_or("out of top-20".to_string(), |p| format!("{}", p + 1))
        );
        println!("  graph channel: n/a at fusion stage (server-side boost; entity graph empty in fixture)");

        let off = run_case(&env, &case, &query, None)?;
        let on = run_case(&env, &case, &query, Some(&lane_on))?;
        let off_rank = off.ranked_ids.iter().position(|id| *id == case.needle_id);
        let on_rank = on.ranked_ids.iter().position(|id| *id == case.needle_id);
        println!(
            "  fused OFF: needle rank {} / pool {}",
            off_rank.map_or("—".to_string(), |p| format!("{}", p + 1)),
            off.ranked_ids.len()
        );
        println!(
            "  fused ON:  needle rank {} / pool {} (in-pool: {})",
            on_rank.map_or("—".to_string(), |p| format!("{}", p + 1)),
            on.ranked_ids.len(),
            on.needle_in_pool
        );
        if label != "(c) diminutive" {
            println!(
                "  AC-3 verdict ({label}): needle in top-5 with lane ON = {}",
                on_rank.is_some_and(|p| p < 5)
            );
        }

        // Channel mechanics assertions (hypothesis gate, brief §4 krok 2):
        // for the surname-bearing queries the needle MUST be findable in the
        // BM25 channel — the surname exists only in the needle.
        if label != "(c) diminutive" {
            assert!(
                needle_bm25.is_some(),
                "needle absent from BM25 channel for {label} — rare surname must match"
            );
            assert!(
                on.needle_in_pool,
                "lane ON must keep the needle in the fused pool for {label}"
            );
        }
    }
    Ok(())
}

// ─── cycle/014 WS-1 tuning sweep (measurement, not an assertion) ────────────

/// Cycle/014, WS-1(a)+(b): sweep `vector_similarity_floor` × `decay_match_relief`
/// jointly — the floor reshapes the score distribution the relief then acts on,
/// so a per-knob sweep would not compose.
///
/// Marked `#[ignore]` on purpose: this test prints a grid and asserts nothing
/// beyond the baseline sanity check, so it must not slow down or destabilise
/// the normal `cargo test` run. Run it with:
///
/// ```console
/// cargo test -p loomem-core --test buried_entity_probe -- --ignored --nocapture ws1_floor_relief_grid
/// ```
///
/// A loop inside one test (rather than env-var-driven re-runs) is deliberate:
/// the fixture build — 50 streams × 35 chunks into Tantivy + RocksDB — is the
/// only expensive part, and it is identical for every grid point.
#[test]
#[ignore = "cycle/014 WS-1 measurement sweep; run with --ignored"]
fn ws1_floor_relief_grid() -> Result<()> {
    const FLOORS: [f64; 4] = [0.0, 0.70, 0.75, 0.80];
    const RELIEFS: [f64; 6] = [0.0, 0.3, 0.5, 0.7, 0.9, 1.0];

    let cases = build_cases();
    let mut env = build_env(&cases)?;
    let lane_on = lane_cfg_on();

    // How much of the (designed) vector channel each floor actually cuts —
    // without this the grid below is uninterpretable.
    for floor in FLOORS {
        let mut kept = 0usize;
        let mut total = 0usize;
        for case in &cases {
            let ch = vector_channel(case, env.top_k * 2);
            total += ch.len();
            kept += ch.iter().filter(|(_, s)| f64::from(*s) >= floor).count();
        }
        println!("WS1GRID floor {floor:.2}: vector candidates surviving {kept}/{total}");
    }

    println!("WS1GRID floor relief | full_r5 full_r10 | dim_r5 dim_r10 | full_r5_lane full_r10_lane | fresh_top5 stale_wins | needle rank | ms");
    for floor in FLOORS {
        for relief in RELIEFS {
            set_search_knobs(&mut env, floor, relief);
            let t0 = std::time::Instant::now();
            let full = measure(&env, &cases, false, None)?;
            let dim = measure(&env, &cases, true, None)?;
            let full_lane = measure(&env, &cases, false, Some(&lane_on))?;
            let fresh = measure_freshness(&env, &cases)?;
            let rank = measure_rank(&env, &cases)?;
            let ms = t0.elapsed().as_millis();
            println!(
                "WS1GRID {floor:.2} {relief:.2} | {:.2} {:.2} | {:.2} {:.2} | {:.2} {:.2} | {:.2} {:.2} | rank {:.2} found {} | {ms}",
                full.r_at_5,
                full.r_at_10,
                dim.r_at_5,
                dim.r_at_10,
                full_lane.r_at_5,
                full_lane.r_at_10,
                fresh.0,
                fresh.1,
                rank.0,
                rank.1
            );
            // Method gate: the (0.0, 0.0) corner must reproduce the cycle/012
            // shipped-default baseline, otherwise the sweep is measuring
            // something other than the probe it claims to.
            if floor == 0.0 && relief == 0.0 {
                assert!(
                    (full.r_at_5 - 0.0).abs() < f64::EPSILON && (full.r_at_10 - 0.44).abs() < 1e-9,
                    "baseline corner drifted: full-name R@5={:.2} R@10={:.2}, expected 0.00 / 0.44",
                    full.r_at_5,
                    full.r_at_10
                );
            }
        }
    }
    Ok(())
}

/// Cycle/014: the needle's mean rank for the full-name query, over the cases
/// where it is in the pool at all. R@5 alone is a knife-edge metric on this
/// fixture — each case has exactly four fresh distractors, so a needle sitting
/// immediately behind them scores R@5 = 1.00 while being one distractor away
/// from scoring 0. The mean rank says which of the two situations we are in.
/// Returns `(mean rank when found, cases where found)`.
fn measure_rank(env: &Env, cases: &[CaseSpec]) -> Result<(f64, usize)> {
    let mut sum = 0usize;
    let mut found = 0usize;
    for case in cases {
        let (full, _, surname) = &case.person_b;
        let out = run_case(env, case, &format!("{full} {surname}"), None)?;
        if let Some(p) = out.ranked_ids.iter().position(|id| *id == case.needle_id) {
            sum += p + 1;
            found += 1;
        }
    }
    let mean = if found == 0 {
        f64::NAN
    } else {
        sum as f64 / found as f64
    };
    Ok((mean, found))
}

/// Cycle/014 counter-metric for WS-1(b). `decay_match_relief` buys buried-fact
/// recall by cancelling the age penalty, so it must be paid for somewhere: the
/// same mechanism lets an old chunk climb over a fresh, genuinely-better one.
/// This probe queries the *fresh* person (C) by full name — the four fresh
/// chunks are the correct answers — and returns
/// `(mean fresh chunks in top-5 out of 4, fraction of cases where the stale L1
/// profile outranks every fresh chunk)`. The second number is the damage.
fn measure_freshness(env: &Env, cases: &[CaseSpec]) -> Result<(f64, f64)> {
    let mut fresh_in_top5 = 0usize;
    let mut stale_wins = 0usize;
    for (i, case) in cases.iter().enumerate() {
        let (first, _, _) = &case.person_b;
        let c_surname = COMMON_SURNAMES[(i + 1) % COMMON_SURNAMES.len()];
        let out = run_case(env, case, &format!("{first} {c_surname}"), None)?;
        fresh_in_top5 += out
            .ranked_ids
            .iter()
            .take(5)
            .filter(|id| id.contains("fresh"))
            .count();
        let needle_rank = out.ranked_ids.iter().position(|id| *id == case.needle_id);
        let best_fresh = out.ranked_ids.iter().position(|id| id.contains("fresh"));
        if let (Some(n), Some(f)) = (needle_rank, best_fresh) {
            if n < f {
                stale_wins += 1;
            }
        } else if needle_rank.is_some() && best_fresh.is_none() {
            stale_wins += 1;
        }
    }
    let n = cases.len().max(1);
    Ok((
        fresh_in_top5 as f64 / n as f64,
        stale_wins as f64 / n as f64,
    ))
}

/// Cycle/014, WS-1(a) calibration arm — the same grid, but with the designed
/// vector channel replaced by **real multilingual-e5-small cosines**.
///
/// Why this arm exists: `vector_similarity_floor` is an *absolute* threshold
/// on a raw cosine, while `build_case` hands the fusion stage designed
/// similarities in the 0.05–0.72 band. A floor tuned against designed numbers
/// says nothing about the value to ship, because the shipped local provider
/// (`embedding_provider = "local"`, multilingual-e5-small, embedded without
/// `query:`/`passage:` prefixes — see `local_embeddings.rs`) puts real cosines
/// in a much narrower, much higher band. This arm measures that band and
/// re-runs the sweep on it.
///
/// Skipped unless `LOOMEM_TEST_EMBED_MODEL` points at a model directory
/// (`model.onnx` + `tokenizer.json`), same convention as the multilingual
/// gate in `local_embeddings.rs`. Run in **release** — tract inference in a
/// debug build is an order of magnitude slower.
#[test]
#[ignore = "cycle/014 WS-1 real-embedding calibration; run with --ignored"]
fn ws1_floor_relief_grid_real_embeddings() -> Result<()> {
    let Ok(model_dir) = std::env::var("LOOMEM_TEST_EMBED_MODEL") else {
        eprintln!("skip: set LOOMEM_TEST_EMBED_MODEL=<dir> to run the calibration arm");
        return Ok(());
    };
    let embedder = loomem_core::local_embeddings::try_load(&model_dir, 384)
        .context("loading multilingual-e5-small")?;

    let cases = build_cases();
    let mut env = build_env(&cases)?;

    // Two embedding regimes, measured side by side:
    //   "prod"  — exactly what the shipped pipeline does: a chunk the local
    //             model refuses (>~512 tokens) simply has no embedding, so it
    //             is absent from the vector channel.
    //   "trunc" — the same corpus with content truncated to a length the model
    //             accepts, i.e. the counterfactual where every chunk *does*
    //             have a vector. Needed to separate the floor's effect from
    //             the effect of an unembeddable needle.
    const TRUNC_BYTES: usize = 2400;
    let t_embed = std::time::Instant::now();
    let mut prod_full = cases.clone();
    let mut prod_dim = cases.clone();
    let mut trunc_full = cases.clone();
    let mut trunc_dim = cases.clone();
    let mut embed_failures = 0usize;
    let mut needle_embed_failures = 0usize;
    let mut all_cosines: Vec<f64> = Vec::new();
    let mut needle_cosines: Vec<f64> = Vec::new();
    let mut fresh_cosines: Vec<f64> = Vec::new();
    for (idx, case) in cases.iter().enumerate() {
        let (full, dim, surname) = &case.person_b;
        let q_full = embedder.embed(&format!("{full} {surname}"))?;
        let q_dim = embedder.embed(&format!("{dim} {surname}"))?;

        let mut sims_prod_full = Vec::new();
        let mut sims_prod_dim = Vec::new();
        let mut sims_trunc_full = Vec::new();
        let mut sims_trunc_dim = Vec::new();
        for (id, content, _, _) in &case.chunks {
            match embedder.embed(content) {
                Ok(v) => {
                    let cf = f64::from(cosine_f32(&q_full, &v));
                    sims_prod_full.push((id.clone(), cf));
                    sims_prod_dim.push((id.clone(), f64::from(cosine_f32(&q_dim, &v))));
                    all_cosines.push(cf);
                    if *id == case.needle_id {
                        needle_cosines.push(cf);
                    } else if id.contains("fresh") {
                        fresh_cosines.push(cf);
                    }
                }
                Err(_) => {
                    embed_failures += 1;
                    if *id == case.needle_id {
                        needle_embed_failures += 1;
                    }
                }
            }
            let v = embed_truncated(&embedder, content, TRUNC_BYTES)?;
            sims_trunc_full.push((id.clone(), f64::from(cosine_f32(&q_full, &v))));
            sims_trunc_dim.push((id.clone(), f64::from(cosine_f32(&q_dim, &v))));
        }
        prod_full[idx].sims = sims_prod_full;
        prod_dim[idx].sims = sims_prod_dim;
        trunc_full[idx].sims = sims_trunc_full;
        trunc_dim[idx].sims = sims_trunc_dim;
    }
    let total_chunks: usize = cases.iter().map(|c| c.chunks.len()).sum();
    println!(
        "WS1REAL embedded {total_chunks} chunks ×2 regimes in {:.1}s",
        t_embed.elapsed().as_secs_f64()
    );
    println!(
        "WS1REAL local-embedder failures (prod regime): {embed_failures}/{total_chunks} chunks, \
         of which needles: {needle_embed_failures}/{}",
        cases.len()
    );
    print_quantiles("WS1REAL cosines all      ", &mut all_cosines);
    print_quantiles("WS1REAL cosines needle   ", &mut needle_cosines);
    print_quantiles("WS1REAL cosines fresh(C) ", &mut fresh_cosines);

    // Mandated grid + an extension into the band real cosines actually occupy.
    const FLOORS: [f64; 8] = [0.0, 0.70, 0.75, 0.80, 0.82, 0.84, 0.86, 0.88];
    const RELIEFS: [f64; 6] = [0.0, 0.3, 0.5, 0.7, 0.9, 1.0];
    for (label, cs_full, cs_dim) in [
        ("prod ", &prod_full, &prod_dim),
        ("trunc", &trunc_full, &trunc_dim),
    ] {
        // Same diagnostic as the synthetic grid: a recall number is only
        // interpretable next to how much of the vector channel the floor cut.
        for floor in FLOORS {
            let (mut kept, mut total, mut kept_fresh, mut total_fresh) = (0, 0, 0, 0);
            for case in cs_full {
                for (id, s) in &vector_channel(case, env.top_k * 2) {
                    total += 1;
                    let survives = f64::from(*s) >= floor;
                    if survives {
                        kept += 1;
                    }
                    if id.contains("fresh") {
                        total_fresh += 1;
                        if survives {
                            kept_fresh += 1;
                        }
                    }
                }
            }
            println!(
                "WS1REAL[{label}] floor {floor:.2}: vector candidates surviving {kept}/{total} \
                 (fresh distractors {kept_fresh}/{total_fresh})"
            );
        }
        println!(
            "WS1REAL[{label}] floor relief | full_r5 full_r10 | dim_r5 dim_r10 | needle rank | ms"
        );
        for floor in FLOORS {
            for relief in RELIEFS {
                set_search_knobs(&mut env, floor, relief);
                let t0 = std::time::Instant::now();
                let full = measure(&env, cs_full, false, None)?;
                let dim = measure(&env, cs_dim, true, None)?;
                let rank = measure_rank(&env, cs_full)?;
                println!(
                    "WS1REAL[{label}] {floor:.2} {relief:.2} | {:.2} {:.2} | {:.2} {:.2} | rank {:.2} found {} | {}",
                    full.r_at_5,
                    full.r_at_10,
                    dim.r_at_5,
                    dim.r_at_10,
                    rank.0,
                    rank.1,
                    t0.elapsed().as_millis()
                );
            }
        }
    }
    Ok(())
}

/// Embed `content`, shrinking it until the fixed-512-position local model
/// accepts it. Character budgets differ per language (Polish tokenizes into
/// more tokens per character than English), so the limit is found by retry
/// rather than assumed.
fn embed_truncated(
    embedder: &loomem_core::local_embeddings::LocalEmbedder,
    content: &str,
    start_bytes: usize,
) -> Result<Vec<f32>> {
    let mut budget = start_bytes;
    loop {
        let cut = content
            .char_indices()
            .map(|(i, _)| i)
            .take_while(|i| *i <= budget)
            .last()
            .unwrap_or(0);
        match embedder.embed(&content[..cut]) {
            Ok(v) => return Ok(v),
            Err(e) => {
                if budget <= 200 {
                    return Err(e).context("local embedder rejects even a 200-byte prefix");
                }
                budget = budget * 9 / 10;
            }
        }
    }
}

/// Cycle/014 side finding: how long a text the shipped local embedder accepts.
/// The buried-entity incident shape is a *long* consolidated profile, so the
/// answer decides whether such a chunk can have a vector representation at all.
#[test]
#[ignore = "cycle/014 local-embedder length probe; run with --ignored"]
fn ws1_local_embed_length_limit() -> Result<()> {
    let Ok(model_dir) = std::env::var("LOOMEM_TEST_EMBED_MODEL") else {
        eprintln!("skip: set LOOMEM_TEST_EMBED_MODEL=<dir> to run the length probe");
        return Ok(());
    };
    let embedder = loomem_core::local_embeddings::try_load(&model_dir, 384)
        .context("loading multilingual-e5-small")?;
    let unit = "Sprint review covered the payment gateway migration. ";
    let mut last_ok = 0usize;
    let mut first_err: Option<(usize, String)> = None;
    for reps in 1..=200 {
        let text = unit.repeat(reps);
        match embedder.embed(&text) {
            Ok(_) => last_ok = text.len(),
            Err(e) => {
                first_err = Some((text.len(), format!("{e}")));
                break;
            }
        }
    }
    println!("WS1EMBED longest accepted text: {last_ok} chars");
    match first_err {
        Some((len, e)) => println!("WS1EMBED first failure at {len} chars: {e}"),
        None => println!("WS1EMBED no failure up to 200 repetitions"),
    }
    Ok(())
}

/// Cosine of two L2-normalized embeddings (the local embedder normalizes).
fn cosine_f32(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

fn print_quantiles(label: &str, xs: &mut Vec<f64>) {
    if xs.is_empty() {
        println!("{label}: (empty)");
        return;
    }
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let q = |p: f64| -> f64 {
        let i = (((xs.len() - 1) as f64) * p).round().max(0.0);
        xs[usize::try_from(i as u64).unwrap_or(0).min(xs.len() - 1)]
    };
    println!(
        "{label}: n={} min={:.4} p05={:.4} p25={:.4} p50={:.4} p75={:.4} p95={:.4} max={:.4}",
        xs.len(),
        q(0.0),
        q(0.05),
        q(0.25),
        q(0.50),
        q(0.75),
        q(0.95),
        q(1.0)
    );
}

/// Cycle/014, WS-1(c): how far a probe query gets through the Tier-1 gate.
///
/// The tier's orchestration lives in `loomem-server` (graph lookup + chunk
/// load), which this core-level probe cannot reach — but the *decision* half
/// is pure and lives here, so the question "would Tier-1 even fire on these
/// queries?" is answerable offline. Prints per-query-shape counts of the two
/// pure gates: `QueryType::Factual` + non-empty detected entities, and
/// non-empty attribute terms.
#[test]
#[ignore = "cycle/014 WS-1(c) gate probe; run with --ignored"]
fn ws1_tier1_gate_probe() -> Result<()> {
    use loomem_core::search::query_classifier::classify;
    use loomem_core::search::tier1::attribute_terms;

    let cases = build_cases();
    for diminutive in [false, true] {
        let (mut factual_with_entities, mut terms_non_empty) = (0usize, 0usize);
        let mut sample = String::new();
        for (i, case) in cases.iter().enumerate() {
            let (full, dim, surname) = &case.person_b;
            let query = if diminutive {
                format!("{dim} {surname}")
            } else {
                format!("{full} {surname}")
            };
            let classified = classify(&query);
            let gate_a = classified.query_type == loomem_core::search::QueryType::Factual
                && !classified.features.entities.is_empty();
            if gate_a {
                factual_with_entities += 1;
            }
            let terms = attribute_terms(&query, &classified.features.entities);
            if !terms.is_empty() {
                terms_non_empty += 1;
            }
            if i == 0 {
                sample = format!(
                    "query='{query}' type={:?} entities={:?} attr_terms={terms:?}",
                    classified.query_type, classified.features.entities
                );
            }
        }
        let label = if diminutive {
            "diminutive"
        } else {
            "full name"
        };
        println!(
            "WS1TIER1 {label}: factual+entities {factual_with_entities}/{}, attr_terms non-empty {terms_non_empty}/{}",
            cases.len(),
            cases.len()
        );
        println!("WS1TIER1 {label} sample: {sample}");
    }
    Ok(())
}

/// AC-4(i): lane OFF must be byte-identical to the pre-cycle path — the
/// public wrapper delegates with `guaranteed = None`, and an explicitly
/// empty guarantee set behaves the same.
#[test]
fn lane_off_identical_to_pre_cycle() -> Result<()> {
    let cases = build_cases();
    let env = build_env(&cases[..8])?;
    for case in &cases[..8] {
        let (full, _, surname) = &case.person_b;
        let query = format!("{full} {surname}");
        let bm25_a = env
            .tantivy
            .search_with_stream(&query, &case.stream, env.top_k * 2)?;
        let bm25_b = bm25_a.clone();
        let vec_a = vector_channel(case, env.top_k * 2);
        let vec_b = vec_a.clone();
        let off = env
            .engine
            .fuse_with_vector(bm25_a, vec_a, Some(&env.store))?;
        let empty: HashSet<String> = HashSet::new();
        let off_guar = env.engine.fuse_with_vector_guaranteed(
            bm25_b,
            vec_b,
            Some(&env.store),
            Some(&empty),
        )?;
        let ids_a: Vec<&str> = off.iter().map(|r| r.id.as_str()).collect();
        let ids_b: Vec<&str> = off_guar.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids_a, ids_b, "OFF path diverged for '{query}'");
    }
    Ok(())
}

/// Unit coverage for the cycle/012 TantivyIndex additions: `tokenize_content`
/// agrees with the index analyzer, `doc_freq_content` counts documents (not
/// occurrences), and `term_candidates` respects stream isolation + the cap.
#[test]
fn tantivy_df_and_candidates_basics() -> Result<()> {
    let temp = TempDir::new()?;
    let mut tantivy = TantivyIndex::open(temp.path().join("t"), &tantivy_config())?;
    let now = now_secs();
    let docs = [
        (
            "d1",
            "s1",
            "Wrzosik dołączyła do zespołu. Wrzosik prowadzi projekt.",
        ),
        (
            "d2",
            "s1",
            "Spotkanie zespołu o budżecie i planach na kwartał.",
        ),
        ("d3", "s2", "Wrzosik pojawia się też w drugim streamie."),
    ];
    for (id, stream, content) in docs {
        tantivy.index_document(TextDocument {
            id: id.to_string(),
            content: content.to_string(),
            user_id: "default".into(),
            app_id: "default".into(),
            level: 0,
            timestamp: now,
            stream: stream.to_string(),
            entities: None,
            relations: None,
            event_date: None,
            source_agent: None,
        })?;
    }
    tantivy.commit()?;

    // Tokenizer: lowercased terms, same pipeline as the index.
    let tokens = tantivy.tokenize_content("Wrzosik Projekt")?;
    assert_eq!(tokens, vec!["wrzosik".to_string(), "projekt".to_string()]);

    // DF counts documents, not term occurrences (d1 mentions it twice).
    assert_eq!(tantivy.doc_freq_content("wrzosik")?, 2);
    assert_eq!(tantivy.doc_freq_content("brak")?, 0);

    // Stream-scoped DF (Greptile PR#53 P1): per-stream posting intersection.
    assert_eq!(tantivy.doc_freq_content_in_stream("wrzosik", "s1")?, 1);
    assert_eq!(tantivy.doc_freq_content_in_stream("wrzosik", "s2")?, 1);
    assert_eq!(tantivy.doc_freq_content_in_stream("wrzosik", "s3")?, 0);
    assert_eq!(tantivy.doc_freq_content_in_stream("brak", "s1")?, 0);

    // Stream isolation + cap.
    let toks = vec!["wrzosik".to_string()];
    let s1 = tantivy.term_candidates(&toks, Some("s1"), 10)?;
    assert_eq!(s1.len(), 1);
    assert_eq!(s1[0].id, "d1");
    let all = tantivy.term_candidates(&toks, None, 10)?;
    assert_eq!(all.len(), 2);
    let capped = tantivy.term_candidates(&toks, None, 1)?;
    assert_eq!(capped.len(), 1);
    let none = tantivy.term_candidates(&[], None, 10)?;
    assert!(none.is_empty());
    Ok(())
}

/// AC-4(iii): p50/p95 of the fusion-stage search path on a ≥1000-chunk
/// corpus, lane OFF vs ON. Numbers are printed for the cycle results; the
/// hard assertion is deliberately loose (3×) to keep CI free of wall-clock
/// flakes — the ≤10% acceptance judgment happens in the cycle report.
#[test]
fn lane_latency_p50_p95() -> Result<()> {
    // One big stream: 1 needle + 1199 noise chunks.
    let mut rng = Rng::new(SEED ^ 0xBEEF);
    let mut case = build_case(3, &mut rng);
    case.stream = "latency".to_string();
    for k in 0..1165 {
        let id = format!("lat-noise{k:04}");
        let content = format!(
            "{} {}",
            NOISE_TOPICS[rng.below(NOISE_TOPICS.len())],
            NOISE_TOPICS[rng.below(NOISE_TOPICS.len())]
        );
        let age = 3 + i64::try_from(rng.below(40)).unwrap_or(0);
        case.chunks.push((id.clone(), content, age, 0));
        case.sims.push((id, rng.range_f64(0.05, 0.4)));
    }
    let env = build_env(std::slice::from_ref(&case))?;
    let lane_on = lane_cfg_on();
    let (full, _, surname) = case.person_b.clone();
    let query = format!("{full} {surname}");

    let time = |lane: Option<&RareTermLaneConfig>| -> Result<Vec<u128>> {
        let mut samples = Vec::with_capacity(40);
        for _ in 0..40 {
            let t0 = std::time::Instant::now();
            let _ = run_case(&env, &case, &query, lane)?;
            samples.push(t0.elapsed().as_micros());
        }
        samples.sort_unstable();
        Ok(samples)
    };
    let off = time(None)?;
    let on = time(Some(&lane_on))?;
    let p = |s: &[u128], q: f64| -> u128 {
        let idx = ((s.len() as f64 - 1.0) * q).round();
        let idx = usize::try_from(idx.max(0.0) as u64).unwrap_or(0);
        s[idx.min(s.len() - 1)]
    };
    let (off_p50, off_p95) = (p(&off, 0.50), p(&off, 0.95));
    let (on_p50, on_p95) = (p(&on, 0.50), p(&on, 0.95));
    println!("── lane latency, 1200-chunk stream, 40 runs (µs) ──");
    println!("lane OFF: p50={off_p50}  p95={off_p95}");
    println!("lane ON:  p50={on_p50}  p95={on_p95}");
    println!(
        "p95 delta: {:+.1}%",
        (on_p95 as f64 - off_p95 as f64) / (off_p95 as f64).max(1.0) * 100.0
    );
    assert!(
        on_p95 <= off_p95.saturating_mul(3).max(off_p95 + 2_000),
        "lane ON p95 ({on_p95}µs) blew past 3× OFF p95 ({off_p95}µs)"
    );
    Ok(())
}

// ─── cycle/015 KROK 3 — freshness probe on supersede pairs ──────────────────
//
// The cycle/014 counter-metric (`measure_freshness`) saturated in all 16 grid
// points (fresh_top5 = 4/4, stale_wins = 0) — not because relief is safe, but
// because that fixture cannot detect its cost: fresh chunks carry the full
// surname, the needle does not, so BM25 separates them cleanly and decay has
// nothing to spoil. This fixture removes the shortcut: each case is a
// supersede-shaped PAIR — two chunks about the same person and the same fact,
// lexically near-identical, differing only in age and the fact's value. The
// question relief must answer before shipping: does cancelling the age
// penalty lift the STALE version of a fact above its FRESH replacement?

fn build_freshness_case(case: usize, rng: &mut Rng) -> CaseSpec {
    let (first, dim) = NAME_PAIRS[case % NAME_PAIRS.len()];
    let surname = surname_for_case(case);
    let stream = format!("fresh{case:03}");
    let stale_id = format!("f{case:03}-stale");
    let fresh_id = format!("f{case:03}-fresh");

    // Shared lexical frame; only the fact value and one trailing word differ.
    let value = (case * 7) % 1000;
    let stale_content = format!(
        "Numer telefonu do {first} {surname}: 601 234 {value:03}. \
         Notatka z rozmowy o dostepnosci."
    );
    let fresh_content = format!(
        "Numer telefonu do {first} {surname}: 698 111 {value:03}. \
         Notatka po aktualizacji danych."
    );

    let mut chunks: Vec<(String, String, i64, i32)> = vec![
        (stale_id.clone(), stale_content, 45, 0),
        (fresh_id.clone(), fresh_content, 1, 0),
    ];
    // Designed cosines: near-identical text -> near-identical vectors. The
    // stale copy gets a hair MORE similarity — the worst case for relief
    // (a strong old match is exactly what relief rewards).
    let mut sims: Vec<(String, f64)> = vec![(stale_id.clone(), 0.63), (fresh_id.clone(), 0.61)];
    for k in 0..8 {
        let id = format!("f{case:03}-noise{k:02}");
        let content = format!(
            "{} {}",
            NOISE_TOPICS[rng.below(NOISE_TOPICS.len())],
            NOISE_TOPICS[rng.below(NOISE_TOPICS.len())]
        );
        let age = 3 + i64::try_from(rng.below(28)).unwrap_or(0);
        chunks.push((id.clone(), content, age, 0));
        sims.push((id, rng.range_f64(0.05, 0.35)));
    }

    CaseSpec {
        stream,
        // `needle_id` doubles as the stale-chunk id for this fixture.
        needle_id: stale_id,
        person_b: (first.to_string(), dim.to_string(), surname),
        chunks,
        sims,
    }
}

struct SupersedeMetrics {
    fresh_top1: f64,
    stale_wins: f64,
    mean_gap: f64,
    missing_pairs: usize,
}

/// For each case: query the shared lexical core and compare the ranks of the
/// fresh and stale copies. `stale_wins` is the freshness COST of relief;
/// `mean_gap` = mean(rank_stale - rank_fresh), positive = fresh above stale.
fn measure_supersede(env: &Env, cases: &[CaseSpec]) -> Result<SupersedeMetrics> {
    let mut fresh_top1 = 0usize;
    let mut stale_wins = 0usize;
    let mut gaps: Vec<f64> = Vec::new();
    let mut missing_pairs = 0usize;
    for case in cases {
        let (first, _, surname) = &case.person_b;
        let out = run_case(env, case, &format!("telefon {first} {surname}"), None)?;
        let stale_id = &case.needle_id;
        let fresh_id = stale_id.replace("-stale", "-fresh");
        let stale_rank = out.ranked_ids.iter().position(|id| id == stale_id);
        let fresh_rank = out.ranked_ids.iter().position(|id| *id == fresh_id);
        match (stale_rank, fresh_rank) {
            (Some(s), Some(f)) => {
                if f == 0 {
                    fresh_top1 += 1;
                }
                if s < f {
                    stale_wins += 1;
                }
                let s_i64 = i64::try_from(s).unwrap_or(i64::MAX);
                let f_i64 = i64::try_from(f).unwrap_or(i64::MAX);
                // truncation intentional: ranks are tiny
                gaps.push((s_i64 - f_i64) as f64);
            }
            (Some(_), None) => {
                // fresh fell out of the pool entirely — worst freshness loss
                stale_wins += 1;
                missing_pairs += 1;
            }
            _ => missing_pairs += 1,
        }
    }
    let n = cases.len().max(1);
    let mean_gap = if gaps.is_empty() {
        0.0
    } else {
        gaps.iter().sum::<f64>() / gaps.len() as f64
    };
    Ok(SupersedeMetrics {
        fresh_top1: fresh_top1 as f64 / n as f64,
        stale_wins: stale_wins as f64 / n as f64,
        mean_gap,
        missing_pairs,
    })
}

fn k3_grid(env: &mut Env, cases: &[CaseSpec], label: &str) -> Result<()> {
    println!("K3[{label}] relief | fresh_top1 | stale_wins | mean_gap(stale-fresh) | missing");
    for relief in [0.0, 0.3, 0.5, 0.7, 0.9, 1.0] {
        set_search_knobs(env, 0.0, relief);
        let m = measure_supersede(env, cases)?;
        println!(
            "K3[{label}] {relief:.2} | {:.2} | {:.2} | {:+.2} | {}",
            m.fresh_top1, m.stale_wins, m.mean_gap, m.missing_pairs
        );
    }
    Ok(())
}

/// Cycle/015 KROK 3, designed-cosine arm: grid over relief with floor = 0.
/// Marked `#[ignore]` on purpose: prints a grid, asserts nothing.
#[test]
#[ignore = "cycle/015 KROK 3 freshness probe (designed cosines); run with --ignored"]
fn k3_freshness_supersede_grid() -> Result<()> {
    let mut rng = Rng(0xC015_0003);
    let cases: Vec<CaseSpec> = (0..50).map(|i| build_freshness_case(i, &mut rng)).collect();
    let mut env = build_env(&cases)?;
    k3_grid(&mut env, &cases, "designed")
}

/// Cycle/015 KROK 3, real-embedding arm: the same pairs with cosines from
/// multilingual-e5-small (every chunk here is short, so every chunk embeds —
/// this is the post-fix production regime by construction).
#[test]
#[ignore = "cycle/015 KROK 3 freshness probe (real embeddings); run with --ignored"]
fn k3_freshness_supersede_grid_real_embeddings() -> Result<()> {
    let Ok(model_dir) = std::env::var("LOOMEM_TEST_EMBED_MODEL") else {
        eprintln!("skip: set LOOMEM_TEST_EMBED_MODEL=<dir> to run the real-embedding arm");
        return Ok(());
    };
    let embedder = loomem_core::local_embeddings::try_load(&model_dir, 384)
        .context("loading multilingual-e5-small")?;

    let mut rng = Rng(0xC015_0003);
    let mut cases: Vec<CaseSpec> = (0..50).map(|i| build_freshness_case(i, &mut rng)).collect();

    let t = std::time::Instant::now();
    let mut pair_cos: Vec<(f64, f64)> = Vec::new();
    for case in &mut cases {
        let (first, _, surname) = &case.person_b;
        let q = embedder.embed(&format!("telefon {first} {surname}"))?;
        let mut sims = Vec::new();
        let mut stale_cos = 0.0_f64;
        let mut fresh_cos = 0.0_f64;
        for (id, content, _, _) in &case.chunks {
            let v = embedder.embed(content)?;
            let c = f64::from(cosine_f32(&q, &v));
            if id.ends_with("-stale") {
                stale_cos = c;
            } else if id.ends_with("-fresh") {
                fresh_cos = c;
            }
            sims.push((id.clone(), c));
        }
        pair_cos.push((stale_cos, fresh_cos));
        case.sims = sims;
    }
    let n_chunks: usize = cases.iter().map(|c| c.chunks.len()).sum();
    println!(
        "K3REAL embedded {n_chunks} chunks in {:.1}s",
        t.elapsed().as_secs_f64()
    );
    let mut deltas: Vec<f64> = pair_cos.iter().map(|(s, f)| s - f).collect();
    print_quantiles("K3REAL cosine(stale)-cosine(fresh)", &mut deltas);

    let mut env = build_env(&cases)?;
    k3_grid(&mut env, &cases, "real")
}

// ─── cycle/015 KROK 4 — path-2 measurement: vector weight vs entity queries ─
//
// The dense channel with e5-small does not differentiate on pure-entity
// queries (cosines live in a narrow 0.66–0.85 band), and an absolute floor is
// a channel on/off switch. Path 2 of KROK 4 asks: what happens if pure-entity
// queries simply get a LOWER vector weight in fusion? This grid measures the
// benefit on the buried-needle fixture and the freshness cost on the
// supersede-pair fixture — code changes only after these numbers.

fn set_weight_knobs(env: &mut Env, relief: f64, w_vector: f64) {
    let mut config = probe_config();
    config.search.vector_similarity_floor = 0.0;
    config.search.decay_match_relief = relief;
    config.search.hybrid_weights.vector = w_vector;
    config.search.hybrid_weights.bm25 = 1.0 - w_vector;
    env.top_k = config.search.top_k;
    env.engine = HybridSearchEngine::new(config);
}

#[test]
#[ignore = "cycle/015 KROK 4 path-2 measurement (real embeddings); run with --ignored"]
fn k4_entity_vector_weight_grid_real_embeddings() -> Result<()> {
    let Ok(model_dir) = std::env::var("LOOMEM_TEST_EMBED_MODEL") else {
        eprintln!("skip: set LOOMEM_TEST_EMBED_MODEL=<dir> to run the KROK 4 arm");
        return Ok(());
    };
    let embedder = loomem_core::local_embeddings::try_load(&model_dir, 384)
        .context("loading multilingual-e5-small")?;

    // Arm 1: buried-needle fixture, full-name entity query, real cosines,
    // post-fix regime (every chunk embeds).
    let cases = build_cases();
    let mut real = cases.clone();
    for (idx, case) in cases.iter().enumerate() {
        let (full, _, surname) = &case.person_b;
        let q = embedder.embed(&format!("{full} {surname}"))?;
        let mut sims = Vec::new();
        for (id, content, _, _) in &case.chunks {
            let v = embedder.embed(content)?;
            sims.push((id.clone(), f64::from(cosine_f32(&q, &v))));
        }
        real[idx].sims = sims;
    }
    let mut env = build_env(&real)?;
    println!("K4[needle] w_vec relief | full_r5 full_r10 | needle rank");
    for w in [0.6, 0.4, 0.3, 0.2, 0.1, 0.0] {
        for relief in [0.0, 0.7] {
            set_weight_knobs(&mut env, relief, w);
            let m = measure(&env, &real, false, None)?;
            let (rank, found) = measure_rank(&env, &real)?;
            println!(
                "K4[needle] {w:.1} {relief:.1} | {:.2} {:.2} | rank {rank:.2} found {found}",
                m.r_at_5, m.r_at_10
            );
        }
    }

    // Arm 2: freshness cost of lowered vector weight (supersede pairs).
    let mut rng = Rng(0xC015_0003);
    let mut fcases: Vec<CaseSpec> = (0..50).map(|i| build_freshness_case(i, &mut rng)).collect();
    for case in &mut fcases {
        let (first, _, surname) = &case.person_b;
        let q = embedder.embed(&format!("telefon {first} {surname}"))?;
        let mut sims = Vec::new();
        for (id, content, _, _) in &case.chunks {
            let v = embedder.embed(content)?;
            sims.push((id.clone(), f64::from(cosine_f32(&q, &v))));
        }
        case.sims = sims;
    }
    let mut fenv = build_env(&fcases)?;
    println!("K4[fresh] w_vec relief | fresh_top1 | stale_wins | mean_gap");
    for w in [0.6, 0.4, 0.3, 0.2, 0.1, 0.0] {
        for relief in [0.0, 0.7] {
            set_weight_knobs(&mut fenv, relief, w);
            let m = measure_supersede(&fenv, &fcases)?;
            println!(
                "K4[fresh] {w:.1} {relief:.1} | {:.2} | {:.2} | {:+.2}",
                m.fresh_top1, m.stale_wins, m.mean_gap
            );
        }
    }
    Ok(())
}
