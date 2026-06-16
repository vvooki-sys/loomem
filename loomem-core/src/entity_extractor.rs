use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntityExtractionConfig {
    pub enabled: bool,
    pub model: String,
    pub batch_size: usize,
    pub flush_interval_secs: u64,
    pub queue_capacity: usize,
    pub confidence_threshold: f64,
    pub max_tokens_per_batch: usize,
}

impl Default for EntityExtractionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            model: "gpt-4.1-mini".to_string(),
            batch_size: 5,
            flush_interval_secs: 10,
            queue_capacity: 200,
            confidence_threshold: 0.7,
            max_tokens_per_batch: 2000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntityType {
    Person,
    Organization,
    Project,
    Technology,
    Place,
}

impl std::fmt::Display for EntityType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EntityType::Person => write!(f, "Person"),
            EntityType::Organization => write!(f, "Organization"),
            EntityType::Project => write!(f, "Project"),
            EntityType::Technology => write!(f, "Technology"),
            EntityType::Place => write!(f, "Place"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Relation {
    pub subject: String,
    pub relation: String,
    pub object: String,
}

#[derive(Debug, Clone)]
pub struct EntityDef {
    pub canonical: String,
    pub entity_type: EntityType,
    pub aliases: Vec<String>,
    pub role: Option<String>,
    pub expand_with: Vec<String>,
}

#[derive(Deserialize)]
struct EntityDefConfig {
    canonical: String,
    #[serde(rename = "type")]
    entity_type: String,
    aliases: Vec<String>,
    role: Option<String>,
    expand_with: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct EntitiesConfig {
    persons: Option<PersonsConfig>,
    organizations: Option<OrganizationsConfig>,
    projects: Option<ProjectsConfig>,
    technology: Option<TechnologyConfig>,
    places: Option<PlacesConfig>,
    relations: Option<RelationsConfig>,
    entity: Option<Vec<EntityDefConfig>>,
}

#[derive(Deserialize)]
struct RelationsConfig {
    entries: Vec<String>,
}

#[derive(Deserialize)]
struct PersonsConfig {
    names: Vec<String>,
}

#[derive(Deserialize)]
struct OrganizationsConfig {
    names: Vec<String>,
}

#[derive(Deserialize)]
struct ProjectsConfig {
    names: Vec<String>,
}

#[derive(Deserialize)]
struct TechnologyConfig {
    names: Vec<String>,
}

#[derive(Deserialize)]
struct PlacesConfig {
    names: Vec<String>,
}

pub struct EntityExtractor {
    // Map from lowercase entity name to (original_name, entity_type)
    known_entities: HashMap<String, (String, EntityType)>,
    // All known relations loaded from entities.toml
    relations: Vec<Relation>,
    // New: structured entity definitions with aliases
    entity_defs: Vec<EntityDef>,
    // New: map from lowercase alias to index in entity_defs
    alias_map: HashMap<String, usize>,
}

impl EntityExtractor {
    pub fn load(path: &Path) -> Result<Self> {
        let contents = fs::read_to_string(path)
            .with_context(|| format!("Failed to read entities file: {}", path.display()))?;

        Self::load_from_str(&contents)
            .with_context(|| format!("Failed to parse entities file: {}", path.display()))
    }

    pub fn load_from_str(content: &str) -> Result<Self> {
        let config: EntitiesConfig =
            toml::from_str(content).context("Failed to parse TOML content")?;

        let mut known_entities = HashMap::new();

        // Load persons
        if let Some(persons) = config.persons {
            for name in persons.names {
                known_entities.insert(name.to_lowercase(), (name.clone(), EntityType::Person));
            }
        }

        // Load organizations
        if let Some(orgs) = config.organizations {
            for name in orgs.names {
                known_entities.insert(
                    name.to_lowercase(),
                    (name.clone(), EntityType::Organization),
                );
            }
        }

        // Load projects
        if let Some(projects) = config.projects {
            for name in projects.names {
                known_entities.insert(name.to_lowercase(), (name.clone(), EntityType::Project));
            }
        }

        // Load technology
        if let Some(tech) = config.technology {
            for name in tech.names {
                known_entities.insert(name.to_lowercase(), (name.clone(), EntityType::Technology));
            }
        }

        // Load places
        if let Some(places) = config.places {
            for name in places.names {
                known_entities.insert(name.to_lowercase(), (name.clone(), EntityType::Place));
            }
        }

        // Load relations
        let mut relations = Vec::new();
        if let Some(rels) = config.relations {
            for entry in rels.entries {
                let parts: Vec<&str> = entry.splitn(3, '|').collect();
                if parts.len() == 3 {
                    relations.push(Relation {
                        subject: parts[0].to_string(),
                        relation: parts[1].to_string(),
                        object: parts[2].to_string(),
                    });
                }
            }
        }

        // NEW: Load [[entity]] sections with alias support
        let mut entity_defs: Vec<EntityDef> = Vec::new();
        let mut alias_map: HashMap<String, usize> = HashMap::new();

        if let Some(entity_defs_config) = config.entity {
            for def_config in entity_defs_config {
                let entity_type = match def_config.entity_type.as_str() {
                    "person" => EntityType::Person,
                    "organization" => EntityType::Organization,
                    "project" => EntityType::Project,
                    "technology" => EntityType::Technology,
                    "place" => EntityType::Place,
                    _ => EntityType::Person, // default fallback
                };

                let canonical = def_config.canonical.clone();
                let aliases = def_config.aliases.clone();
                let expand_with = def_config.expand_with.unwrap_or_default();

                // Register in known_entities for extract() matching
                // (must happen before entity_type is moved into EntityDef)
                known_entities
                    .entry(canonical.to_lowercase())
                    .or_insert_with(|| (canonical.clone(), entity_type.clone()));
                for alias in &aliases {
                    known_entities
                        .entry(alias.to_lowercase())
                        .or_insert_with(|| (canonical.clone(), entity_type.clone()));
                }

                let def = EntityDef {
                    canonical: canonical.clone(),
                    entity_type,
                    aliases: aliases.clone(),
                    role: def_config.role,
                    expand_with,
                };

                let idx = entity_defs.len();
                entity_defs.push(def);

                // Zmiana A: collision detection - warn on duplicate, first wins
                let canonical_lower = canonical.to_lowercase();
                if let Some(&existing_idx) = alias_map.get(&canonical_lower) {
                    tracing::warn!(
                        "Alias collision: canonical '{}' already mapped to entity '{}', ignoring duplicate",
                        canonical,
                        entity_defs[existing_idx].canonical
                    );
                } else {
                    alias_map.insert(canonical_lower, idx);
                }

                for alias in &aliases {
                    let alias_lower = alias.to_lowercase();
                    if let Some(&existing_idx) = alias_map.get(&alias_lower) {
                        tracing::warn!(
                            "Alias collision: '{}' already mapped to entity '{}', ignoring duplicate for '{}'",
                            alias,
                            entity_defs[existing_idx].canonical,
                            canonical
                        );
                    } else {
                        alias_map.insert(alias_lower, idx);
                    }
                }
            }
        }

        Ok(EntityExtractor {
            known_entities,
            relations,
            entity_defs,
            alias_map,
        })
    }

    pub fn extract(&self, text: &str) -> Vec<(String, EntityType)> {
        let mut found_entities = Vec::new();
        let mut matched_positions = Vec::new();
        let text_lower = text.to_lowercase();

        // Collect all matches with their positions
        let mut matches: Vec<(usize, usize, String, EntityType)> = Vec::new();

        for (entity_lower, (entity_name, entity_type)) in &self.known_entities {
            let mut start = 0;
            while let Some(pos) = text_lower[start..].find(entity_lower) {
                let actual_pos = start + pos;
                let end_pos = actual_pos + entity_lower.len();

                // Check word boundaries using byte-safe operations
                let before_ok = actual_pos == 0 || {
                    let before_char = text_lower[..actual_pos].chars().last();
                    before_char.map(|c| !c.is_alphanumeric()).unwrap_or(true)
                };

                let after_ok = end_pos >= text_lower.len() || {
                    let after_char = text_lower[end_pos..].chars().next();
                    after_char.map(|c| !c.is_alphanumeric()).unwrap_or(true)
                };

                if before_ok && after_ok {
                    matches.push((
                        actual_pos,
                        end_pos,
                        entity_name.clone(),
                        entity_type.clone(),
                    ));
                }

                // Move past this match, ensuring we stay on character boundary
                start = end_pos;
            }
        }

        // Sort by position, then by length (longest first) to handle overlapping matches
        matches.sort_by(|a, b| {
            if a.0 != b.0 {
                a.0.cmp(&b.0)
            } else {
                b.1.cmp(&a.1) // longer match first
            }
        });

        // Filter out overlapping matches (keep longest)
        for (start, end, name, entity_type) in matches {
            let overlaps = matched_positions.iter().any(|(s, e)| {
                (start >= *s && start < *e) || (end > *s && end <= *e) || (start <= *s && end >= *e)
            });

            if !overlaps {
                matched_positions.push((start, end));
                found_entities.push((name, entity_type));
            }
        }

        // Deduplicate by name
        let mut seen = std::collections::HashSet::new();
        found_entities.retain(|(name, _)| seen.insert(name.clone()));

        found_entities
    }

    /// Find relations relevant to the given extracted entities.
    /// Returns relations where any extracted entity appears as subject or object.
    pub fn find_relations(&self, entities: &[(String, EntityType)]) -> Vec<Relation> {
        let entity_names_lower: Vec<String> = entities
            .iter()
            .map(|(name, _)| name.to_lowercase())
            .collect();

        self.relations
            .iter()
            .filter(|rel| {
                let subj_lower = rel.subject.to_lowercase();
                let obj_lower = rel.object.to_lowercase();
                entity_names_lower.contains(&subj_lower) || entity_names_lower.contains(&obj_lower)
            })
            .cloned()
            .collect()
    }

    /// NEW: Resolve entity aliases in query and return enriched query.
    /// Example: "dietetyczka" → "dietetyczka Maria Nowak jadłospis dieta przepis posiłek"
    /// Zmiana B: Uses Unicode-aware word boundary check for multi-byte UTF-8 safety.
    pub fn resolve_aliases(&self, query: &str) -> String {
        let query_lower = query.to_lowercase();
        let mut extra_terms: Vec<String> = Vec::new();
        let mut matched_indices: Vec<usize> = Vec::new();

        for (alias_lower, &idx) in &self.alias_map {
            // Zmiana B: Unicode-aware word boundary check
            let mut start = 0;
            while let Some(pos) = query_lower[start..].find(alias_lower.as_str()) {
                let abs_pos = start + pos;
                let end_pos = abs_pos + alias_lower.len();

                // Check word boundaries using Unicode-aware operations (NOT as_bytes())
                let before_ok = abs_pos == 0 || {
                    query_lower[..abs_pos]
                        .chars()
                        .last()
                        .map(|c| !c.is_alphanumeric())
                        .unwrap_or(true)
                };

                let after_ok = end_pos >= query_lower.len() || {
                    query_lower[end_pos..]
                        .chars()
                        .next()
                        .map(|c| !c.is_alphanumeric())
                        .unwrap_or(true)
                };

                if before_ok && after_ok && !matched_indices.contains(&idx) {
                    matched_indices.push(idx);
                    let def = &self.entity_defs[idx];

                    // Add canonical name if not already in query
                    if !query_lower.contains(&def.canonical.to_lowercase()) {
                        extra_terms.push(def.canonical.clone());
                    }

                    // Add expand_with terms
                    for term in &def.expand_with {
                        if !query_lower.contains(&term.to_lowercase()) {
                            extra_terms.push(term.clone());
                        }
                    }
                    break; // Found match for this alias, move to next alias
                }

                start = end_pos;
            }
        }

        if extra_terms.is_empty() {
            query.to_string()
        } else {
            format!("{} {}", query, extra_terms.join(" "))
        }
    }

    /// NEW: For a given query, return matched EntityDefs (for logging/debugging).
    pub fn find_entities_in_query(&self, query: &str) -> Vec<&EntityDef> {
        let query_lower = query.to_lowercase();
        let mut result = Vec::new();
        let mut seen = Vec::new();

        for (alias_lower, &idx) in &self.alias_map {
            // Same Unicode-aware word boundary check as resolve_aliases
            let mut start = 0;
            while let Some(pos) = query_lower[start..].find(alias_lower.as_str()) {
                let abs_pos = start + pos;
                let end_pos = abs_pos + alias_lower.len();

                let before_ok = abs_pos == 0 || {
                    query_lower[..abs_pos]
                        .chars()
                        .last()
                        .map(|c| !c.is_alphanumeric())
                        .unwrap_or(true)
                };

                let after_ok = end_pos >= query_lower.len() || {
                    query_lower[end_pos..]
                        .chars()
                        .next()
                        .map(|c| !c.is_alphanumeric())
                        .unwrap_or(true)
                };

                if before_ok && after_ok && !seen.contains(&idx) {
                    seen.push(idx);
                    result.push(&self.entity_defs[idx]);
                    break;
                }

                start = end_pos;
            }
        }
        result
    }

    /// Get aliases for a canonical entity name.
    pub fn get_aliases_for(&self, canonical_name: &str) -> Vec<String> {
        let lower = canonical_name.to_lowercase();
        for def in &self.entity_defs {
            if def.canonical.to_lowercase() == lower {
                return def.aliases.clone();
            }
        }
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn create_test_extractor() -> EntityExtractor {
        let mut known_entities = HashMap::new();
        known_entities.insert("adam".to_string(), ("Adam".to_string(), EntityType::Person));
        known_entities.insert(
            "adam wiśniewski".to_string(),
            ("Adam Wiśniewski".to_string(), EntityType::Person),
        );
        known_entities.insert("anna".to_string(), ("Anna".to_string(), EntityType::Person));
        known_entities.insert(
            "acme".to_string(),
            ("Acme".to_string(), EntityType::Organization),
        );
        known_entities.insert(
            "loomem".to_string(),
            ("Loomem".to_string(), EntityType::Project),
        );

        EntityExtractor {
            known_entities,
            relations: Vec::new(),
            entity_defs: Vec::new(),
            alias_map: HashMap::new(),
        }
    }

    #[test]
    fn test_extract_person() {
        let extractor = create_test_extractor();
        let result = extractor.extract("Adam Wiśniewski jest dyrektorem");

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0, "Adam Wiśniewski");
        assert_eq!(result[0].1, EntityType::Person);
    }

    #[test]
    fn test_extract_multiple() {
        let extractor = create_test_extractor();
        let result = extractor.extract("Anna z Acme");

        assert_eq!(result.len(), 2);
        assert_eq!(result[0].0, "Anna");
        assert_eq!(result[0].1, EntityType::Person);
        assert_eq!(result[1].0, "Acme");
        assert_eq!(result[1].1, EntityType::Organization);
    }

    #[test]
    fn test_case_insensitive() {
        let extractor = create_test_extractor();
        let result = extractor.extract("Working on loomem today");

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0, "Loomem");
        assert_eq!(result[0].1, EntityType::Project);
    }

    #[test]
    fn test_no_match() {
        let extractor = create_test_extractor();
        let result = extractor.extract("random text without entities");

        assert_eq!(result.len(), 0);
    }

    #[test]
    fn test_extract_overlapping() {
        let extractor = create_test_extractor();
        // "Adam Wiśniewski" should match as one entity, not "Adam" separately
        let result = extractor.extract("Adam Wiśniewski is here");

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0, "Adam Wiśniewski");
        assert_eq!(result[0].1, EntityType::Person);
    }

    #[test]
    fn test_load_from_file() {
        let mut temp_file = NamedTempFile::new().expect("create temp file");
        write!(
            temp_file,
            r#"
[persons]
names = ["Test Person"]

[organizations]
names = ["Test Org"]
            "#
        )
        .expect("write to temp file");

        let extractor =
            EntityExtractor::load(temp_file.path()).expect("load extractor from temp file");
        let result = extractor.extract("Test Person works at Test Org");

        assert_eq!(result.len(), 2);
    }

    #[test]
    fn test_load_relations() {
        let mut temp_file = NamedTempFile::new().expect("create temp file");
        write!(
            temp_file,
            r#"
[persons]
names = ["Anna"]

[organizations]
names = ["Acme", "SAR"]

[relations]
entries = [
    "Acme|member_of|SAR",
    "Anna|works_at|Acme",
]
            "#
        )
        .expect("write to temp file");

        let extractor =
            EntityExtractor::load(temp_file.path()).expect("load extractor from temp file");
        assert_eq!(extractor.relations.len(), 2);
        assert_eq!(extractor.relations[0].subject, "Acme");
        assert_eq!(extractor.relations[0].relation, "member_of");
        assert_eq!(extractor.relations[0].object, "SAR");
    }

    #[test]
    fn test_find_relations() {
        let mut temp_file = NamedTempFile::new().expect("create temp file");
        write!(
            temp_file,
            r#"
[persons]
names = ["Anna"]

[organizations]
names = ["Acme", "SAR"]

[relations]
entries = [
    "Acme|member_of|SAR",
    "Anna|works_at|Acme",
    "Atlas|uses|Upstash",
]
            "#
        )
        .expect("write to temp file");

        let extractor =
            EntityExtractor::load(temp_file.path()).expect("load extractor from temp file");
        let entities = extractor.extract("Anna z Acme");
        let relations = extractor.find_relations(&entities);

        // Should find "Acme|member_of|SAR" and "Anna|works_at|Acme"
        // Should NOT find "Atlas|uses|Upstash"
        assert_eq!(relations.len(), 2);
        let rel_strs: Vec<String> = relations
            .iter()
            .map(|r| format!("{}|{}|{}", r.subject, r.relation, r.object))
            .collect();
        assert!(rel_strs.contains(&"Acme|member_of|SAR".to_string()));
        assert!(rel_strs.contains(&"Anna|works_at|Acme".to_string()));
    }

    // NEW TESTS for Entity Alias Layer

    #[test]
    fn test_alias_resolve_dietetyczka() {
        let toml_content = r#"
[[entity]]
canonical = "Maria Nowak"
type = "person"
aliases = ["dietetyczka", "Marta", "pani Marta"]
role = "dietetyczka"
expand_with = ["jadłospis", "dieta", "przepis", "posiłek"]
        "#;

        let extractor =
            EntityExtractor::load_from_str(toml_content).expect("load extractor from TOML string");
        let resolved = extractor.resolve_aliases("co powiedziała dietetyczka");

        assert!(resolved.contains("Maria Nowak"));
        assert!(resolved.contains("jadłospis"));
        assert!(resolved.contains("dieta"));
        assert!(resolved.contains("przepis"));
    }

    #[test]
    fn test_alias_resolve_prezes() {
        let toml_content = r#"
[[entity]]
canonical = "Jan Kowalski"
type = "person"
aliases = ["Jan", "Kowalski", "prezes", "prezes Acme"]
role = "Prezes Zarządu Acme"
expand_with = ["eventy", "Acme Events"]
        "#;

        let extractor =
            EntityExtractor::load_from_str(toml_content).expect("load extractor from TOML string");
        let resolved = extractor.resolve_aliases("co powiedział prezes na spotkaniu");

        assert!(resolved.contains("Jan Kowalski"));
        assert!(resolved.contains("eventy"));
    }

    #[test]
    fn test_alias_resolve_no_match() {
        let toml_content = r#"
[[entity]]
canonical = "Maria Nowak"
type = "person"
aliases = ["dietetyczka"]
expand_with = ["jadłospis"]
        "#;

        let extractor =
            EntityExtractor::load_from_str(toml_content).expect("load extractor from TOML string");
        let resolved = extractor.resolve_aliases("random tekst bez encji");

        assert_eq!(resolved, "random tekst bez encji");
    }

    #[test]
    fn test_alias_map_case_insensitive() {
        let toml_content = r#"
[[entity]]
canonical = "Maria Nowak"
type = "person"
aliases = ["dietetyczka"]
expand_with = ["jadłospis"]
        "#;

        let extractor =
            EntityExtractor::load_from_str(toml_content).expect("load extractor from TOML string");
        let resolved = extractor.resolve_aliases("DIETETYCZKA jest świetna");

        assert!(resolved.contains("Maria Nowak"));
        assert!(resolved.contains("jadłospis"));
    }

    #[test]
    fn test_no_duplicate_terms() {
        let toml_content = r#"
[[entity]]
canonical = "Maria Nowak"
type = "person"
aliases = ["dietetyczka", "Marta"]
expand_with = ["jadłospis"]
        "#;

        let extractor =
            EntityExtractor::load_from_str(toml_content).expect("load extractor from TOML string");
        let resolved = extractor.resolve_aliases("Maria Nowak jadłospis");

        // Canonical and expand_with already in query - should not duplicate
        let count_canonical = resolved.matches("Maria Nowak").count();
        let count_jadlospis = resolved.matches("jadłospis").count();
        assert_eq!(count_canonical, 1);
        assert_eq!(count_jadlospis, 1);
    }

    #[test]
    fn test_extract_finds_entity_section_entries() {
        // Entities defined ONLY via [[entity]] (not in [persons]) must be found by extract()
        let toml_content = r#"
[[entity]]
canonical = "Jan Kowalski"
type = "person"
aliases = ["Jan", "Kowalski", "prezes"]
expand_with = ["eventy"]
        "#;

        let extractor = EntityExtractor::load_from_str(toml_content).unwrap();

        // Match by canonical name
        let results = extractor.extract("Spotkanie z Jan Kowalski w piątek");
        assert!(!results.is_empty(), "Should find entity by canonical name");
        assert_eq!(results[0].0, "Jan Kowalski");

        // Match by alias
        let results = extractor.extract("Kowalski powiedział że projekt idzie dobrze");
        assert!(!results.is_empty(), "Should find entity by alias");
        assert_eq!(results[0].0, "Jan Kowalski");

        // Match by another alias
        let results = extractor.extract("prezes chce zmienić plan");
        assert!(!results.is_empty(), "Should find entity by alias 'prezes'");
        assert_eq!(results[0].0, "Jan Kowalski");
    }
}
