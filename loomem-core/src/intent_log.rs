use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use tracing::{debug, info, warn};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntentLogConfig {
    pub enabled: bool,
    pub dir: String,
    pub max_size_mb: usize,
    pub sync_on_write: bool,
    /// Hard age cap for archived `wal_<ts>_<rot>.log` files. Archives older
    /// than this are removed by `gc_old_logs` regardless of pending status —
    /// a pending entry that hasn't been committed in N days is effectively
    /// a lost write that no recovery will replay anyway. Set to 0 to disable
    /// age-based retention (committed-only GC remains active).
    #[serde(default = "default_archive_max_age_days")]
    pub archive_max_age_days: u64,
}

fn default_archive_max_age_days() -> u64 {
    7
}

impl Default for IntentLogConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            dir: "wal".to_string(),
            max_size_mb: 10,
            sync_on_write: false,
            archive_max_age_days: default_archive_max_age_days(),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum OpType {
    Store,
    Delete,
    Purge,
    /// Single-chunk hard-delete (cycle/135). Distinct from `Purge` which holds
    /// a stream id and replays as `purge_namespace`. `entry.id` is the chunk
    /// UUID; replay calls `store.hard_delete_by_id` for idempotent re-execution.
    PurgeChunk,
    Consolidate,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum OpStatus {
    Pending,
    Committed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntentEntry {
    pub seq: u64,
    pub ts: u64,
    pub op: OpType,
    pub id: String,
    pub status: OpStatus,
}

pub struct IntentLog {
    dir: PathBuf,
    file: File,
    file_size: u64,
    next_seq: AtomicU64,
    rotate_counter: AtomicU64,
    max_size_bytes: u64,
    sync_on_write: bool,
    archive_max_age_ms: u64,
}

impl IntentLog {
    pub fn open(data_dir: &Path, config: &IntentLogConfig) -> Result<Self> {
        let dir = data_dir.join(&config.dir);
        fs::create_dir_all(&dir)
            .with_context(|| format!("Failed to create intent log dir: {}", dir.display()))?;

        // Restrict permissions: owner-only (chmod 700)
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o700);
            let _ = fs::set_permissions(&dir, perms);
        }

        let log_path = dir.join("current.log");
        let existing_seq = Self::scan_max_seq(&log_path).unwrap_or(0);

        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .with_context(|| format!("Failed to open intent log: {}", log_path.display()))?;

        let file_size = file.metadata().map(|m| m.len()).unwrap_or(0);

        info!(
            "Intent log opened at {} (size={}B, next_seq={})",
            log_path.display(),
            file_size,
            existing_seq + 1
        );

        Ok(Self {
            dir,
            file,
            file_size,
            next_seq: AtomicU64::new(existing_seq + 1),
            rotate_counter: AtomicU64::new(0),
            max_size_bytes: (config.max_size_mb as u64) * 1024 * 1024,
            sync_on_write: config.sync_on_write,
            archive_max_age_ms: config
                .archive_max_age_days
                .saturating_mul(24 * 60 * 60 * 1000),
        })
    }

    /// Append a pending intent. Returns seq_id for later commit.
    pub fn append_pending(&mut self, op: OpType, chunk_id: &str) -> Result<u64> {
        let seq = self.next_seq.fetch_add(1, Ordering::SeqCst);
        let entry = IntentEntry {
            seq,
            ts: now_millis(),
            op,
            id: chunk_id.to_string(),
            status: OpStatus::Pending,
        };
        self.write_entry(&entry)?;
        Ok(seq)
    }

    /// Mark a previously pending operation as committed.
    pub fn mark_committed(&mut self, seq_id: u64, op: OpType, chunk_id: &str) -> Result<()> {
        let entry = IntentEntry {
            seq: seq_id,
            ts: now_millis(),
            op,
            id: chunk_id.to_string(),
            status: OpStatus::Committed,
        };
        self.write_entry(&entry)
    }

    /// Scan all log files for pending entries without matching committed marker.
    pub fn scan_pending(&self) -> Result<Vec<IntentEntry>> {
        let mut pending = std::collections::HashMap::<u64, IntentEntry>::new();

        // Scan archived logs first (oldest to newest)
        let mut archives: Vec<_> = self.archived_logs()?;
        archives.sort();

        for path in &archives {
            Self::scan_file(path, &mut pending)?;
        }

        // Then scan current log
        let current = self.dir.join("current.log");
        if current.exists() {
            Self::scan_file(&current, &mut pending)?;
        }

        let result: Vec<_> = pending.into_values().collect();
        Ok(result)
    }

    /// Remove old archive logs. Two paths:
    /// 1. Fully-committed archives — deleted unconditionally (fast path).
    /// 2. Archives older than `archive_max_age_ms` — deleted even if some
    ///    pending entries remain. A pending entry that has not been committed
    ///    in N days is a lost write; no recovery scan will repair it. The
    ///    count of pending entries dropped is logged at WARN.
    pub fn gc_old_logs(&self) -> Result<usize> {
        let archives = self.archived_logs()?;
        let mut removed = 0;
        let now_ms = now_millis();

        for path in &archives {
            let pending_count = count_pending_in_archive(path);
            let archive_age_ms = archive_age_ms(path, now_ms);
            let too_old = self.archive_max_age_ms > 0
                && archive_age_ms.is_some_and(|age| age > self.archive_max_age_ms);

            if pending_count == 0 {
                fs::remove_file(path)?;
                removed += 1;
            } else if too_old {
                warn!(
                    "Intent log GC: dropping aged archive {} ({} pending entries lost, age={}ms)",
                    path.display(),
                    pending_count,
                    archive_age_ms.unwrap_or(0),
                );
                fs::remove_file(path)?;
                removed += 1;
            }
        }

        if removed > 0 {
            info!("Intent log GC: removed {} old archive(s)", removed);
        }
        Ok(removed)
    }

    /// Flush user-space buffer and fsync to disk. Always fsyncs regardless
    /// of sync_on_write — this is for shutdown/recovery, not hot path.
    pub fn flush(&mut self) -> Result<()> {
        self.file.flush().context("Failed to flush intent log")?;
        self.file.sync_all().context("Failed to fsync intent log")?;
        Ok(())
    }

    fn write_entry(&mut self, entry: &IntentEntry) -> Result<()> {
        let mut line = serde_json::to_string(entry).context("Failed to serialize intent entry")?;
        line.push('\n');

        let bytes = line.as_bytes();
        self.file
            .write_all(bytes)
            .context("Failed to write intent entry")?;

        // flush() = push user-space buffer to kernel (cheap, ~0 cost on unbuffered File)
        // sync_all() = fsync to disk (expensive, ~0.1ms on SSD)
        // The PENDING entry MUST reach at least kernel buffer before we mutate backends.
        // With sync_on_write=true, we guarantee on-disk durability (survives power loss).
        // With sync_on_write=false, we survive kill -9 but not power loss.
        self.file.flush().context("Failed to flush intent entry")?;
        if self.sync_on_write {
            self.file.sync_all().context("Failed to fsync intent log")?;
        }

        self.file_size += bytes.len() as u64;
        self.maybe_rotate()?;

        Ok(())
    }

    fn maybe_rotate(&mut self) -> Result<()> {
        if self.file_size < self.max_size_bytes {
            return Ok(());
        }

        let ts = now_millis();
        let rot = self.rotate_counter.fetch_add(1, Ordering::SeqCst);
        let archive_name = format!("wal_{}_{}.log", ts, rot);
        let archive_path = self.dir.join(&archive_name);
        let current_path = self.dir.join("current.log");

        // Flush before rename
        self.file.flush()?;

        fs::rename(&current_path, &archive_path)
            .with_context(|| format!("Failed to rotate intent log to {}", archive_name))?;

        self.file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&current_path)
            .context("Failed to open new intent log after rotation")?;

        self.file_size = 0;
        info!("Intent log rotated → {}", archive_name);

        Ok(())
    }

    fn archived_logs(&self) -> Result<Vec<PathBuf>> {
        let mut logs = Vec::new();
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.starts_with("wal_") && name_str.ends_with(".log") {
                logs.push(entry.path());
            }
        }
        Ok(logs)
    }

    fn scan_file(
        path: &Path,
        pending: &mut std::collections::HashMap<u64, IntentEntry>,
    ) -> Result<()> {
        let file = File::open(path)
            .with_context(|| format!("Failed to open log file: {}", path.display()))?;

        for line in BufReader::new(file).lines() {
            let line = match line {
                Ok(l) => l,
                Err(e) => {
                    warn!("Skipping corrupt line in {}: {}", path.display(), e);
                    continue;
                }
            };
            if line.trim().is_empty() {
                continue;
            }

            match serde_json::from_str::<IntentEntry>(&line) {
                Ok(entry) => {
                    if entry.status == OpStatus::Pending {
                        pending.insert(entry.seq, entry);
                    } else if entry.status == OpStatus::Committed {
                        pending.remove(&entry.seq);
                    }
                }
                Err(e) => {
                    warn!("Skipping unparseable line in {}: {}", path.display(), e);
                }
            }
        }

        Ok(())
    }

    fn scan_max_seq(path: &Path) -> Result<u64> {
        if !path.exists() {
            return Ok(0);
        }

        let file = File::open(path)?;
        let mut max_seq: u64 = 0;

        for line in BufReader::new(file).lines() {
            let Ok(line) = line else { continue };
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(entry) = serde_json::from_str::<IntentEntry>(&line) {
                if entry.seq > max_seq {
                    max_seq = entry.seq;
                }
            }
        }

        Ok(max_seq)
    }
}

/// Replay a single chunk's Tantivy state from RocksDB. Used by recover()
/// for both Store and Consolidate replay paths.
///
/// Returns Ok(true) if the chunk existed in RocksDB and was re-indexed,
/// Ok(false) if the chunk was missing (caller increments report.skipped).
fn replay_chunk_to_tantivy(
    store: &crate::storage::RocksDbStore,
    tantivy: &mut crate::tantivy_index::TantivyIndex,
    chunk_id: &str,
) -> Result<bool> {
    match store.get_chunk(chunk_id)? {
        Some(chunk) => {
            let doc = build_recovery_text_doc(store, &chunk)?;
            tantivy.upsert_document(doc)?;
            Ok(true)
        }
        None => Ok(false),
    }
}

/// Build a Tantivy TextDocument for a chunk, fetching entities/relations from RocksDB.
/// Used by recover() for both Store and Consolidate replay paths.
fn build_recovery_text_doc(
    store: &crate::storage::RocksDbStore,
    chunk: &crate::storage::Chunk,
) -> Result<crate::tantivy_index::TextDocument> {
    let entities = store.get_entities(&chunk.id, &chunk.stream)?;
    let relations = store.get_relations(&chunk.id, &chunk.stream)?;

    let entity_text = entities.join(" ");
    let relation_text = relations
        .iter()
        .map(|(s, r, o)| format!("{} {} {}", s, r, o))
        .collect::<Vec<_>>()
        .join(" ");

    let event_date_ts: Option<i64> = chunk
        .extraction_meta
        .as_ref()
        .and_then(|m| m.event_date.as_ref())
        .and_then(|d| chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d").ok())
        .and_then(|d| d.and_hms_opt(12, 0, 0))
        .map(|dt| dt.and_utc().timestamp());

    Ok(crate::tantivy_index::TextDocument {
        id: chunk.id.clone(),
        content: chunk.content.clone(),
        user_id: String::new(),
        app_id: String::new(),
        level: chunk.level,
        stream: chunk.stream.clone(),
        timestamp: chunk.timestamp as i64,
        entities: Some(entity_text),
        relations: Some(relation_text),
        event_date: event_date_ts,
        source_agent: chunk.source.as_ref().map(|s| s.agent.clone()),
    })
}

/// Recovery: replay pending operations after crash.
pub fn recover(
    intent_log: &mut IntentLog,
    store: &crate::storage::RocksDbStore,
    tantivy: &mut crate::tantivy_index::TantivyIndex,
) -> Result<RecoveryReport> {
    let pending = intent_log.scan_pending()?;

    if pending.is_empty() {
        debug!("Intent log recovery: no pending entries");
        return Ok(RecoveryReport::default());
    }

    info!(
        "Intent log recovery: {} pending entries to process",
        pending.len()
    );

    let mut report = RecoveryReport::default();

    for entry in &pending {
        match entry.op {
            OpType::Store => {
                // RocksDB has it? Re-index in Tantivy. Doesn't have it? Skip.
                if replay_chunk_to_tantivy(store, tantivy, &entry.id)? {
                    info!("Recovery: re-indexed store op for chunk {}", entry.id);
                    report.replayed += 1;
                } else {
                    warn!(
                        "Recovery: pending store for {} but not in RocksDB, skipping",
                        entry.id
                    );
                    report.skipped += 1;
                }
            }
            OpType::Delete => {
                // Ensure chunk is gone from both stores
                if store.get_chunk(&entry.id)?.is_some() {
                    store.delete_by_id(&entry.id)?;
                    info!("Recovery: completed delete for chunk {}", entry.id);
                }
                tantivy.delete_document(&entry.id)?;
                report.replayed += 1;
            }
            OpType::Purge => {
                // entry.id holds the stream ID for purge ops
                let deleted = store.purge_namespace(&entry.id, false)?;
                for id in &deleted {
                    tantivy.delete_document(id)?;
                }
                info!(
                    "Recovery: replayed purge for stream {} ({} chunks)",
                    entry.id,
                    deleted.len()
                );
                report.replayed += 1;
            }
            OpType::PurgeChunk => {
                // cycle/135: single-chunk hard-delete replay. entry.id is the
                // chunk UUID. `hard_delete_by_id` cascades to RocksDB primary,
                // CF_EMBEDDINGS, entity, and relation keys. Tantivy delete is
                // idempotent — safe to issue regardless of store outcome so a
                // crash between store and tantivy cascade steps still converges.
                let found = store.hard_delete_by_id(&entry.id)?;
                tantivy.delete_document(&entry.id)?;
                if found {
                    info!("Recovery: replayed purge_chunk for {}", entry.id);
                } else {
                    debug!(
                        "Recovery: purge_chunk for {} no-op (already gone)",
                        entry.id
                    );
                }
                report.replayed += 1;
            }
            OpType::Consolidate => {
                // If L1 chunk doesn't exist, source chunks' in_progress flags
                // are already cleared by recover_orphaned_chunks
                if replay_chunk_to_tantivy(store, tantivy, &entry.id)? {
                    info!("Recovery: re-indexed consolidate op for chunk {}", entry.id);
                    report.replayed += 1;
                } else {
                    warn!(
                        "Recovery: pending consolidation for {} but L1 not in RocksDB, skipping",
                        entry.id
                    );
                    report.skipped += 1;
                }
            }
        }

        intent_log.mark_committed(entry.seq, entry.op, &entry.id)?;
    }

    tantivy.commit()?;

    info!(
        "Intent log recovery complete: {} replayed, {} skipped",
        report.replayed, report.skipped
    );
    Ok(report)
}

#[derive(Debug, Default)]
pub struct RecoveryReport {
    pub replayed: usize,
    pub skipped: usize,
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Count entries with `status: pending` in an archive file. Corrupt or
/// unreadable lines are skipped silently — `scan_file` already covers
/// recovery semantics, this is purely a GC heuristic.
fn count_pending_in_archive(path: &Path) -> usize {
    let Ok(file) = File::open(path) else {
        return 0;
    };
    let mut count = 0;
    for line in BufReader::new(file).lines() {
        let Ok(line) = line else { continue };
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(entry) = serde_json::from_str::<IntentEntry>(&line) {
            if entry.status == OpStatus::Pending {
                count += 1;
            }
        }
    }
    count
}

/// Extract the rotation timestamp encoded in `wal_<ts_millis>_<rot>.log` and
/// return the archive's age in ms. `None` if the filename does not match the
/// expected shape (e.g. a pre-existing or manually-renamed file).
fn archive_age_ms(path: &Path, now_ms: u64) -> Option<u64> {
    let stem = path.file_stem()?.to_str()?;
    let rest = stem.strip_prefix("wal_")?;
    let ts_str = rest.split('_').next()?;
    let ts: u64 = ts_str.parse().ok()?;
    Some(now_ms.saturating_sub(ts))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> IntentLogConfig {
        IntentLogConfig {
            enabled: true,
            dir: "wal".to_string(),
            max_size_mb: 10,
            sync_on_write: false,
            archive_max_age_days: 7,
        }
    }

    #[test]
    fn test_append_and_commit() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut log = IntentLog::open(tmp.path(), &test_config()).unwrap();

        let seq = log.append_pending(OpType::Store, "chunk-1").unwrap();
        assert_eq!(seq, 1);

        log.mark_committed(seq, OpType::Store, "chunk-1").unwrap();

        let pending = log.scan_pending().unwrap();
        assert!(pending.is_empty(), "All entries should be committed");
    }

    #[test]
    fn test_pending_detected() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut log = IntentLog::open(tmp.path(), &test_config()).unwrap();

        let seq1 = log.append_pending(OpType::Store, "chunk-1").unwrap();
        let _seq2 = log.append_pending(OpType::Delete, "chunk-2").unwrap();
        log.mark_committed(seq1, OpType::Store, "chunk-1").unwrap();

        let pending = log.scan_pending().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, "chunk-2");
        assert_eq!(pending[0].op, OpType::Delete);
    }

    #[test]
    fn test_rotation() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config = IntentLogConfig {
            enabled: true,
            dir: "wal".to_string(),
            max_size_mb: 0, // force rotation on every write (0 MB threshold)
            sync_on_write: false,
            archive_max_age_days: 7,
        };
        let mut log = IntentLog::open(tmp.path(), &config).unwrap();

        log.append_pending(OpType::Store, "chunk-1").unwrap();
        log.append_pending(OpType::Store, "chunk-2").unwrap();

        let archives = log.archived_logs().unwrap();
        assert!(
            !archives.is_empty(),
            "Should have at least one archived log"
        );
    }

    #[test]
    fn test_scan_across_rotation() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config = IntentLogConfig {
            enabled: true,
            dir: "wal".to_string(),
            max_size_mb: 0,
            sync_on_write: false,
            archive_max_age_days: 7,
        };
        let mut log = IntentLog::open(tmp.path(), &config).unwrap();

        let seq1 = log.append_pending(OpType::Store, "chunk-1").unwrap();
        let seq2 = log.append_pending(OpType::Delete, "chunk-2").unwrap();
        log.mark_committed(seq1, OpType::Store, "chunk-1").unwrap();

        // seq2 is still pending across rotation
        let pending = log.scan_pending().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].seq, seq2);
    }

    #[test]
    fn test_gc_old_logs() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config = IntentLogConfig {
            enabled: true,
            dir: "wal".to_string(),
            max_size_mb: 0,
            sync_on_write: false,
            archive_max_age_days: 7,
        };
        let mut log = IntentLog::open(tmp.path(), &config).unwrap();

        let seq1 = log.append_pending(OpType::Store, "chunk-1").unwrap();
        log.mark_committed(seq1, OpType::Store, "chunk-1").unwrap();

        // After rotation + GC, old fully-committed archives should be cleaned
        // Note: gc only removes archives, not current.log
        // Force one more write to push committed entries into archive
        let seq2 = log.append_pending(OpType::Store, "chunk-2").unwrap();
        log.mark_committed(seq2, OpType::Store, "chunk-2").unwrap();

        let _removed = log.gc_old_logs().unwrap();
        // Archives with only committed+pending pairs resolved should be cleaned
    }

    #[test]
    fn test_reopen_continues_seq() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config = test_config();

        {
            let mut log = IntentLog::open(tmp.path(), &config).unwrap();
            log.append_pending(OpType::Store, "chunk-1").unwrap();
            log.append_pending(OpType::Store, "chunk-2").unwrap();
        }

        // Reopen
        let mut log = IntentLog::open(tmp.path(), &config).unwrap();
        let seq = log.append_pending(OpType::Store, "chunk-3").unwrap();
        assert_eq!(seq, 3, "Should continue from last seq_id");
    }

    #[test]
    fn test_gc_drops_aged_archive_with_pending() {
        let tmp = tempfile::TempDir::new().unwrap();
        let wal_dir = tmp.path().join("wal");
        fs::create_dir_all(&wal_dir).unwrap();

        // Write an aged archive: filename ts is 30 days ago in millis.
        let ts_30d_ago = now_millis().saturating_sub(30 * 24 * 60 * 60 * 1000);
        let aged_archive = wal_dir.join(format!("wal_{}_0.log", ts_30d_ago));
        let mut f = File::create(&aged_archive).unwrap();
        // Pending without committed → would normally pin the archive forever.
        writeln!(
            f,
            r#"{{"seq":1,"ts":100,"op":"store","id":"chunk-1","status":"pending"}}"#
        )
        .unwrap();
        drop(f);

        // Write a fresh archive with same orphan-pending shape.
        let ts_now = now_millis();
        let fresh_archive = wal_dir.join(format!("wal_{}_0.log", ts_now));
        let mut f = File::create(&fresh_archive).unwrap();
        writeln!(
            f,
            r#"{{"seq":2,"ts":100,"op":"store","id":"chunk-2","status":"pending"}}"#
        )
        .unwrap();
        drop(f);

        // archive_max_age_days=7 → aged removed, fresh kept.
        let config = IntentLogConfig {
            archive_max_age_days: 7,
            ..test_config()
        };
        let log = IntentLog::open(tmp.path(), &config).unwrap();
        let removed = log.gc_old_logs().unwrap();
        assert_eq!(removed, 1);
        assert!(!aged_archive.exists());
        assert!(fresh_archive.exists());
    }

    #[test]
    fn test_gc_age_disabled_keeps_pending_archive() {
        let tmp = tempfile::TempDir::new().unwrap();
        let wal_dir = tmp.path().join("wal");
        fs::create_dir_all(&wal_dir).unwrap();

        let ts_30d_ago = now_millis().saturating_sub(30 * 24 * 60 * 60 * 1000);
        let aged_archive = wal_dir.join(format!("wal_{}_0.log", ts_30d_ago));
        let mut f = File::create(&aged_archive).unwrap();
        writeln!(
            f,
            r#"{{"seq":1,"ts":100,"op":"store","id":"chunk-1","status":"pending"}}"#
        )
        .unwrap();
        drop(f);

        // archive_max_age_days=0 disables the age path → committed-only GC.
        let config = IntentLogConfig {
            archive_max_age_days: 0,
            ..test_config()
        };
        let log = IntentLog::open(tmp.path(), &config).unwrap();
        let removed = log.gc_old_logs().unwrap();
        assert_eq!(removed, 0);
        assert!(aged_archive.exists());
    }

    #[test]
    fn test_corrupt_line_skipped() {
        let tmp = tempfile::TempDir::new().unwrap();
        let wal_dir = tmp.path().join("wal");
        fs::create_dir_all(&wal_dir).unwrap();

        // Write a valid entry, then garbage, then another valid entry
        let log_path = wal_dir.join("current.log");
        let mut f = File::create(&log_path).unwrap();
        writeln!(
            f,
            r#"{{"seq":1,"ts":100,"op":"store","id":"chunk-1","status":"pending"}}"#
        )
        .unwrap();
        writeln!(f, "GARBAGE LINE").unwrap();
        writeln!(
            f,
            r#"{{"seq":2,"ts":200,"op":"delete","id":"chunk-2","status":"pending"}}"#
        )
        .unwrap();
        drop(f);

        let log = IntentLog::open(tmp.path(), &test_config()).unwrap();
        let pending = log.scan_pending().unwrap();
        assert_eq!(
            pending.len(),
            2,
            "Should skip corrupt line and parse both valid entries"
        );
    }

    /// cycle/135: PurgeChunk variant round-trips through append/scan, distinct
    /// from `Purge`. Full replay coverage lives in
    /// `loomem-server/tests/purge_integration.rs` because it requires a real
    /// RocksDB + Tantivy.
    #[test]
    fn test_purge_chunk_round_trip() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut log = IntentLog::open(tmp.path(), &test_config()).unwrap();

        let seq = log
            .append_pending(OpType::PurgeChunk, "chunk-uuid-135")
            .unwrap();
        let pending = log.scan_pending().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].op, OpType::PurgeChunk);
        assert_eq!(pending[0].id, "chunk-uuid-135");

        log.mark_committed(seq, OpType::PurgeChunk, "chunk-uuid-135")
            .unwrap();
        let pending_after = log.scan_pending().unwrap();
        assert!(pending_after.is_empty());
    }
}
