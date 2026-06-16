//! Decode-failure accounting for chunk scans (/157 S2/S3).
//!
//! Incident B (2026-06-11): a boot scan over rows `decode_chunk` rejects
//! emitted one WARN per row (60 → 1268 lines between boots), drowning the
//! log and making a pipeline backlog indistinguishable from corruption.
//! Scans now feed failures into a [`ScanDecodeLog`] which logs each row at
//! `debug!`, classifies the failure stage, and emits at most TWO `warn!`
//! lines per scan (counts summary + first error with the FULL context chain
//! — the per-stage serde detail is the discriminator the incident lacked).

use serde::Serialize;
use tracing::{debug, info, warn};

/// Stage of `decode_chunk` at which a row failed, derived from the error
/// context chain. The matched strings are the `.context(...)` literals in
/// `storage.rs::decode_chunk` — keep both sites in sync.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DecodeFailStage {
    /// Envelope bytes are not a `StoredChunkRead` JSON document.
    Envelope,
    /// Decryption under the chunk's stream DEK failed (key/DEK mismatch).
    Decrypt,
    /// Decrypt succeeded but the payload is not the §D 3-tuple
    /// (e.g. /134 §C whole-blob era rows, or a foreign writer).
    Payload,
}

/// Classify a `decode_chunk` error by its context chain.
pub fn classify_decode_error(e: &anyhow::Error) -> DecodeFailStage {
    let chain = format!("{e:#}");
    if chain.contains("Failed to deserialize chunk envelope") {
        DecodeFailStage::Envelope
    } else if chain.contains("Failed to decrypt chunk payload") {
        DecodeFailStage::Decrypt
    } else {
        DecodeFailStage::Payload
    }
}

/// Serializable result of one scan's decode accounting — the source of the
/// `undecodable_chunks` status counter (S3).
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct ScanDecodeSummary {
    /// Scan label (e.g. `"get_all_chunks"`).
    pub scan: &'static str,
    /// Rows the scan saw (decodable or not).
    pub scanned: usize,
    /// Rows rejected by `decode_chunk` (sum of the three stages).
    pub undecodable: usize,
    pub envelope: usize,
    pub decrypt: usize,
    pub payload: usize,
    /// Full context chain of the first failure (truncated) — discriminates
    /// failure classes without per-row warns.
    pub first_error: Option<String>,
    /// Unix seconds when the scan finished.
    pub at_unix: u64,
}

/// Max chars of the first-error chain kept in the summary.
const FIRST_ERROR_CAP: usize = 300;

/// Label of the canonical full scan — the one boot scan whose clean result
/// must stay visible at `info!` (/159 AC-1). Shared with the
/// `get_all_chunks` call site in `storage.rs` so a rename breaks loudly at
/// compile time instead of silently demoting the line to `debug!`.
pub const CANONICAL_FULL_SCAN: &str = "get_all_chunks";

/// Whether a clean (0 undecodable) scan logs its summary at `info!`
/// (canonical full scan) or `debug!` (periodic tick scans, which would
/// otherwise emit thousands of routine INFO lines per day).
fn clean_scan_logs_at_info(scan: &str) -> bool {
    scan == CANONICAL_FULL_SCAN
}

/// Per-scan accumulator. Create at scan start, [`Self::record`] every decode
/// failure, [`Self::finish`] at scan end (emits the ≤2-line warn summary).
pub struct ScanDecodeLog {
    scan: &'static str,
    scanned: usize,
    envelope: usize,
    decrypt: usize,
    payload: usize,
    first_error: Option<String>,
}

impl ScanDecodeLog {
    pub fn new(scan: &'static str) -> Self {
        Self {
            scan,
            scanned: 0,
            envelope: 0,
            decrypt: 0,
            payload: 0,
            first_error: None,
        }
    }

    /// Count one scanned row (decodable or not).
    pub fn saw_row(&mut self) {
        self.scanned += 1;
    }

    /// Record one decode failure: classify, log at `debug!`, keep the first
    /// full error chain for the summary.
    pub fn record(&mut self, key: &[u8], e: &anyhow::Error) {
        let stage = classify_decode_error(e);
        match stage {
            DecodeFailStage::Envelope => self.envelope += 1,
            DecodeFailStage::Decrypt => self.decrypt += 1,
            DecodeFailStage::Payload => self.payload += 1,
        }
        debug!(
            "{}: undecodable row ({:?}) key={}: {:#}",
            self.scan,
            stage,
            String::from_utf8_lossy(key),
            e
        );
        if self.first_error.is_none() {
            let chain = format!("{e:#}");
            let capped: String = chain.chars().take(FIRST_ERROR_CAP).collect();
            self.first_error = Some(capped);
        }
    }

    pub fn undecodable(&self) -> usize {
        self.envelope + self.decrypt + self.payload
    }

    /// Emit the scan summary — ≤2 `warn!` lines when anything failed, one
    /// clean-scan line otherwise — and return the summary for status
    /// reporting. The clean line is `info!` only for the canonical full scan
    /// (`get_all_chunks`; /159 AC-1: a healthy boot scan must be visible as
    /// `0 of N chunk rows undecodable`, not silent). Periodic tick scans
    /// (scan_for_decay, list_active_streams, …) log the clean line at
    /// `debug!`, since per-tick INFO would otherwise emit thousands of
    /// routine lines per day.
    pub fn finish(self) -> ScanDecodeSummary {
        let undecodable = self.undecodable();
        if undecodable > 0 {
            warn!(
                "{}: {} of {} chunk rows undecodable (envelope-invalid: {}, decrypt-failed: {}, payload-not-tuple: {})",
                self.scan, undecodable, self.scanned, self.envelope, self.decrypt, self.payload
            );
            if let Some(ref first) = self.first_error {
                warn!("{}: first decode failure: {}", self.scan, first);
            }
        } else if clean_scan_logs_at_info(self.scan) {
            info!(
                "{}: 0 of {} chunk rows undecodable",
                self.scan, self.scanned
            );
        } else {
            debug!(
                "{}: 0 of {} chunk rows undecodable",
                self.scan, self.scanned
            );
        }
        ScanDecodeSummary {
            scan: self.scan,
            scanned: self.scanned,
            undecodable,
            envelope: self.envelope,
            decrypt: self.decrypt,
            payload: self.payload,
            first_error: self.first_error,
            at_unix: u64::try_from(chrono::Utc::now().timestamp()).unwrap_or(0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context;

    fn payload_err() -> anyhow::Error {
        serde_json::from_slice::<(String, u32)>(b"{\"a\":1}")
            .context("Failed to deserialize chunk payload")
            .expect_err("must fail")
    }

    fn envelope_err() -> anyhow::Error {
        serde_json::from_slice::<serde_json::Value>(b"\x00garbage")
            .context("Failed to deserialize chunk envelope")
            .expect_err("must fail")
    }

    fn decrypt_err() -> anyhow::Error {
        anyhow::anyhow!("ciphertext too short").context("Failed to decrypt chunk payload")
    }

    /// Stage classification matches the decode_chunk context literals.
    #[test]
    fn classifies_all_three_stages() {
        assert_eq!(
            classify_decode_error(&envelope_err()),
            DecodeFailStage::Envelope
        );
        assert_eq!(
            classify_decode_error(&decrypt_err()),
            DecodeFailStage::Decrypt
        );
        assert_eq!(
            classify_decode_error(&payload_err()),
            DecodeFailStage::Payload
        );
    }

    /// AC-4: summary carries per-stage counters and the first full error
    /// chain; warn output is structurally capped at 2 lines (the two `warn!`
    /// calls in `finish`), regardless of row count.
    #[test]
    fn summary_counts_and_first_error() {
        let mut log = ScanDecodeLog::new("test_scan");
        for _ in 0..5 {
            log.saw_row();
        }
        log.record(b"chunk:L0:a", &payload_err());
        log.record(b"chunk:L0:b", &payload_err());
        log.record(b"chunk:L0:c", &envelope_err());
        let summary = log.finish();
        assert_eq!(summary.scanned, 5);
        assert_eq!(summary.undecodable, 3);
        assert_eq!(summary.payload, 2);
        assert_eq!(summary.envelope, 1);
        assert_eq!(summary.decrypt, 0);
        let first = summary.first_error.expect("first error captured");
        assert!(
            first.contains("Failed to deserialize chunk payload"),
            "full chain kept: {first}"
        );
        assert!(
            first.contains("invalid type") || first.contains("expected"),
            "serde detail kept (the incident-B discriminator): {first}"
        );
    }

    /// Clean scans produce an empty summary and no first error.
    #[test]
    fn clean_scan_is_quiet() {
        let mut log = ScanDecodeLog::new("test_scan");
        log.saw_row();
        let summary = log.finish();
        assert_eq!(summary.undecodable, 0);
        assert_eq!(summary.first_error, None);
    }

    /// The clean-scan severity gate — only
    /// the canonical full scan logs clean results at `info!`; every other
    /// label (periodic tick scans) stays at `debug!`.
    #[test]
    fn clean_scan_severity_gate_branches() {
        assert!(clean_scan_logs_at_info(CANONICAL_FULL_SCAN));
        for periodic in [
            "scan_for_decay",
            "list_active_streams",
            "recover_orphaned_chunks",
            "purge_namespace",
            "test_scan",
        ] {
            assert!(
                !clean_scan_logs_at_info(periodic),
                "periodic scan {periodic} must log clean results at debug!"
            );
        }
    }

    /// Both branches of `finish` must produce an identical summary — the
    /// severity gate may only affect log level, never the returned data.
    #[test]
    fn finish_summary_identical_across_severity_branches() {
        let mut canonical = ScanDecodeLog::new(CANONICAL_FULL_SCAN);
        canonical.saw_row();
        let mut periodic = ScanDecodeLog::new("scan_for_decay");
        periodic.saw_row();
        let (c, p) = (canonical.finish(), periodic.finish());
        assert_eq!(c.scanned, p.scanned);
        assert_eq!(c.undecodable, p.undecodable);
        assert_eq!(
            (c.envelope, c.decrypt, c.payload),
            (p.envelope, p.decrypt, p.payload)
        );
        assert_eq!(c.first_error, p.first_error);
    }
}
