//! Cycle/112: feedback write-side — schema, storage layout, service layer.
//!
//! `POST /v1/feedback` accepts agent-supplied ratings for chunks they
//! retrieved. We collect the signal (Bayesian Beta posterior on chunk +
//! append-only event log) without changing retrieval in v1.

pub mod config;
pub mod event;
pub mod service;

#[cfg(test)]
mod tests;

pub use config::FeedbackConfig;
pub use event::{event_key, event_prefix_for_chunk, FeedbackEvent};
pub use service::{update_chunk_tally, ApplyRatingArgs, FeedbackService, RatingOutcome};
