//! Allocation and scan accounting shared by bounded lane stages.
use super::{E2LaneMetrics, RECORD_SCAN_BYTES_MAX, REQUEST_SCAN_BYTES_MAX, TEMPORARY_BYTES_MAX};
use loomem_core::storage::Chunk;
use loomem_core::SearchResult;

pub(crate) fn search_result_bytes(result: &SearchResult) -> usize {
    result
        .id
        .capacity()
        .saturating_add(result.content.capacity())
        .saturating_add(result.user_id.capacity())
        .saturating_add(result.app_id.capacity())
        .saturating_add(result.stream.capacity())
}

pub(crate) fn charge_hit(
    hit: &loomem_core::SearchResult,
    metrics: &mut E2LaneMetrics,
) -> Option<()> {
    if hit.content.len() > RECORD_SCAN_BYTES_MAX {
        return None;
    }
    let bytes = metrics.scanned_bytes.checked_add(hit.content.len())?;
    if bytes > REQUEST_SCAN_BYTES_MAX {
        return None;
    }
    metrics.scanned_bytes = bytes;
    metrics.scanned_candidates = metrics.scanned_candidates.saturating_add(1);
    Some(())
}

// Serialization into a counting sink bounds every metadata field without
// allocating a second copy of a potentially oversized record.
struct RecordBudget(usize);
impl std::io::Write for RecordBudget {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let size = self.0.saturating_add(bytes.len());
        if size > RECORD_SCAN_BYTES_MAX {
            return Err(std::io::Error::other("record budget"));
        }
        self.0 = size;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub(crate) fn charge_chunk(
    chunk: &Chunk,
    metrics: &mut E2LaneMetrics,
    bytes: &mut usize,
) -> Option<()> {
    let mut record = RecordBudget(0);
    serde_json::to_writer(&mut record, chunk).ok()?;
    let scanned = metrics.scanned_bytes.checked_add(record.0)?;
    if scanned > REQUEST_SCAN_BYTES_MAX {
        return None;
    }
    metrics.scanned_bytes = scanned;
    *bytes = bytes
        .saturating_add(record.0.saturating_mul(4))
        .saturating_add(std::mem::size_of::<Chunk>());
    if *bytes > TEMPORARY_BYTES_MAX {
        return None;
    }
    metrics.scan_temporary_bytes_upper_bound = *bytes;
    Some(())
}

pub(crate) fn hybrid_result_bytes(result: &loomem_core::HybridSearchResult) -> usize {
    result
        .id
        .capacity()
        .saturating_add(result.content.capacity())
        .saturating_add(result.user_id.capacity())
        .saturating_add(result.app_id.capacity())
}
