//! cycle/010 Phase A — forgetting-correctness baseline eval.
//!
//! Measures the correctness of the three mutation paths through which Loomem
//! "forgets": **supersede** (contradiction version chain), **purge** (hard
//! delete across every retrieval surface) and **release** (retention expiry
//! of soft-deleted chunks). Cases are data-driven from
//! `tests/fixtures/forgetting_eval.json`; the measured baseline is written
//! back to `tests/fixtures/forgetting_baseline.json` (same write-back
//! convention as `se_regression.rs`).
//!
//! The purge cases mirror the cascade order used by the server handlers
//! (`delete.rs::delete_memory_fully`, `purge.rs::hard_delete_memory_fully`:
//! store → tantivy → graph) but exercise the loomem-core primitives directly
//! (`hard_delete_by_id`, `TantivyIndex::delete_document`,
//! `GraphStore::remove_chunk_references`) so the eval stays in-crate and
//! LLM/network-free. Deterministic: retention ages use day granularity
//! written directly into `deleted_at` — no sleeps, no clock mocking.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use loomem_core::config::{RocksDbConfig, TantivyConfig};
use loomem_core::contradiction::{apply_supersede, get_memory_chain};
use loomem_core::graph::GraphStore;
use loomem_core::storage::Chunk;
use loomem_core::{RocksDbStore, TantivyIndex, TextDocument};
use serde::{Deserialize, Serialize};
use tempfile::TempDir;

// ─── shared helpers (conventions from integration_test.rs) ──────────────────

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
        heap_size_mb: 16,
        drift_warn_pct: 5.0,
        auto_rebuild_on_drift: false,
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn base_chunk(id: &str, stream: &str, content: &str) -> Chunk {
    Chunk {
        id: id.to_string(),
        content: content.to_string(),
        stream: stream.to_string(),
        level: 0,
        score: 1.0,
        timestamp: now_secs(),
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
    }
}

fn make_doc(chunk: &Chunk) -> Result<TextDocument> {
    Ok(TextDocument {
        id: chunk.id.clone(),
        content: chunk.content.clone(),
        user_id: "test".to_string(),
        app_id: "test".to_string(),
        level: chunk.level,
        timestamp: i64::try_from(chunk.timestamp).context("timestamp fits i64")?,
        stream: chunk.stream.clone(),
        entities: None,
        relations: None,
        event_date: None,
        source_agent: None,
    })
}

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

// ─── fixture schema ──────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct Fixtures {
    retention_days: u64,
    supersede: Vec<SupersedeCase>,
    purge: Vec<PurgeCase>,
    release: Vec<ReleaseCase>,
}

#[derive(Debug, Deserialize)]
struct SupersedeCase {
    name: String,
    old_trust: Option<String>,
    new_trust: Option<String>,
    expect: String,
    #[serde(default)]
    chain_len: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct PurgeCase {
    name: String,
    soft_delete_first: bool,
    shared_entity: bool,
}

#[derive(Debug, Deserialize)]
struct ReleaseCase {
    name: String,
    deleted_age_days: Option<u64>,
    persistent: bool,
    superseded: bool,
    expect_collected: bool,
}

// ─── report schema (written to tests/fixtures/forgetting_baseline.json) ─────

#[derive(Debug, Serialize)]
struct Report {
    eval: &'static str,
    generated_at_unix: u64,
    retention_days: u64,
    supersede: PathReport,
    purge: PathReport,
    release: PathReport,
    purge_orphans: OrphanCounts,
}

#[derive(Debug, Serialize)]
struct PathReport {
    total: usize,
    passed: usize,
    cases: Vec<CaseResult>,
}

#[derive(Debug, Serialize)]
struct CaseResult {
    name: String,
    pass: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    violations: Vec<String>,
}

#[derive(Debug, Default, Serialize)]
struct OrphanCounts {
    rocksdb_chunk: u32,
    embedding: u32,
    entity_kv: u32,
    relation_kv: u32,
    graph_reverse_index: u32,
    graph_entity_refs: u32,
    tantivy_doc: u32,
}

impl OrphanCounts {
    fn total(&self) -> u32 {
        self.rocksdb_chunk
            + self.embedding
            + self.entity_kv
            + self.relation_kv
            + self.graph_reverse_index
            + self.graph_entity_refs
            + self.tantivy_doc
    }
}

fn path_report(cases: Vec<CaseResult>) -> PathReport {
    let passed = cases.iter().filter(|c| c.pass).count();
    PathReport {
        total: cases.len(),
        passed,
        cases,
    }
}

// ─── path 1: supersede (contradiction version chain) ────────────────────────

fn run_supersede_case(case: &SupersedeCase, idx: usize) -> Result<CaseResult> {
    let temp = TempDir::new()?;
    let store = RocksDbStore::open(temp.path().join("rocks"), &rocksdb_config())?;
    let stream = format!("s_sup_{idx}");

    let mut violations = Vec::new();
    if let Some(n) = case.chain_len {
        check_supersede_chain(&store, &stream, n, &mut violations)?;
    } else {
        check_supersede_pair(&store, &stream, case, &mut violations)?;
    }
    Ok(CaseResult {
        name: case.name.clone(),
        pass: violations.is_empty(),
        violations,
    })
}

fn check_supersede_pair(
    store: &RocksDbStore,
    stream: &str,
    case: &SupersedeCase,
    violations: &mut Vec<String>,
) -> Result<()> {
    let mut old = base_chunk("old", stream, "the user lives in Krakow");
    old.trust_level = case.old_trust.clone();
    store.store_chunk(&old)?;

    let mut new = base_chunk("new", stream, "the user lives in Gdansk");
    new.trust_level = case.new_trust.clone();
    let returned = apply_supersede(store, &old, new)?;
    store.store_chunk(&returned)?;

    let old_after = store
        .get_chunk(&old.id)?
        .context("old chunk must persist")?;
    let applied = returned.supersedes_id.as_deref() == Some(old.id.as_str());

    match case.expect.as_str() {
        "applied" => check_applied(&old, &old_after, &returned, violations),
        "guard_blocked" => check_blocked(&old_after, applied, violations),
        other => violations.push(format!("unknown expect value '{other}' in fixture")),
    }
    Ok(())
}

fn check_applied(old: &Chunk, old_after: &Chunk, returned: &Chunk, violations: &mut Vec<String>) {
    if returned.supersedes_id.as_deref() != Some(old.id.as_str()) {
        violations.push("expected supersede to apply, but supersedes_id is missing".into());
    }
    if old_after.is_latest {
        violations.push("old.is_latest must flip to false after supersede".into());
    }
    if old_after.superseded_by.as_deref() != Some(returned.id.as_str()) {
        violations.push("old.superseded_by must point at the new chunk".into());
    }
    if returned.version != old.version + 1 {
        violations.push(format!(
            "new.version must be old+1 (old={}, new={})",
            old.version, returned.version
        ));
    }
    if returned.root_memory_id.as_deref() != Some(old.id.as_str()) {
        violations.push("new.root_memory_id must anchor to the chain root".into());
    }
    if old_after.content != old.content {
        violations.push("history destroyed: superseded content was rewritten".into());
    }
}

fn check_blocked(old_after: &Chunk, applied: bool, violations: &mut Vec<String>) {
    if applied {
        violations.push("trust guard failed: lower-trust chunk superseded higher-trust".into());
    }
    if !old_after.is_latest {
        violations.push("guard block must leave old.is_latest untouched".into());
    }
    if old_after.superseded_by.is_some() {
        violations.push("guard block must not attach superseded_by to the old chunk".into());
    }
}

fn check_supersede_chain(
    store: &RocksDbStore,
    stream: &str,
    n: usize,
    violations: &mut Vec<String>,
) -> Result<()> {
    let mut prev = base_chunk("m0", stream, "version 0 of the fact");
    store.store_chunk(&prev)?;
    for i in 1..n {
        let new = base_chunk(
            &format!("m{i}"),
            stream,
            &format!("version {i} of the fact"),
        );
        let returned = apply_supersede(store, &prev, new)?;
        store.store_chunk(&returned)?;
        prev = returned;
    }

    let chain = get_memory_chain(store, "m0", n + 5)?;
    if chain.len() != n {
        violations.push(format!("chain length {} != expected {n}", chain.len()));
        return Ok(());
    }
    for (i, link) in chain.iter().enumerate() {
        if usize::try_from(link.version).ok() != Some(i + 1) {
            violations.push(format!(
                "link {i} has version {} != {}",
                link.version,
                i + 1
            ));
        }
        let is_last = i == n - 1;
        if link.is_latest != is_last {
            violations.push(format!(
                "link {i} is_latest={} (expected {is_last})",
                link.is_latest
            ));
        }
        if link.superseded_by.is_some() == is_last {
            violations.push(format!("link {i} superseded_by inconsistent with position"));
        }
        if i > 0 && link.root_memory_id.as_deref() != Some("m0") {
            violations.push(format!("link {i} lost root_memory_id anchor"));
        }
    }
    Ok(())
}

// ─── path 2: purge (hard delete across every retrieval surface) ─────────────

struct PurgeEnv {
    _temp: TempDir,
    store: Arc<RocksDbStore>,
    tantivy: TantivyIndex,
    graph: GraphStore,
}

fn purge_env() -> Result<PurgeEnv> {
    let temp = TempDir::new()?;
    let store = Arc::new(RocksDbStore::open(
        temp.path().join("rocks"),
        &rocksdb_config(),
    )?);
    let tantivy = TantivyIndex::open(temp.path().join("tantivy"), &tantivy_config())?;
    let graph = GraphStore::new(store.clone());
    Ok(PurgeEnv {
        _temp: temp,
        store,
        tantivy,
        graph,
    })
}

fn run_purge_case(case: &PurgeCase, idx: usize, orphans: &mut OrphanCounts) -> Result<CaseResult> {
    let mut env = purge_env()?;
    let stream = format!("s_purge_{idx}");
    let victim_id = format!("victim-{idx}");
    let marker = format!("qzsentinel{idx}");

    let victim = base_chunk(&victim_id, &stream, &format!("secret fact {marker}"));
    env.store.store_chunk(&victim)?;
    env.store
        .store_embedding(&victim_id, vec![0.25, 0.5, 0.25])?;
    env.tantivy.index_document(make_doc(&victim)?)?;
    env.tantivy.commit()?;
    let entity = env.graph.get_or_create_entity(
        &format!("Sentinel Topic {idx}"),
        "project",
        &[],
        &stream,
    )?;
    env.graph.add_chunk_to_entity(&entity.id, &victim_id)?;

    let survivor_id = format!("survivor-{idx}");
    if case.shared_entity {
        let survivor = base_chunk(&survivor_id, &stream, "unrelated surviving fact");
        env.store.store_chunk(&survivor)?;
        env.tantivy.index_document(make_doc(&survivor)?)?;
        env.tantivy.commit()?;
        env.graph.add_chunk_to_entity(&entity.id, &survivor_id)?;
    }

    if case.soft_delete_first {
        // memory_delete path first (delete.rs::delete_memory_fully order).
        env.store.delete_by_id(&victim_id)?;
        env.tantivy.delete_document(&victim_id)?;
        env.graph.remove_chunk_references(&victim_id)?;
    }

    // Purge cascade — same order as purge.rs::hard_delete_memory_fully.
    env.store.hard_delete_by_id(&victim_id)?;
    env.tantivy.delete_document(&victim_id)?;
    env.graph.remove_chunk_references(&victim_id)?;
    // delete_document commits the writer but does not reload the reader;
    // commit() does both. The eval measures residue, not reload latency.
    env.tantivy.commit()?;

    let mut violations = probe_residue(&env, &victim_id, &marker, orphans)?;
    if case.shared_entity {
        check_survivor(&env, &entity.id, &survivor_id, &victim_id, &mut violations)?;
    }
    Ok(CaseResult {
        name: case.name.clone(),
        pass: violations.is_empty(),
        violations,
    })
}

fn probe_residue(
    env: &PurgeEnv,
    id: &str,
    marker: &str,
    orphans: &mut OrphanCounts,
) -> Result<Vec<String>> {
    let mut violations = Vec::new();
    if env.store.get_chunk(id)?.is_some() {
        orphans.rocksdb_chunk += 1;
        violations.push("chunk survives in RocksDB after purge".into());
    }
    if env.store.get_embedding(id)?.is_some() {
        orphans.embedding += 1;
        violations.push("embedding survives in CF_EMBEDDINGS after purge".into());
    }
    if env.store.get(format!("entity:{id}").as_bytes())?.is_some() {
        orphans.entity_kv += 1;
        violations.push("entity:<id> kv survives after purge".into());
    }
    if env.store.get(format!("rel:{id}").as_bytes())?.is_some() {
        orphans.relation_kv += 1;
        violations.push("rel:<id> kv survives after purge".into());
    }
    if env
        .store
        .get(format!("graph:chunk:{id}").as_bytes())?
        .is_some()
    {
        orphans.graph_reverse_index += 1;
        violations.push("graph:chunk:<id> reverse index survives after purge".into());
    }
    if !env.graph.get_entities_for_chunk(id)?.is_empty() {
        orphans.graph_entity_refs += 1;
        violations.push("graph entities still reference the purged chunk".into());
    }
    if env.tantivy.search(marker, 10)?.iter().any(|h| h.id == id) {
        orphans.tantivy_doc += 1;
        violations.push("document still searchable in Tantivy after purge".into());
    }
    Ok(violations)
}

fn check_survivor(
    env: &PurgeEnv,
    entity_id: &str,
    survivor_id: &str,
    victim_id: &str,
    violations: &mut Vec<String>,
) -> Result<()> {
    match env.graph.get_entity_by_id(entity_id)? {
        Some(entity) => {
            if !entity.chunk_ids.iter().any(|c| c == survivor_id) {
                violations.push("purge over-deleted: survivor lost its entity reference".into());
            }
            if entity.chunk_ids.iter().any(|c| c == victim_id) {
                violations.push("purged chunk still listed in entity.chunk_ids".into());
            }
        }
        None => violations.push("purge over-deleted: shared entity was pruned".into()),
    }
    if env.store.get_chunk(survivor_id)?.is_none() {
        violations.push("purge over-deleted: survivor chunk vanished from RocksDB".into());
    }
    Ok(())
}

// ─── path 3: release (retention expiry of soft-deleted chunks) ──────────────

fn seed_release_chunk(
    store: &RocksDbStore,
    case: &ReleaseCase,
    id: &str,
    stream: &str,
    now: u64,
) -> Result<()> {
    let mut chunk = base_chunk(id, stream, &format!("release case {}", case.name));
    chunk.persistent = case.persistent;
    if case.superseded {
        chunk.is_latest = false;
        chunk.superseded_by = Some("some-newer-chunk".to_string());
    }
    if let Some(days) = case.deleted_age_days {
        chunk.deleted_at = Some(now.saturating_sub(days * 86400));
    }
    store.store_chunk(&chunk)
}

fn run_release_cases(cases: &[ReleaseCase], retention_days: u64) -> Result<Vec<CaseResult>> {
    let temp = TempDir::new()?;
    let store = RocksDbStore::open(temp.path().join("rocks"), &rocksdb_config())?;
    let stream = "s_release";
    let now = now_secs();

    for (i, case) in cases.iter().enumerate() {
        seed_release_chunk(&store, case, &format!("rel-{i}"), stream, now)?;
    }

    let collected: HashSet<String> = store
        .find_expired_soft_deleted(retention_days)?
        .into_iter()
        .collect();

    // Selection correctness per case.
    let mut results = Vec::new();
    for (i, case) in cases.iter().enumerate() {
        let id = format!("rel-{i}");
        let got = collected.contains(&id);
        let mut violations = Vec::new();
        if got != case.expect_collected {
            violations.push(format!(
                "retention selection: expected collected={}, got {got}",
                case.expect_collected
            ));
        }
        results.push(CaseResult {
            name: case.name.clone(),
            pass: violations.is_empty(),
            violations,
        });
    }

    // End-to-end: hard-purge exactly what the worker would purge, then verify
    // expired chunks are gone and still-valid facts survived.
    for id in &collected {
        store.hard_delete_by_id(id)?;
    }
    for (i, case) in cases.iter().enumerate() {
        let id = format!("rel-{i}");
        let present = store.get_chunk(&id)?.is_some();
        if case.expect_collected && present {
            results[i]
                .violations
                .push("post-purge: expired chunk still present".into());
        }
        if !case.expect_collected && !present {
            results[i]
                .violations
                .push("post-purge: retention deleted a still-valid fact".into());
        }
        results[i].pass = results[i].violations.is_empty();
    }
    Ok(results)
}

// ─── runner ──────────────────────────────────────────────────────────────────

#[test]
fn forgetting_correctness_baseline() -> Result<()> {
    let fixture_path = fixture_dir().join("forgetting_eval.json");
    let raw = std::fs::read_to_string(&fixture_path)
        .with_context(|| format!("read {}", fixture_path.display()))?;
    let fixtures: Fixtures = serde_json::from_str(&raw).context("parse forgetting_eval.json")?;

    let mut supersede_results = Vec::new();
    for (i, case) in fixtures.supersede.iter().enumerate() {
        supersede_results.push(run_supersede_case(case, i)?);
    }

    let mut orphans = OrphanCounts::default();
    let mut purge_results = Vec::new();
    for (i, case) in fixtures.purge.iter().enumerate() {
        purge_results.push(run_purge_case(case, i, &mut orphans)?);
    }

    let release_results = run_release_cases(&fixtures.release, fixtures.retention_days)?;

    let report = Report {
        eval: "forgetting_correctness_baseline",
        generated_at_unix: now_secs(),
        retention_days: fixtures.retention_days,
        supersede: path_report(supersede_results),
        purge: path_report(purge_results),
        release: path_report(release_results),
        purge_orphans: orphans,
    };

    let out = fixture_dir().join("forgetting_baseline.json");
    std::fs::write(&out, serde_json::to_string_pretty(&report)?)
        .with_context(|| format!("write {}", out.display()))?;
    println!(
        "forgetting baseline: supersede {}/{}, purge {}/{}, release {}/{} → {}",
        report.supersede.passed,
        report.supersede.total,
        report.purge.passed,
        report.purge.total,
        report.release.passed,
        report.release.total,
        out.display()
    );

    // AC-1: baseline measured for all three paths.
    assert!(
        report.supersede.total > 0 && report.purge.total > 0 && report.release.total > 0,
        "AC-1: fixtures must cover all three forgetting paths"
    );
    // AC-2: purge leaves no residue in any retrieval surface.
    assert_eq!(
        report.purge_orphans.total(),
        0,
        "AC-2: purge left residue: {:?} — details in {}",
        report.purge_orphans,
        out.display()
    );
    // Mechanical invariants of all three paths hold on the baseline.
    let failing: Vec<&str> = report
        .supersede
        .cases
        .iter()
        .chain(report.purge.cases.iter())
        .chain(report.release.cases.iter())
        .filter(|c| !c.pass)
        .map(|c| c.name.as_str())
        .collect();
    assert!(
        failing.is_empty(),
        "forgetting baseline violations in {failing:?} — details in {}",
        out.display()
    );
    Ok(())
}
