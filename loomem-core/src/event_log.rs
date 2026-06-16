use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use tracing::{info, warn};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventLogConfig {
    pub enabled: bool,
    pub dir: String,
    pub max_size_mb: usize,
    pub max_files: usize,
}

impl Default for EventLogConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            dir: "events".to_string(),
            max_size_mb: 10,
            max_files: 30,
        }
    }
}

/// Event types for the Loomem telemetry system
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MemoryEvent {
    Search {
        query: String,
        stream_id: String,
        top_scores: Vec<f32>,
        latency_ms: u64,
        result_count: usize,
    },
    Store {
        content_len: usize,
        chunk_count: usize,
        stream_id: String,
        source: String,
    },
    Consolidation {
        input_count: usize,
        output_count: usize,
        dropped_ids: Vec<String>,
        cost_usd: f64,
    },
    CostEvent {
        tokens: u64,
        model: String,
        operation: String,
    },
    Association {
        query: String,
        mechanisms_used: Vec<String>,
        scores: Vec<f32>,
        surfaced_count: usize,
    },
    DreamCycle {
        stream_id: String,
        discoveries: usize,
        evictions: usize,
        latent_total: usize,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventEntry {
    pub timestamp: u64,
    pub event: MemoryEvent,
}

/// Channel-based event sender for the hot path (zero-blocking)
pub type EventSender = tokio::sync::mpsc::UnboundedSender<EventEntry>;
pub type EventReceiver = tokio::sync::mpsc::UnboundedReceiver<EventEntry>;

/// Create an event channel pair
pub fn event_channel() -> (EventSender, EventReceiver) {
    tokio::sync::mpsc::unbounded_channel()
}

/// Process-wide count of events dropped because the writer channel was closed
/// (receiver gone — the background writer task has stopped). /150b Gap 6: makes
/// the previously-silent loss observable without blocking the hot path.
static EMIT_DROPS: AtomicU64 = AtomicU64::new(0);

/// Number of `MemoryEvent`s dropped on `emit` because the writer channel was
/// closed. Non-zero means the event log is incomplete.
pub fn emit_drop_count() -> u64 {
    EMIT_DROPS.load(Ordering::Relaxed)
}

/// Convenience: emit an event. Send failure (writer task gone) is non-fatal —
/// the hot path is never blocked — but is counted and warned rather than
/// silently dropped (/150b Gap 6).
pub fn emit(tx: &EventSender, event: MemoryEvent) {
    let entry = EventEntry {
        timestamp: chrono::Utc::now().timestamp() as u64,
        event,
    };
    if tx.send(entry).is_err() {
        let n = EMIT_DROPS.fetch_add(1, Ordering::Relaxed) + 1;
        warn!("event_log emit dropped (writer channel closed); total dropped={n}");
    }
}

/// The EventLog writer — runs in a background task, receives events via channel
pub struct EventLog {
    dir: PathBuf,
    writer: BufWriter<File>,
    file_size: u64,
    max_size_bytes: u64,
    max_files: usize,
}

impl EventLog {
    pub fn open(data_dir: &Path, config: &EventLogConfig) -> Result<Self> {
        let dir = data_dir.join(&config.dir);
        fs::create_dir_all(&dir)
            .with_context(|| format!("Failed to create event log dir: {}", dir.display()))?;

        let log_path = dir.join("events.jsonl");
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .with_context(|| format!("Failed to open event log: {}", log_path.display()))?;

        let file_size = file.metadata().map(|m| m.len()).unwrap_or(0);

        info!(
            "Event log opened at {} (size={}B)",
            log_path.display(),
            file_size
        );

        Ok(Self {
            dir,
            writer: BufWriter::new(file),
            file_size,
            max_size_bytes: (config.max_size_mb as u64) * 1024 * 1024,
            max_files: config.max_files,
        })
    }

    /// Append a single event entry
    pub fn append(&mut self, entry: &EventEntry) -> Result<()> {
        let line = serde_json::to_string(entry)?;
        let bytes = line.as_bytes();
        self.writer.write_all(bytes)?;
        self.writer.write_all(b"\n")?;
        self.file_size += bytes.len() as u64 + 1;

        if self.file_size >= self.max_size_bytes {
            self.rotate()?;
        }
        Ok(())
    }

    /// Flush buffered writes
    pub fn flush(&mut self) -> Result<()> {
        self.writer.flush()?;
        Ok(())
    }

    /// Rotate: rename current to events.{N}.jsonl, open fresh file
    fn rotate(&mut self) -> Result<()> {
        self.writer.flush()?;

        // Find next rotation index
        let mut max_idx = 0u32;
        if let Ok(entries) = fs::read_dir(&self.dir) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if let Some(rest) = name.strip_prefix("events.") {
                    if let Some(idx_str) = rest.strip_suffix(".jsonl") {
                        if let Ok(idx) = idx_str.parse::<u32>() {
                            max_idx = max_idx.max(idx);
                        }
                    }
                }
            }
        }

        let new_name = format!("events.{}.jsonl", max_idx + 1);
        let current_path = self.dir.join("events.jsonl");
        let archive_path = self.dir.join(&new_name);
        fs::rename(&current_path, &archive_path)
            .with_context(|| format!("Failed to rotate event log to {}", archive_path.display()))?;

        // Open fresh file
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&current_path)
            .with_context(|| format!("Failed to open new event log: {}", current_path.display()))?;

        self.writer = BufWriter::new(file);
        self.file_size = 0;

        info!("Event log rotated to {}", new_name);

        // Clean up old files if over max_files
        self.cleanup_old_files()?;

        Ok(())
    }

    fn cleanup_old_files(&self) -> Result<()> {
        let mut archive_files: Vec<(u32, PathBuf)> = Vec::new();
        if let Ok(entries) = fs::read_dir(&self.dir) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name_str = name.to_string_lossy().to_string();
                if let Some(rest) = name_str.strip_prefix("events.") {
                    if let Some(idx_str) = rest.strip_suffix(".jsonl") {
                        if let Ok(idx) = idx_str.parse::<u32>() {
                            archive_files.push((idx, entry.path()));
                        }
                    }
                }
            }
        }

        if archive_files.len() > self.max_files {
            archive_files.sort_by_key(|(idx, _)| *idx);
            let to_remove = archive_files.len() - self.max_files;
            for (_, path) in archive_files.iter().take(to_remove) {
                if let Err(e) = fs::remove_file(path) {
                    warn!("Failed to remove old event log {}: {}", path.display(), e);
                }
            }
        }

        Ok(())
    }
}

/// Spawn the background event log writer task.
/// Returns the EventSender to use on hot paths.
pub fn spawn_writer(data_dir: &Path, config: &EventLogConfig) -> Result<EventSender> {
    let mut log = EventLog::open(data_dir, config)?;
    let (tx, mut rx) = event_channel();

    tokio::spawn(async move {
        let mut flush_interval = tokio::time::interval(std::time::Duration::from_secs(5));
        loop {
            tokio::select! {
                Some(entry) = rx.recv() => {
                    if let Err(e) = log.append(&entry) {
                        warn!("Failed to write event: {}", e);
                    }
                }
                _ = flush_interval.tick() => {
                    if let Err(e) = log.flush() {
                        warn!("Failed to flush event log: {}", e);
                    }
                }
            }
        }
    });

    Ok(tx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn emit_drop_increments_counter_on_closed_channel() {
        // /150b Gap 6: a send to a closed channel (writer task gone) must be
        // counted, not silently dropped. Counter is process-wide → assert delta.
        let (tx, rx) = event_channel();
        drop(rx); // close the channel so the next send fails
        let before = emit_drop_count();
        emit(
            &tx,
            MemoryEvent::Search {
                query: "x".into(),
                stream_id: "s".into(),
                top_scores: vec![],
                latency_ms: 0,
                result_count: 0,
            },
        );
        assert_eq!(
            emit_drop_count(),
            before + 1,
            "closed-channel emit must increment the drop counter"
        );
    }

    #[test]
    fn test_event_serialization_roundtrip() {
        let event = MemoryEvent::Search {
            query: "test query".into(),
            stream_id: "100".into(),
            top_scores: vec![0.9, 0.8, 0.7],
            latency_ms: 42,
            result_count: 3,
        };
        let entry = EventEntry {
            timestamp: 1234567890,
            event,
        };
        let json = serde_json::to_string(&entry).unwrap();
        let parsed: EventEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.timestamp, 1234567890);
    }

    #[test]
    fn test_event_log_append_and_rotation() {
        let tmp = TempDir::new().unwrap();
        let config = EventLogConfig {
            enabled: true,
            dir: "events".to_string(),
            max_size_mb: 0, // 0 MB = rotate immediately
            max_files: 2,
        };
        // Set max_size_bytes to very small for testing
        let mut log = EventLog::open(tmp.path(), &config).unwrap();
        // Override max_size_bytes for testing
        log.max_size_bytes = 100; // 100 bytes triggers rotation quickly

        for i in 0..10 {
            let entry = EventEntry {
                timestamp: 1000 + i,
                event: MemoryEvent::Store {
                    content_len: 100,
                    chunk_count: 1,
                    stream_id: "100".into(),
                    source: "test".into(),
                },
            };
            log.append(&entry).unwrap();
        }
        log.flush().unwrap();

        // Should have rotated - check archived files exist
        let events_dir = tmp.path().join("events");
        let entries: Vec<_> = fs::read_dir(&events_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert!(
            entries.len() > 1,
            "Expected rotation to create archive files"
        );
    }
}
