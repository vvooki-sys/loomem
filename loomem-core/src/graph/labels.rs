//! Source-attributed graph labels use the existing encryption provider and format.
//! Raw entity payloads stay unchanged; live readers never certify aggregate aliases.
use super::{Edge, EntityNode, GraphStore};
use crate::{pii_filter::PiiFilter, storage::Chunk};
use anyhow::{ensure, Context, Result};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::collections::BTreeSet;

mod index;
mod legacy_links;
mod read;
#[cfg(test)]
mod tests;

/// Owned source incarnation captured before inference; contains no source text.
#[derive(Debug, Clone)]
pub struct GraphContributionTarget {
    chunk_id: String,
    stream: String,
    fingerprint: String,
}

pub struct EntityContribution<'a> {
    pub name: &'a str,
    pub entity_type: &'a str,
    pub aliases: &'a [String],
    pub stream: &'a str,
    pub chunk_id: &'a str,
    pub expected_content: &'a str,
    pub pii: &'a PiiFilter,
}

pub struct RelationContribution<'a> {
    pub source_entity_id: &'a str,
    pub target_entity_id: &'a str,
    pub relation: &'a str,
    pub stream: &'a str,
    pub chunk_id: &'a str,
    pub expected_content: &'a str,
    pub pii: &'a PiiFilter,
}

/// Shared configured source policy for a batch of graph contributions.
pub struct GraphContributionContext<'a> {
    pub stream: &'a str,
    pub chunk_id: &'a str,
    pub expected_content: &'a str,
    pub pii: &'a PiiFilter,
}

impl GraphContributionContext<'_> {
    pub fn entity<'a>(
        &'a self,
        name: &'a str,
        entity_type: &'a str,
        aliases: &'a [String],
    ) -> EntityContribution<'a> {
        EntityContribution {
            name,
            entity_type,
            aliases,
            stream: self.stream,
            chunk_id: self.chunk_id,
            expected_content: self.expected_content,
            pii: self.pii,
        }
    }
    pub fn relation<'a>(
        &'a self,
        source_entity_id: &'a str,
        target_entity_id: &'a str,
        relation: &'a str,
    ) -> RelationContribution<'a> {
        RelationContribution {
            source_entity_id,
            target_entity_id,
            relation,
            stream: self.stream,
            chunk_id: self.chunk_id,
            expected_content: self.expected_content,
            pii: self.pii,
        }
    }
}

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LabelClaims {
    #[serde(default)]
    version: u8,
    #[serde(default)]
    owner_id: String,
    #[serde(default)]
    chunk_id: String,
    #[serde(default)]
    stream: String,
    #[serde(default)]
    fingerprint: String,
    #[serde(default)]
    claims: Vec<LabelClaim>,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LabelClaim {
    #[serde(default)]
    name: String,
    #[serde(default)]
    entity_type: String,
    #[serde(default)]
    aliases: Vec<String>,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RelationClaims {
    #[serde(default)]
    version: u8,
    #[serde(default)]
    owner_id: String,
    #[serde(default)]
    chunk_id: String,
    #[serde(default)]
    stream: String,
    #[serde(default)]
    fingerprint: String,
    #[serde(default)]
    relations: BTreeSet<String>,
}

impl GraphStore {
    /// Capture before asynchronous extraction. Publication validates under the same
    /// lifecycle mutex, including content changes and delete/recreate ABA cycles.
    pub fn capture_contribution_target(
        &self,
        chunk_id: &str,
        stream: &str,
        expected_content: &str,
    ) -> Result<GraphContributionTarget> {
        let _guard = self.store.lock_lifecycle()?;
        let chunk = self.contribution_chunk(chunk_id, stream, expected_content)?;
        Ok(GraphContributionTarget {
            chunk_id: chunk.id.clone(),
            stream: chunk.stream.clone(),
            fingerprint: self.support_fingerprint(&chunk)?,
        })
    }

    /// Publish auxiliary extraction rows only for the captured source incarnation.
    /// The marker carries no model output; consumers only test its presence.
    pub fn publish_extraction_metadata(
        &self,
        target: &GraphContributionTarget,
        entities: &[(String, String)],
        relations: &[(String, String, String)],
        pii: &PiiFilter,
    ) -> Result<()> {
        self.publish_metadata(target, entities, relations, pii, true)
    }

    /// Dictionary evidence does not mark asynchronous LLM extraction complete.
    pub fn publish_dictionary_metadata(
        &self,
        target: &GraphContributionTarget,
        entities: &[(String, String)],
        relations: &[(String, String, String)],
        pii: &PiiFilter,
    ) -> Result<()> {
        self.publish_metadata(target, entities, relations, pii, false)
    }

    fn publish_metadata(
        &self,
        target: &GraphContributionTarget,
        entities: &[(String, String)],
        relations: &[(String, String, String)],
        pii: &PiiFilter,
        mark_extracted: bool,
    ) -> Result<()> {
        let _guard = self.store.lock_lifecycle()?;
        let chunk = self
            .store
            .get_chunk(&target.chunk_id)?
            .context("Graph support missing")?;
        ensure!(chunk.deleted_at.is_none(), "Graph support deleted");
        self.validate_contribution_target(Some(target), &chunk)?;
        let entities: Vec<_> = entities
            .iter()
            .map(|(name, kind)| (pii.redact_for_sink(name), pii.redact_for_sink(kind)))
            .collect();
        let relations: Vec<_> = relations
            .iter()
            .map(|(left, kind, right)| {
                (
                    pii.redact_for_sink(left),
                    pii.redact_for_sink(kind),
                    pii.redact_for_sink(right),
                )
            })
            .collect();
        self.store
            .store_entities(&chunk.id, &chunk.stream, &entities)?;
        self.store
            .store_relations(&chunk.id, &chunk.stream, &relations)?;
        if mark_extracted {
            self.store
                .put(format!("llm_entities:{}", chunk.id).as_bytes(), b"1")?;
        }
        Ok(())
    }

    /// Publish only the incoming labels, against the exact current source content.
    pub fn link_entity(&self, input: EntityContribution<'_>) -> Result<EntityNode> {
        self.publish_entity(input, None)
    }

    /// Use a target captured before asynchronous work, never a fresh late snapshot.
    pub fn link_entity_if_current(
        &self,
        input: EntityContribution<'_>,
        target: &GraphContributionTarget,
    ) -> Result<EntityNode> {
        self.publish_entity(input, Some(target))
    }

    fn publish_entity(
        &self,
        input: EntityContribution<'_>,
        target: Option<&GraphContributionTarget>,
    ) -> Result<EntityNode> {
        let _guard = self.store.lock_lifecycle()?;
        let chunk =
            self.contribution_chunk(input.chunk_id, input.stream, input.expected_content)?;
        self.validate_contribution_target(target, &chunk)?;
        self.publish_entity_claim(input, &chunk)
    }

    fn publish_entity_claim(
        &self,
        input: EntityContribution<'_>,
        chunk: &Chunk,
    ) -> Result<EntityNode> {
        let claim = LabelClaim {
            name: input.pii.redact_for_sink(input.name).trim().to_string(),
            entity_type: input
                .pii
                .redact_for_sink(input.entity_type)
                .trim()
                .to_string(),
            aliases: input
                .aliases
                .iter()
                .map(|alias| input.pii.redact_for_sink(alias))
                .filter(|alias| !alias.trim().is_empty())
                .collect(),
        };
        ensure!(
            !claim.name.is_empty() && !claim.entity_type.is_empty(),
            "Empty graph label"
        );
        self.check_label_owners(&claim, input.stream)?;
        let entity = self.get_or_create_entity(
            &claim.name,
            &claim.entity_type,
            &claim.aliases,
            input.stream,
        )?;
        ensure!(
            entity.stream_id == input.stream,
            "Graph entity crosses streams"
        );
        let entity = self.retain_claim_name(entity, &claim.name)?;
        self.store_label_claim(&entity, chunk, claim.clone())?;
        self.add_chunk_to_entity(&entity.id, &chunk.id)?;
        self.index_claim(&entity, &claim)?;
        self.get_entity_by_id(&entity.id)?
            .context("Published graph entity missing")
    }

    /// Publish a normalized relation only with matching live endpoint support.
    pub fn link_relation(&self, input: RelationContribution<'_>) -> Result<Edge> {
        self.publish_relation(input, None)
    }

    /// Use a target captured before asynchronous work, never a fresh late snapshot.
    pub fn link_relation_if_current(
        &self,
        input: RelationContribution<'_>,
        target: &GraphContributionTarget,
    ) -> Result<Edge> {
        self.publish_relation(input, Some(target))
    }

    fn publish_relation(
        &self,
        input: RelationContribution<'_>,
        target: Option<&GraphContributionTarget>,
    ) -> Result<Edge> {
        let _guard = self.store.lock_lifecycle()?;
        let chunk =
            self.contribution_chunk(input.chunk_id, input.stream, input.expected_content)?;
        self.validate_contribution_target(target, &chunk)?;
        self.publish_relation_claim(input, &chunk)
    }

    fn publish_relation_claim(
        &self,
        input: RelationContribution<'_>,
        chunk: &Chunk,
    ) -> Result<Edge> {
        self.contributing_endpoint(input.source_entity_id, chunk)?;
        self.contributing_endpoint(input.target_entity_id, chunk)?;
        let relation = input.pii.redact_for_sink(input.relation);
        let relation = relation_label(&relation).to_string();
        let edge = self.get_or_create_edge(
            input.source_entity_id,
            input.target_entity_id,
            &relation,
            input.stream,
        )?;
        ensure!(
            edge.stream_id == input.stream
                && edge.source_entity_id == input.source_entity_id
                && edge.target_entity_id == input.target_entity_id
                && relation_label(&edge.relation_type) == relation,
            "Graph edge does not match contribution"
        );
        self.store_relation_claim(&edge, chunk, relation)?;
        self.add_chunk_to_edge(&edge.id, &chunk.id)?;
        self.get_edge_by_id(&edge.id)?
            .context("Published graph edge missing")
    }

    fn validate_contribution_target(
        &self,
        target: Option<&GraphContributionTarget>,
        chunk: &Chunk,
    ) -> Result<()> {
        if let Some(target) = target {
            ensure!(
                target.chunk_id == chunk.id
                    && target.stream == chunk.stream
                    && target.fingerprint == self.support_fingerprint(chunk)?,
                "Graph source incarnation changed"
            );
        }
        Ok(())
    }

    fn support_fingerprint(&self, chunk: &Chunk) -> Result<String> {
        let mut hash = blake3::Hasher::new();
        hash.update(chunk.content.as_bytes());
        hash.update(&[0]);
        if let Some(generation) = self.store.graph_generation(&chunk.id)? {
            hash.update(&[1]);
            hash.update(&generation);
        }
        Ok(hash.finalize().to_hex().to_string())
    }

    fn contribution_chunk(&self, id: &str, stream: &str, content: &str) -> Result<Chunk> {
        let chunk = self.store.get_chunk(id)?.context("Graph support missing")?;
        ensure!(
            chunk.deleted_at.is_none() && chunk.stream == stream && chunk.content == content,
            "Graph support is obsolete or outside stream"
        );
        Ok(chunk)
    }

    fn contributing_endpoint(&self, id: &str, chunk: &Chunk) -> Result<()> {
        let node = self
            .get_live_entity_by_id(id)?
            .context("Graph endpoint unsupported")?;
        ensure!(
            node.stream_id == chunk.stream && node.chunk_ids.contains(&chunk.id),
            "Graph endpoint has no matching source support"
        );
        Ok(())
    }

    fn check_label_owners(&self, claim: &LabelClaim, stream: &str) -> Result<()> {
        let mut owners = BTreeSet::new();
        for label in std::iter::once(&claim.name).chain(&claim.aliases) {
            self.label_owners(label, stream, &mut owners)?;
        }
        ensure!(owners.len() <= 1, "Graph label collision across entities");
        Ok(())
    }

    fn label_owners(&self, label: &str, stream: &str, owners: &mut BTreeSet<String>) -> Result<()> {
        let token = self.name_token(stream, label)?;
        for kind in ["name", "alias"] {
            let key = format!("{}{kind}:{token}", self.stream_prefix(stream));
            if let Some(id) = self.get_string(&key)? {
                match self.get_entity_by_id(&id)? {
                    Some(node) => {
                        ensure!(node.stream_id == stream, "Graph index crosses streams");
                        owners.insert(id);
                    }
                    None => ensure!(
                        !self.entity_key_present(&id)?,
                        "Graph index target unreadable"
                    ),
                }
            }
        }
        Ok(())
    }

    // Rekey reconstructs indexes from the encrypted aggregate row. Keep incoming
    // canonical labels there for routing, while live display still requires claims.
    fn retain_claim_name(&self, entity: EntityNode, name: &str) -> Result<EntityNode> {
        if entity.canonical_name.to_lowercase() == name.to_lowercase() {
            return Ok(entity);
        }
        let prefix = self.stream_prefix(&entity.stream_id);
        self.merge_aliases_if_new(entity, &prefix, &[name.to_string()])
    }

    fn index_claim(&self, entity: &EntityNode, claim: &LabelClaim) -> Result<()> {
        for label in std::iter::once(&claim.name).chain(&claim.aliases) {
            let token = self.name_token(&entity.stream_id, label)?;
            self.put_string(
                &format!("{}alias:{token}", self.stream_prefix(&entity.stream_id)),
                &entity.id,
            )?;
        }
        Ok(())
    }

    fn store_label_claim(
        &self,
        entity: &EntityNode,
        chunk: &Chunk,
        claim: LabelClaim,
    ) -> Result<()> {
        let key = label_key(&entity.id, &chunk.id);
        let existing: Option<LabelClaims> = self.read_claim(&key, &chunk.stream)?;
        if let Some(claims) = &existing {
            ensure!(
                claims.version == 1,
                "Unsupported graph contribution version"
            );
            validate_binding(
                &entity.id,
                chunk,
                &claims.owner_id,
                &claims.chunk_id,
                &claims.stream,
            )?;
        }
        let mut claims = existing.unwrap_or_default();
        let fingerprint = self.support_fingerprint(chunk)?;
        if claims.fingerprint != fingerprint {
            claims = LabelClaims::default();
        }
        claims.version = 1;
        claims.owner_id = entity.id.clone();
        claims.chunk_id = chunk.id.clone();
        claims.stream = chunk.stream.clone();
        claims.fingerprint = fingerprint;
        if !claims.claims.contains(&claim) {
            claims.claims.push(claim);
        }
        self.write_claim(&key, &chunk.stream, &claims)
    }

    fn store_relation_claim(&self, edge: &Edge, chunk: &Chunk, relation: String) -> Result<()> {
        let key = relation_key(&edge.id, &chunk.id);
        let existing: Option<RelationClaims> = self.read_claim(&key, &chunk.stream)?;
        if let Some(claims) = &existing {
            ensure!(
                claims.version == 1,
                "Unsupported graph relation contribution version"
            );
            validate_binding(
                &edge.id,
                chunk,
                &claims.owner_id,
                &claims.chunk_id,
                &claims.stream,
            )?;
        }
        let mut claims = existing.unwrap_or_default();
        let fingerprint = self.support_fingerprint(chunk)?;
        if claims.fingerprint != fingerprint {
            claims = RelationClaims::default();
        }
        claims.version = 1;
        claims.owner_id = edge.id.clone();
        claims.chunk_id = chunk.id.clone();
        claims.stream = chunk.stream.clone();
        claims.fingerprint = fingerprint;
        claims.relations.insert(relation);
        self.write_claim(&key, &chunk.stream, &claims)
    }

    fn read_claim<T: DeserializeOwned>(&self, key: &str, stream: &str) -> Result<Option<T>> {
        let Some(bytes) = self.store.get(key.as_bytes())? else {
            return Ok(None);
        };
        let provider = self.store.encryption_provider();
        ensure!(
            crate::crypto::at_rest::is_encrypted(&bytes) == provider.is_enabled(),
            "Graph contribution encryption unavailable or inconsistent"
        );
        let plaintext = provider
            .decrypt(stream, &bytes)
            .context("Decrypt graph contribution")?;
        Ok(Some(
            serde_json::from_slice(&plaintext).context("Decode graph contribution")?,
        ))
    }

    fn write_claim<T: Serialize>(&self, key: &str, stream: &str, claim: &T) -> Result<()> {
        let bytes = serde_json::to_vec(claim).context("Encode graph contribution")?;
        let provider = self.store.encryption_provider();
        let bytes = provider
            .encrypt(stream, &bytes)
            .context("Encrypt graph contribution")?;
        ensure!(
            crate::crypto::at_rest::is_encrypted(&bytes) == provider.is_enabled(),
            "Graph contribution encryption failed"
        );
        self.store.put(key.as_bytes(), &bytes)
    }
}

fn label_key(entity: &str, chunk: &str) -> String {
    format!("graph:labels:{entity}:{chunk}")
}
fn relation_key(edge: &str, chunk: &str) -> String {
    format!("graph:edge_labels:{edge}:{chunk}")
}

fn relation_label(raw: &str) -> &'static str {
    match raw.trim().to_lowercase().as_str() {
        "works_at" => "works_at",
        "uses" => "uses",
        "manages" => "manages",
        "member_of" => "member_of",
        "implements" => "implements",
        "located_in" => "located_in",
        "created" => "created",
        "logged_in_via" => "logged_in_via",
        "pays_for" => "pays_for",
        "deployed_in" => "deployed_in",
        "accepted" => "accepted",
        "confirmed_participation" => "confirmed_participation",
        _ => "related_to",
    }
}

fn validate_binding(
    expected_owner: &str,
    chunk: &Chunk,
    owner: &str,
    chunk_id: &str,
    stream: &str,
) -> Result<()> {
    ensure!(
        owner == expected_owner && chunk_id == chunk.id && stream == chunk.stream,
        "Graph contribution owner or source mismatch"
    );
    Ok(())
}
