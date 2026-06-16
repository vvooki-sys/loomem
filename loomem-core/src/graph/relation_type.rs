//! Relation type whitelist + normalization for edge creation (cycle/131).
//!
//! Implements D4/D5 decisions: 13-variant enum, unknown → RelatedTo + warn,
//! empty → RelatedTo silent. See brief §3 decisions D4–D5.

use tracing::warn;

/// Validated relation types for graph edges.
///
/// Based on live graph audit (2026-05-21). Unknown raw strings are mapped to
/// `RelatedTo` by `normalize_relation`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelationType {
    WorksAt,
    Uses,
    Manages,
    MemberOf,
    Implements,
    LocatedIn,
    RelatedTo,
    Created,
    LoggedInVia,
    PaysFor,
    DeployedIn,
    Accepted,
    ConfirmedParticipation,
}

impl RelationType {
    /// Snake_case string representation used as the edge `relation_type` label.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::WorksAt => "works_at",
            Self::Uses => "uses",
            Self::Manages => "manages",
            Self::MemberOf => "member_of",
            Self::Implements => "implements",
            Self::LocatedIn => "located_in",
            Self::RelatedTo => "related_to",
            Self::Created => "created",
            Self::LoggedInVia => "logged_in_via",
            Self::PaysFor => "pays_for",
            Self::DeployedIn => "deployed_in",
            Self::Accepted => "accepted",
            Self::ConfirmedParticipation => "confirmed_participation",
        }
    }
}

/// Normalize a raw extracted relation string into a known `RelationType`.
///
/// Matching is case-insensitive and whitespace-trimmed.
/// - Empty string → `RelatedTo` (no warn; treat as missing).
/// - Unknown non-empty string → `RelatedTo` + `tracing::warn!`.
pub fn normalize_relation(raw: &str) -> RelationType {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return RelationType::RelatedTo;
    }
    let lower = trimmed.to_lowercase();
    match lower.as_str() {
        "works_at" => RelationType::WorksAt,
        "uses" => RelationType::Uses,
        "manages" => RelationType::Manages,
        "member_of" => RelationType::MemberOf,
        "implements" => RelationType::Implements,
        "located_in" => RelationType::LocatedIn,
        "related_to" => RelationType::RelatedTo,
        "created" => RelationType::Created,
        "logged_in_via" => RelationType::LoggedInVia,
        "pays_for" => RelationType::PaysFor,
        "deployed_in" => RelationType::DeployedIn,
        "accepted" => RelationType::Accepted,
        "confirmed_participation" => RelationType::ConfirmedParticipation,
        _ => {
            warn!(raw_relation = %raw, "unknown relation type, mapped to related_to");
            RelationType::RelatedTo
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_known_lowercase() {
        assert_eq!(normalize_relation("works_at"), RelationType::WorksAt);
    }

    #[test]
    fn normalize_known_uppercase() {
        assert_eq!(normalize_relation("WORKS_AT"), RelationType::WorksAt);
    }

    #[test]
    fn normalize_unknown_falls_back() {
        assert_eq!(normalize_relation("works_with"), RelationType::RelatedTo);
    }

    #[test]
    fn normalize_empty_falls_back_silent() {
        // empty string → RelatedTo, no warn
        assert_eq!(normalize_relation(""), RelationType::RelatedTo);
    }

    #[test]
    fn normalize_trims_whitespace() {
        assert_eq!(normalize_relation("  works_at  "), RelationType::WorksAt);
    }
}
