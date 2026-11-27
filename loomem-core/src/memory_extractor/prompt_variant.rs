//! Extraction prompt variant (`[knowledge_extraction] prompt_variant`).
//!
//! Holdout r5 (2026-11) traced some lost answers to extraction itself: a stated
//! valuation never reached a fact, a reason was dropped from the fact it
//! explained, and the sense of a state change came out inverted. The
//! [`ExtractionPromptVariant::Relations`] variant prepends a RELATIONS AND
//! VALUES notice to the extraction system prompt asking the model to keep
//! causes, the direction of a change, and numeric values with their entity.
//!
//! The offline diagnosis of the same holdout found two more relation kinds the
//! `relations` notice does not cover: the scope of a condition stated in the
//! next sentence ("in those sessions, only on mornings when…" was split into
//! two unrelated facts) and the attribution of a warning or proposal to a
//! named third party (who warned was lost). [`ExtractionPromptVariant::RelationsV2`]
//! is the `relations` notice followed by a SCOPE and a SPEAKER clause; the
//! `relations` notice itself is unchanged, byte for byte.
//!
//! The notices change only what a fact's `content` should keep: the output
//! schema, the JSON contract and the reply parser are unchanged. With
//! [`ExtractionPromptVariant::Default`] the prompt bytes are identical to the
//! pre-option prompt (pinned by sha256 in the tests, as are both notices).

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
    /// The `relations` notice followed by a SCOPE and a SPEAKER clause
    /// (changes LLM prompt bytes).
    RelationsV2,
}

/// The `relations` notice as one literal, so that [`NOTICE_V2`] can be built
/// from it at compile time and the shared prefix is `NOTICE` by construction.
macro_rules! relations_notice {
    () => {
        "RELATIONS AND VALUES: Keep what connects and quantifies a fact. CAUSE: when the source gives a reason or a consequence (because, so, due to, since, as a result), keep it in the same fact as what it explains, e.g. \"The team postponed the launch to May because the security audit failed\", not just \"The team postponed the launch\". DIRECTION: when something changed, keep which way it went: from what to what, increased or decreased, started or stopped, came back or left, e.g. \"Tom's rent increased from $1,200 to $1,350\". Never invert or drop the direction of a change. VALUES: keep every stated number, amount, price, valuation, currency, quantity, percentage and date together with the entity it belongs to, including its unit and any qualifier such as \"about\" or \"at least\", e.g. \"The user's vintage guitar was appraised at about $4,500\". Never replace a stated value with a vaguer paraphrase. Only what each fact's content keeps changes; the output format stays the same."
    };
}

pub(super) const NOTICE: &str = relations_notice!();

/// The two clauses `relations_v2` appends to the `relations` notice.
macro_rules! relations_v2_extra {
    () => {
        "SCOPE: when a sentence restricts or conditions the previous one (in those sessions, only then, within that plan, on those days, except when), keep the restriction in the same fact as what it restricts and name the referent explicitly instead of a pronoun or \"those\", e.g. \"In the two weekly pool sessions, the user swims intervals only on days without shoulder pain\", not two unrelated facts. SPEAKER: when a named person other than the user or the assistant warned, said, advised, suggested, proposed or promised something, keep that person as the subject of the fact together with what they said, e.g. \"Dana Reyes of Northgate Plumbing warned that the old boiler will fail within a year and suggested replacing it before winter\", not a bare list of contents. This does not change the attributed_to field, which stays user or assistant."
    };
}

/// The `relations_v2` notice: [`NOTICE`], one space, then the SCOPE and
/// SPEAKER clauses. Built with `concat!` so `NOTICE_V2.starts_with(NOTICE)`
/// holds by construction (and is asserted in the tests).
pub(super) const NOTICE_V2: &str = concat!(relations_notice!(), " ", relations_v2_extra!());

/// Prepend the variant's notice to a prompt body under
/// [`ExtractionPromptVariant::Relations`] and
/// [`ExtractionPromptVariant::RelationsV2`]; the body is returned unchanged
/// (moved, not copied) under `Default`.
pub(super) fn with_notice(variant: ExtractionPromptVariant, body: String) -> String {
    match variant {
        ExtractionPromptVariant::Relations => format!("{NOTICE}\n\n{body}"),
        ExtractionPromptVariant::RelationsV2 => format!("{NOTICE_V2}\n\n{body}"),
        ExtractionPromptVariant::Default => body,
    }
}

#[cfg(test)]
mod tests;
