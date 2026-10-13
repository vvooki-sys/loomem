use super::Chunk;

/// Filter before the scan budget so obsolete versions cannot starve live work.
pub(super) fn eligible(chunk: &Chunk, now: u64, min_age_secs: u64) -> bool {
    !chunk.consolidated
        && !chunk.in_progress
        && !chunk.dormant
        && chunk.is_latest
        && chunk.deleted_at.is_none()
        && now
            .checked_sub(chunk.timestamp)
            .is_some_and(|age| age >= min_age_secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn future_timestamps_are_not_eligible_even_with_zero_minimum_age() {
        let chunk: Chunk = serde_json::from_value(serde_json::json!({
            "id": "future", "content": "synthetic", "stream": "test", "level": 0,
            "score": 1.0, "timestamp": 101, "consolidated": false,
            "dormant": false, "in_progress": false
        }))
        .unwrap();
        assert!(!eligible(&chunk, 100, 0));
        assert!(eligible(&chunk, 101, 0));
    }
}
