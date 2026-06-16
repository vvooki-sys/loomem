use serde::{Deserialize, Serialize};

/// Cycle/112: configuration for the `POST /v1/feedback` write-side endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeedbackConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default = "default_max_ratings")]
    pub max_ratings_per_request: usize,
    #[serde(default = "default_max_justification")]
    pub max_justification_chars: usize,
}

fn default_enabled() -> bool {
    true
}
fn default_max_ratings() -> usize {
    50
}
fn default_max_justification() -> usize {
    500
}

impl Default for FeedbackConfig {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            max_ratings_per_request: default_max_ratings(),
            max_justification_chars: default_max_justification(),
        }
    }
}
