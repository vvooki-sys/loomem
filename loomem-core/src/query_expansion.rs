use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashSet;
use std::path::Path;

#[derive(Debug, Deserialize)]
struct SynonymsFile {
    synonyms: Vec<SynonymGroup>,
}

#[derive(Debug, Deserialize)]
struct SynonymGroup {
    terms: Vec<String>,
}

pub struct QueryExpander {
    synonym_groups: Vec<Vec<String>>,
}

impl QueryExpander {
    /// Load synonym groups from a TOML file
    pub fn load(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .context(format!("Failed to read synonyms file: {:?}", path))?;
        let file: SynonymsFile =
            toml::from_str(&content).context("Failed to parse synonyms TOML")?;

        let synonym_groups = file.synonyms.into_iter().map(|g| g.terms).collect();

        Ok(QueryExpander { synonym_groups })
    }

    /// Create an empty expander (no-op)
    pub fn empty() -> Self {
        QueryExpander {
            synonym_groups: Vec::new(),
        }
    }

    /// Expand query with synonyms
    /// Returns: original terms + matching synonyms
    /// Rules:
    /// - Case-insensitive matching
    /// - No duplicate terms
    /// - Max expansion: 3x original query length (prevent query explosion)
    ///   But with a minimum of 200 chars to allow synonym expansion on short queries
    pub fn expand(&self, query: &str) -> String {
        let original_len = query.len();
        let max_len = std::cmp::max(original_len * 3, 200);

        // Tokenize query (simple whitespace split)
        let query_lower = query.to_lowercase();
        let query_tokens: Vec<&str> = query_lower.split_whitespace().collect();

        // Track all terms (case-insensitive)
        let mut all_terms: HashSet<String> = HashSet::new();
        let mut result_terms: Vec<String> = Vec::new();

        // Add original terms first
        for token in query.split_whitespace() {
            let token_lower = token.to_lowercase();
            if all_terms.insert(token_lower.clone()) {
                result_terms.push(token.to_string());
            }
        }

        // Pre-stem query tokens for fuzzy synonym matching
        let query_stems: Vec<Vec<String>> = query_tokens.iter().map(|t| polish_stem(t)).collect();

        // Find matching synonym groups and add synonyms
        for group in &self.synonym_groups {
            // Check if any query token matches any term in this group
            let mut group_matches = false;
            for (ti, token) in query_tokens.iter().enumerate() {
                for term in group {
                    let term_lower = term.to_lowercase();
                    // Check if token matches the full term or any word in the term
                    if term_lower == *token {
                        group_matches = true;
                        break;
                    }
                    // Also check if any word in the multi-word term matches the token
                    for word in term_lower.split_whitespace() {
                        if word == *token {
                            group_matches = true;
                            break;
                        }
                        // Stem-aware matching: compare stems of token and synonym word
                        let word_stems = polish_stem(word);
                        for qs in &query_stems[ti] {
                            for ws in &word_stems {
                                if qs == ws && qs.len() >= 4 {
                                    group_matches = true;
                                    break;
                                }
                            }
                            if group_matches {
                                break;
                            }
                        }
                        if group_matches {
                            break;
                        }
                    }
                    if group_matches {
                        break;
                    }
                }
                if group_matches {
                    break;
                }
            }

            // If group matches, add all terms from group (except duplicates)
            if group_matches {
                for term in group {
                    // For multi-word terms, add each word separately to avoid duplicates
                    for word in term.split_whitespace() {
                        let word_lower = word.to_lowercase();
                        if all_terms.insert(word_lower.clone()) {
                            result_terms.push(word.to_string());

                            // Check if we've exceeded max length
                            let current_result = result_terms.join(" ");
                            if current_result.len() > max_len {
                                // Remove the last added term and stop
                                result_terms.pop();
                                return result_terms.join(" ");
                            }
                        }
                    }
                }
            }
        }

        result_terms.join(" ")
    }
}

/// Polish stemming - simple suffix stripping
/// Returns: original word + stemmed variants
pub fn polish_stem(word: &str) -> Vec<String> {
    let mut results = vec![word.to_string()];
    let lower = word.to_lowercase();

    // Rules ordered by priority (apply first match)
    let suffixes = [
        ("ów", ""),  // plural genitive
        ("ach", ""), // plural locative
        ("ami", ""), // plural instrumental
        ("om", ""),  // plural dative
        ("owi", ""), // singular dative
        ("em", ""),  // singular instrumental
        ("ię", ""),  // accusative
        ("ie", ""),  // locative/nominative
        ("ej", ""),  // comparative/locative
        ("ą", ""),   // accusative/instrumental
        ("ę", ""),   // accusative
        ("y", ""),   // nominative plural / adjective
        ("i", ""),   // nominative plural / adjective
    ];

    for (suffix, _) in &suffixes {
        if lower.ends_with(suffix) && lower.len() > suffix.len() + 2 {
            // Keep at least 2 chars in base
            let stemmed = lower[..lower.len() - suffix.len()].to_string();
            if !results.contains(&stemmed) {
                results.push(stemmed);
            }
            break; // Apply only first matching suffix
        }
    }

    results
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn create_test_synonyms() -> NamedTempFile {
        let mut file = NamedTempFile::new().expect("create temp file");
        writeln!(
            file,
            r#"
[[synonyms]]
terms = ["ACME", "ACME Corporation", "acme corp"]

[[synonyms]]
terms = ["Bob", "Robert", "Robert Smith"]

[[synonyms]]
terms = ["YouTube", "YT", "film", "wideo", "video"]

[[synonyms]]
terms = ["transcription", "transcript", "meeting notes"]
"#
        )
        .expect("write synonyms to temp file");
        file
    }

    #[test]
    fn test_synonym_expansion() {
        let file = create_test_synonyms();
        let expander =
            QueryExpander::load(file.path()).expect("load query expander from temp file");

        let expanded = expander.expand("ACME");
        assert!(expanded.contains("ACME"));
        assert!(expanded.contains("Corporation"));
    }

    #[test]
    fn test_no_duplicate() {
        let file = create_test_synonyms();
        let expander =
            QueryExpander::load(file.path()).expect("load query expander from temp file");

        let expanded = expander.expand("ACME Corporation");
        let count = expanded.matches("Corporation").count();
        assert_eq!(count, 1, "Should not duplicate 'Corporation'");
    }

    #[test]
    fn test_case_insensitive() {
        let file = create_test_synonyms();
        let expander =
            QueryExpander::load(file.path()).expect("load query expander from temp file");

        let expanded = expander.expand("acme");
        assert!(expanded.to_lowercase().contains("corporation"));
    }

    #[test]
    fn test_max_expansion() {
        let file = create_test_synonyms();
        let expander =
            QueryExpander::load(file.path()).expect("load query expander from temp file");

        // Test with a longer query to verify 3x limit works
        let long_query = "ACME YouTube Bob ".repeat(20); // ~340 chars
        let expanded = expander.expand(&long_query);
        let max_expected = std::cmp::max(long_query.len() * 3, 200);
        assert!(
            expanded.len() <= max_expected,
            "Expansion exceeded limit: {} > {}",
            expanded.len(),
            max_expected
        );
    }

    #[test]
    fn test_no_match_passthrough() {
        let file = create_test_synonyms();
        let expander =
            QueryExpander::load(file.path()).expect("load query expander from temp file");

        let expanded = expander.expand("xyzabc");
        assert_eq!(expanded, "xyzabc");
    }

    #[test]
    fn test_polish_stem() {
        let stems = polish_stem("transkrypcją");
        assert!(stems.contains(&"transkrypcją".to_string()));
        assert!(stems.contains(&"transkrypcj".to_string()));
    }

    #[test]
    fn test_sar_from_organizacji() {
        let mut file = NamedTempFile::new().expect("create temp file");
        writeln!(
            file,
            r#"
[[synonyms]]
terms = ["SAR", "Stowarzyszenie Agencji Reklamowych", "organizacja branżowa", "organizacji"]
"#
        )
        .expect("write synonyms to temp file");
        let expander =
            QueryExpander::load(file.path()).expect("load query expander from temp file");
        let expanded = expander.expand("Do jakiej organizacji branżowej należy Acme?");
        println!("Expanded: {}", expanded);
        assert!(
            expanded.contains("SAR"),
            "Should contain SAR, got: {}",
            expanded
        );
    }

    #[test]
    fn test_polish_stem_plural() {
        let stems = polish_stem("oczyszczaczy");
        assert!(stems.contains(&"oczyszczaczy".to_string()));
        // Should remove "y" suffix
        assert!(stems.contains(&"oczyszczacz".to_string()));
    }
}
