use anyhow::{Context, Result};

use crate::storage::{Chunk, RocksDbStore};

use super::config::FeedbackConfig;
use super::event::{event_key, event_prefix_for_chunk, FeedbackEvent};

/// Cycle/112: result of applying one rating in a request.
/// Caller (handler) aggregates these into `accepted` count and `rejected` array.
#[derive(Debug)]
pub enum RatingOutcome {
    Accepted,
    Rejected { chunk_id: String, reason: String },
}

/// Cycle/112: service layer enforcing validation, scope, atomic write
/// of (Chunk aggregate update + FeedbackEvent append) per brief §6.
pub struct FeedbackService<'a> {
    pub store: &'a RocksDbStore,
    pub config: &'a FeedbackConfig,
}

/// Input arguments for `apply_rating`. Avoids large parameter lists.
pub struct ApplyRatingArgs<'a> {
    pub chunk_id: &'a str,
    pub usefulness: u8,
    pub harmful: bool,
    pub justification: &'a str,
    pub caller_stream: &'a str,
    pub caller_is_admin: bool,
    pub agent_id: &'a str,
    pub model_version: &'a str,
    pub prompt_version: &'a str,
    pub trajectory_id: Option<&'a str>,
    pub now_unix_ms: i64,
    pub event_id: &'a str,
}

impl<'a> FeedbackService<'a> {
    pub fn new(store: &'a RocksDbStore, config: &'a FeedbackConfig) -> Self {
        Self { store, config }
    }

    /// Validate one rating's payload independent of storage state. Errors are
    /// strings suitable for HTTP 400 response bodies.
    pub fn validate_rating(
        &self,
        usefulness: u8,
        harmful: bool,
        justification: &str,
    ) -> Result<(), String> {
        if usefulness > 4 {
            return Err(format!("usefulness {usefulness} out of range 0..=4"));
        }
        if justification.is_empty() {
            return Err("justification must not be empty".to_string());
        }
        if justification.chars().count() > self.config.max_justification_chars {
            return Err(format!(
                "justification exceeds max_justification_chars={}",
                self.config.max_justification_chars
            ));
        }
        if harmful && justification.trim().is_empty() {
            return Err("harmful=true requires non-empty justification".to_string());
        }
        Ok(())
    }

    /// Apply a single rating: validate, scope-check, compute updated chunk
    /// aggregate fields, append FeedbackEvent — atomically via WriteBatch.
    ///
    /// Returns `RatingOutcome::Accepted` on success, `Rejected` when the chunk
    /// does not exist in the caller's stream (or doesn't exist at all). Errors
    /// are reserved for actual storage failures.
    pub fn apply_rating(&self, args: ApplyRatingArgs<'_>) -> Result<RatingOutcome> {
        let chunk_opt = if args.caller_is_admin {
            self.store.get_chunk(args.chunk_id)?
        } else {
            self.store
                .get_chunk_scoped(args.chunk_id, args.caller_stream)?
        };
        let Some(mut chunk) = chunk_opt else {
            return Ok(RatingOutcome::Rejected {
                chunk_id: args.chunk_id.to_string(),
                reason: "not_found_in_stream".to_string(),
            });
        };

        update_chunk_tally(&mut chunk, args.usefulness, args.harmful, args.now_unix_ms);

        let event = FeedbackEvent {
            event_id: args.event_id.to_string(),
            chunk_id: args.chunk_id.to_string(),
            stream: chunk.stream.clone(),
            agent_id: args.agent_id.to_string(),
            model_version: args.model_version.to_string(),
            prompt_version: args.prompt_version.to_string(),
            usefulness: args.usefulness,
            harmful: args.harmful,
            justification: args.justification.to_string(),
            trajectory_id: args.trajectory_id.map(str::to_string),
            rated_at: args.now_unix_ms,
        };

        write_batch_atomic(self.store, &chunk, &event)?;

        Ok(RatingOutcome::Accepted)
    }

    /// Iterate feedback events for a single chunk via key-prefix scan.
    pub fn query_events_for_chunk(&self, chunk_id: &str) -> Result<Vec<FeedbackEvent>> {
        let prefix = event_prefix_for_chunk(chunk_id);
        let mut out = Vec::new();
        for (_k, v) in self.store.prefix_scan(prefix.as_bytes()) {
            let ev: FeedbackEvent =
                serde_json::from_slice(&v).context("deserialize FeedbackEvent")?;
            out.push(ev);
        }
        Ok(out)
    }
}

/// Apply the Beta update rule + counters to the chunk in-memory.
/// Pure function for unit-testing the math without touching storage.
pub fn update_chunk_tally(chunk: &mut Chunk, usefulness: u8, harmful: bool, rated_at_ms: i64) {
    let u = f64::from(usefulness);
    chunk.alpha += u / 4.0;
    chunk.beta += (4.0 - u) / 4.0;
    if harmful {
        chunk.beta += 4.0;
        chunk.harmful_count = chunk.harmful_count.saturating_add(1);
    }
    chunk.n_ratings = chunk.n_ratings.saturating_add(1);
    chunk.last_rated_at = Some(rated_at_ms);
}

/// Atomically persist updated chunk + new feedback event via RocksDB `WriteBatch`.
/// Both writes succeed or none — protects against partial state.
/// Chunk bytes MUST come from `encode_chunk` (same envelope as `store_chunk`);
/// raw `serde_json::to_vec(chunk)` here rewrote encrypted rows back to
/// plaintext when field-level encryption was enabled (/157 finding 1).
fn write_batch_atomic(store: &RocksDbStore, chunk: &Chunk, event: &FeedbackEvent) -> Result<()> {
    let chunk_key = format!("chunk:L{}:{}", chunk.level, chunk.id);
    let chunk_value = store
        .encode_chunk(chunk)
        .context("encode Chunk for feedback update")?;
    let ev_key = event_key(&event.chunk_id, event.rated_at, &event.event_id);
    let ev_value = serde_json::to_vec(event).context("serialize FeedbackEvent")?;

    let mut batch = rocksdb::WriteBatch::default();
    batch.put(chunk_key.as_bytes(), &chunk_value);
    batch.put(ev_key.as_bytes(), &ev_value);
    store
        .db()
        .write(batch)
        .context("rocksdb feedback write batch")?;
    Ok(())
}
