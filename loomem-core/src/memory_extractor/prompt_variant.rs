//! Extraction prompt variant (`[knowledge_extraction] prompt_variant`).
//!
//! Holdout r5 (2026-11) traced some lost answers to extraction itself: a stated
//! valuation never reached a fact, a reason was dropped from the fact it
//! explained, and the sense of a state change came out inverted. The
//! [`ExtractionPromptVariant::Relations`] variant prepends a RELATIONS AND
//! VALUES notice to the extraction system prompt asking the model to keep
//! causes, the direction of a change, and numeric values with their entity.
//!
//! The notice changes only what a fact's `content` should keep: the output
//! schema, the JSON contract and the reply parser are unchanged. With
//! [`ExtractionPromptVariant::Default`] the prompt bytes are identical to the
//! pre-option prompt (pinned by sha256 in the tests).

use serde::{Deserialize, Serialize};

/// Which extraction system prompt to send.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtractionPromptVariant {
    /// The built-in prompt, byte-identical to the pre-option prompt.
    #[default]
    Default,
    /// The built-in prompt with a RELATIONS AND VALUES notice prepended
    /// (changes LLM prompt bytes).
    Relations,
}

pub(super) const NOTICE: &str = "RELATIONS AND VALUES: Keep what connects and quantifies a fact. CAUSE: when the source gives a reason or a consequence (because, so, due to, since, as a result), keep it in the same fact as what it explains, e.g. \"The team postponed the launch to May because the security audit failed\", not just \"The team postponed the launch\". DIRECTION: when something changed, keep which way it went: from what to what, increased or decreased, started or stopped, came back or left, e.g. \"Tom's rent increased from $1,200 to $1,350\". Never invert or drop the direction of a change. VALUES: keep every stated number, amount, price, valuation, currency, quantity, percentage and date together with the entity it belongs to, including its unit and any qualifier such as \"about\" or \"at least\", e.g. \"The user's vintage guitar was appraised at about $4,500\". Never replace a stated value with a vaguer paraphrase. Only what each fact's content keeps changes; the output format stays the same.";

/// Prepend the relations notice to a prompt body under
/// [`ExtractionPromptVariant::Relations`]; the body is returned unchanged
/// (moved, not copied) under `Default`.
pub(super) fn with_notice(variant: ExtractionPromptVariant, body: String) -> String {
    match variant {
        ExtractionPromptVariant::Relations => format!("{NOTICE}\n\n{body}"),
        ExtractionPromptVariant::Default => body,
    }
}

#[cfg(test)]
mod tests;
