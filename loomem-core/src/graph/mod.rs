pub mod alias;
pub mod relation_type;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::{debug, warn};

use crate::storage::RocksDbStore;

use alias::find_person_alias;
use relation_type::normalize_relation;

/// Outcome of probing the name or alias index for an entity.
///
/// Used by `try_resolve_by_name` / `try_resolve_by_alias` to distinguish
/// three cases that `Option<EntityNode>` cannot express:
///   - `Resolved` — index hit AND entity bytes are readable.
///   - `DanglingId` — index hit but `get_entity_by_id` returned `Ok(None)`.
///     Per cycle/117 read-path fail-graceful, this covers both a truly
///     absent key AND a key whose bytes are present but undeserializable;
///     callers disambiguate via `entity_key_present`.
///   - `NoHit` — no index entry at all.
///
/// The `get_or_create_entity` orchestrator uses this to decide between
/// safe overwrite (dangling → absent key) and refusing-with-error
/// (dangling → present-but-corrupt key).
enum ResolveOutcome {
    Resolved(EntityNode),
    /// Index pointed at `id`, but `get_entity_by_id(id)` returned `Ok(None)`
    /// — bytes either absent (safe to overwrite) or present-but-corrupt
    /// (must refuse). Callers disambiguate via `entity_key_present`.
    DanglingId(String),
    NoHit,
}

/// Graph-enhanced search settings (experimental, tune on real data)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphSearchConfig {
    pub enabled: bool,
    pub max_hops: usize,
    pub boost_factor: f64,
    pub max_graph_additions: usize,
}

impl Default for GraphSearchConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_hops: 1,
            boost_factor: 0.3,
            max_graph_additions: 3,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntityNode {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub canonical_name: String,
    #[serde(default)]
    pub entity_type: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub chunk_ids: Vec<String>,
    #[serde(default)]
    pub stream_id: String,
    #[serde(default)]
    pub created_at: u64,
    #[serde(default)]
    pub updated_at: u64,
}

/// /138 phase 3b: storage-layer envelope for an entity node written with
/// field-level encryption. Wraps the in-memory `EntityNode` (unchanged) and
/// carries the `encrypted_payload` next to the plaintext routing fields.
/// Serialize-only and used solely on the encrypted write path; readers still
/// deserialize the raw value as `EntityNode`, ignoring the extra
/// `encrypted_payload` key until §D wires decrypt-on-read. Defined as a wrapper
/// (not an `EntityNode` field) so the in-memory struct and its construction
/// sites stay untouched. See ADR-013 §4 (graph:entity row, /138 resolution).
#[derive(Serialize)]
struct StoredEntity<'a> {
    #[serde(flatten)]
    entity: &'a EntityNode,
    /// AES-256-GCM blob of the serde_json-encoded tuple `(canonical_name,
    /// aliases)` under the entity's `stream_id` DEK. Tuple order is the wire
    /// format §D (decrypt) must match.
    encrypted_payload: Vec<u8>,
}

/// /138 §D: read-side inverse of `StoredEntity<'a>`. Deserializes the on-disk
/// envelope: flattened plaintext routing fields land in `entity`, and the
/// optional `encrypted_payload` (absent ⇒ empty for legacy/NoopProvider rows)
/// carries the field-level ciphertext for `(canonical_name, aliases)`.
#[derive(Deserialize)]
pub(crate) struct StoredEntityRead {
    #[serde(flatten)]
    pub(crate) entity: EntityNode,
    #[serde(default)]
    pub(crate) encrypted_payload: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Edge {
    pub id: String,
    pub source_entity_id: String,
    pub target_entity_id: String,
    pub relation_type: String,
    pub chunk_ids: Vec<String>,
    pub stream_id: String,
    pub created_at: u64,
    pub updated_at: u64,
}

pub struct GraphStore {
    store: Arc<RocksDbStore>,
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

impl GraphStore {
    pub fn new(store: Arc<RocksDbStore>) -> Self {
        Self { store }
    }

    // --- Entity CRUD ---

    /// Get or create an entity node, scoped to a stream.
    /// Deduplicates by canonical name within the stream, then aliases.
    ///
    /// If either the name index or any alias index points at a UUID whose
    /// `graph:entity:<id>` key bytes are physically present but unreadable
    /// (cycle/117 fail-graceful returns `Ok(None)` in that case), this
    /// function returns `Err` naming the offending UUID(s) rather than
    /// silently overwriting the index and orphaning existing chunk references.
    ///
    /// If the index points at a UUID whose key is entirely absent (dangling
    /// index), the safe overwrite path is preserved — a new entity is minted
    /// and the index is corrected.
    pub fn get_or_create_entity(
        &self,
        canonical_name: &str,
        entity_type: &str,
        aliases: &[String],
        stream_id: &str,
    ) -> Result<EntityNode> {
        let sp = self.stream_prefix(stream_id);
        let (resolved, probed_ids) =
            self.probe_entity_indexes(&sp, stream_id, canonical_name, aliases)?;
        if let Some(entity) = resolved {
            return Ok(entity);
        }
        self.refuse_if_any_unreadable_present(&probed_ids)?;
        // Alias-merge gate: for Person entities, check if an existing Person's
        // name is a substring of `canonical_name` or vice-versa (D2/D3 rules).
        // All 5 callsites gain this behavior via central enforcement (amendment AC3).
        if let Some(existing) = find_person_alias(canonical_name, entity_type, stream_id, self)? {
            // Merge BOTH canonical_name and caller-supplied aliases (regression of
            // cycle/131: only canonical_name was passed, silently dropping caller
            // aliases on this path; the other two paths preserve them).
            let mut to_merge = Vec::with_capacity(aliases.len() + 1);
            to_merge.push(canonical_name.to_string());
            to_merge.extend_from_slice(aliases);
            let merged = self.merge_aliases_if_new(existing, &sp, &to_merge)?;
            return Ok(merged);
        }
        self.create_new_entity_with_indexes(&sp, canonical_name, entity_type, aliases, stream_id)
    }

    pub fn get_entity_by_name(&self, name: &str, stream_id: &str) -> Result<Option<EntityNode>> {
        let sp = self.stream_prefix(stream_id);
        let token = self.name_token(stream_id, name)?;
        let name_key = format!("{}name:{}", sp, token);
        if let Some(entity_id) = self.get_string(&name_key)? {
            return self.get_entity_by_id(&entity_id);
        }
        // Try alias lookup
        let alias_key = format!("{}alias:{}", sp, token);
        if let Some(entity_id) = self.get_string(&alias_key)? {
            return self.get_entity_by_id(&entity_id);
        }
        Ok(None)
    }

    /// /138 §D: read-side inverse of `store_entity`. Reconstitutes an
    /// `EntityNode` from raw bytes: parses the flattened routing fields and — if
    /// `encrypted_payload` is present — decrypts `(canonical_name, aliases)`
    /// under the entity's own `stream_id` DEK. Legacy (pre-/138 plaintext) rows
    /// have no `encrypted_payload` and return as-is. Public so the
    /// `loomem-server` crate's prefix-scan read paths (dashboard entity list)
    /// can reconstitute entities without bypassing decryption.
    pub fn decode_entity(&self, bytes: &[u8]) -> Result<EntityNode> {
        let staged: StoredEntityRead =
            serde_json::from_slice(bytes).context("Failed to deserialize entity envelope")?;
        let mut entity = staged.entity;
        if staged.encrypted_payload.is_empty() {
            // Legacy fall-through: pre-/138 entity had plaintext canonical_name/
            // aliases populated directly. Removed post-/F backfill (cycle TBD).
            return Ok(entity);
        }
        let payload = self
            .store
            .encryption_provider()
            .decrypt(&entity.stream_id, &staged.encrypted_payload)
            .context("Failed to decrypt entity payload")?;
        let (canonical_name, aliases): (String, Vec<String>) =
            serde_json::from_slice(&payload).context("Failed to deserialize entity payload")?;
        entity.canonical_name = canonical_name;
        entity.aliases = aliases;
        Ok(entity)
    }

    pub fn get_entity_by_id(&self, id: &str) -> Result<Option<EntityNode>> {
        let key = format!("graph:entity:{}", id);
        match self.store.get(key.as_bytes())? {
            Some(bytes) => match self.decode_entity(&bytes) {
                Ok(entity) => Ok(Some(entity)),
                Err(err) => {
                    // Read-path fail-graceful (cycle/117): corrupted/legacy entity bytes
                    // are treated as missing so the delete chain (remove_chunk_references,
                    // delete_entity) can make progress instead of returning HTTP 500.
                    // Write paths still call store_entity which is strict on serialize.
                    warn!(
                        entity_id = %id,
                        error = %err,
                        "graph: get_entity_by_id deserialize failed — treating entity as missing (fail-graceful)"
                    );
                    Ok(None)
                }
            },
            None => Ok(None),
        }
    }

    /// Delete an entity and all its edges, indexes, and reverse references.
    pub fn delete_entity(&self, entity_id: &str) -> Result<bool> {
        let entity = match self.get_entity_by_id(entity_id)? {
            Some(e) => e,
            None => return Ok(false),
        };

        let sp = self.stream_prefix(&entity.stream_id);

        // Remove name index
        let name_token = self.name_token(&entity.stream_id, &entity.canonical_name)?;
        let name_key = format!("{}name:{}", sp, name_token);
        let _ = self.store.delete(name_key.as_bytes());

        // Remove alias indexes
        for alias in &entity.aliases {
            let alias_token = self.name_token(&entity.stream_id, alias)?;
            let alias_key = format!("{}alias:{}", sp, alias_token);
            let _ = self.store.delete(alias_key.as_bytes());
        }
        // Also remove canonical name alias
        let cn_alias_key = format!("{}alias:{}", sp, name_token);
        let _ = self.store.delete(cn_alias_key.as_bytes());

        // Remove chunk reverse indexes that reference this entity
        for cid in &entity.chunk_ids {
            let chunk_key = format!("graph:chunk:{}", cid);
            // Read existing, remove this entity_id, write back or delete
            if let Ok(Some(bytes)) = self.store.get(chunk_key.as_bytes()) {
                let ids_str = String::from_utf8_lossy(&bytes);
                let remaining: Vec<&str> =
                    ids_str.split(',').filter(|id| *id != entity_id).collect();
                if remaining.is_empty() {
                    let _ = self.store.delete(chunk_key.as_bytes());
                } else {
                    let _ = self
                        .store
                        .put(chunk_key.as_bytes(), remaining.join(",").as_bytes());
                }
            }
        }

        // Remove all edges involving this entity + their adjacency indexes
        // Outgoing edges
        let adj_prefix = format!("graph:adj:{}:", entity_id);
        let adj_keys: Vec<(String, String)> = self
            .store
            .prefix_scan(adj_prefix.as_bytes())
            .map(|(k, _)| {
                let key_str = String::from_utf8_lossy(&k).to_string();
                let edge_id = key_str.strip_prefix(&adj_prefix).unwrap_or("").to_string();
                (key_str, edge_id)
            })
            .collect();
        for (adj_key, edge_id) in &adj_keys {
            let _ = self.store.delete(adj_key.as_bytes());
            if let Ok(Some(edge)) = self.get_edge_by_id(edge_id) {
                // Remove reverse adjacency on the other side
                let radj_key = format!("graph:radj:{}:{}", edge.target_entity_id, edge_id);
                let _ = self.store.delete(radj_key.as_bytes());
                // Remove edge itself
                let edge_key = format!("graph:edge:{}", edge_id);
                let _ = self.store.delete(edge_key.as_bytes());
            }
        }

        // Incoming edges
        let radj_prefix = format!("graph:radj:{}:", entity_id);
        let radj_keys: Vec<(String, String)> = self
            .store
            .prefix_scan(radj_prefix.as_bytes())
            .map(|(k, _)| {
                let key_str = String::from_utf8_lossy(&k).to_string();
                let edge_id = key_str.strip_prefix(&radj_prefix).unwrap_or("").to_string();
                (key_str, edge_id)
            })
            .collect();
        for (radj_key, edge_id) in &radj_keys {
            let _ = self.store.delete(radj_key.as_bytes());
            if let Ok(Some(edge)) = self.get_edge_by_id(edge_id) {
                let adj_key = format!("graph:adj:{}:{}", edge.source_entity_id, edge_id);
                let _ = self.store.delete(adj_key.as_bytes());
                let edge_key = format!("graph:edge:{}", edge_id);
                let _ = self.store.delete(edge_key.as_bytes());
            }
        }

        // Remove the entity itself
        let entity_key = format!("graph:entity:{}", entity_id);
        let _ = self.store.delete(entity_key.as_bytes());

        debug!(
            "Deleted graph entity: {} ({})",
            entity.canonical_name, entity_id
        );
        Ok(true)
    }

    /// Add a chunk reference to an entity node.
    pub fn add_chunk_to_entity(&self, entity_id: &str, chunk_id: &str) -> Result<()> {
        if let Some(mut entity) = self.get_entity_by_id(entity_id)? {
            if !entity.chunk_ids.contains(&chunk_id.to_string()) {
                entity.chunk_ids.push(chunk_id.to_string());
                entity.updated_at = now_secs();
                self.store_entity(&entity)?;

                // Update reverse index: chunk -> entities
                self.add_to_chunk_index(chunk_id, entity_id)?;
            }
        }
        Ok(())
    }

    // --- Edge CRUD ---

    /// Get or create an edge between two entities, scoped to a stream.
    pub fn get_or_create_edge(
        &self,
        source_entity_id: &str,
        target_entity_id: &str,
        relation_type: &str,
        stream_id: &str,
    ) -> Result<Edge> {
        // Normalize raw relation string to a known RelationType (D4 rule).
        // Unknown strings map to related_to + warn log. Central enforcement
        // for all 5 callsites (amendment AC3).
        let normalized_relation = normalize_relation(relation_type);
        let relation_type = normalized_relation.as_str();
        // Check existing edges via adjacency list
        let adj_prefix = format!("graph:adj:{}:", source_entity_id);
        for (key, value) in self.store.prefix_scan(adj_prefix.as_bytes()) {
            let target_id = String::from_utf8_lossy(&value).to_string();
            if target_id == target_entity_id {
                // Extract edge_id from key: graph:adj:{source}:{edge_id}
                let key_str = String::from_utf8_lossy(&key);
                if let Some(edge_id) = key_str.strip_prefix(&adj_prefix) {
                    if let Some(edge) = self.get_edge_by_id(edge_id)? {
                        // Normalize the stored edge's relation_type before comparing:
                        // pre-PR rows hold raw strings (e.g. "works_at_tantivy"), and
                        // a naked == against the already-normalized incoming key would
                        // miss them, producing a duplicate canonical-type edge on the
                        // first re-ingestion. Normalization is idempotent on canonical
                        // values, so already-canonical edges still match.
                        if normalize_relation(&edge.relation_type).as_str() == relation_type {
                            return Ok(edge);
                        }
                    }
                }
            }
        }

        // Create new edge
        let id = uuid::Uuid::new_v4().to_string();
        let now = now_secs();
        let edge = Edge {
            id: id.clone(),
            source_entity_id: source_entity_id.to_string(),
            target_entity_id: target_entity_id.to_string(),
            relation_type: relation_type.to_string(),
            chunk_ids: Vec::new(),
            stream_id: stream_id.to_string(),
            created_at: now,
            updated_at: now,
        };

        self.store_edge(&edge)?;

        // Adjacency indexes
        self.put_string(
            &format!("graph:adj:{}:{}", source_entity_id, id),
            target_entity_id,
        )?;
        self.put_string(
            &format!("graph:radj:{}:{}", target_entity_id, id),
            source_entity_id,
        )?;

        debug!(
            "Created graph edge: {} --[{}]--> {} [stream={}]",
            source_entity_id, relation_type, target_entity_id, stream_id
        );
        Ok(edge)
    }

    /// Add a chunk reference to an edge.
    pub fn add_chunk_to_edge(&self, edge_id: &str, chunk_id: &str) -> Result<()> {
        if let Some(mut edge) = self.get_edge_by_id(edge_id)? {
            if !edge.chunk_ids.contains(&chunk_id.to_string()) {
                edge.chunk_ids.push(chunk_id.to_string());
                edge.updated_at = now_secs();
                self.store_edge(&edge)?;
            }
        }
        Ok(())
    }

    fn get_edge_by_id(&self, id: &str) -> Result<Option<Edge>> {
        let key = format!("graph:edge:{}", id);
        match self.store.get(key.as_bytes())? {
            Some(bytes) => {
                let edge: Edge =
                    serde_json::from_slice(&bytes).context("Failed to deserialize edge")?;
                Ok(Some(edge))
            }
            None => Ok(None),
        }
    }

    // --- Traversal ---

    /// Get 1-hop neighbors of an entity (outgoing + incoming edges).
    pub fn get_neighbors(&self, entity_id: &str) -> Result<Vec<(Edge, EntityNode)>> {
        let mut results = Vec::new();

        // Outgoing edges
        let adj_prefix = format!("graph:adj:{}:", entity_id);
        for (key, _value) in self.store.prefix_scan(adj_prefix.as_bytes()) {
            let key_str = String::from_utf8_lossy(&key);
            if let Some(edge_id) = key_str.strip_prefix(&adj_prefix) {
                if let Some(edge) = self.get_edge_by_id(edge_id)? {
                    if let Some(target) = self.get_entity_by_id(&edge.target_entity_id)? {
                        results.push((edge, target));
                    }
                }
            }
        }

        // Incoming edges
        let radj_prefix = format!("graph:radj:{}:", entity_id);
        for (key, _value) in self.store.prefix_scan(radj_prefix.as_bytes()) {
            let key_str = String::from_utf8_lossy(&key);
            if let Some(edge_id) = key_str.strip_prefix(&radj_prefix) {
                if let Some(edge) = self.get_edge_by_id(edge_id)? {
                    if let Some(source) = self.get_entity_by_id(&edge.source_entity_id)? {
                        results.push((edge, source));
                    }
                }
            }
        }

        Ok(results)
    }

    /// Get 2-hop neighbors: entity -> neighbor -> neighbor's neighbor.
    pub fn get_neighbors_2hop(&self, entity_id: &str) -> Result<Vec<(Vec<Edge>, EntityNode)>> {
        let mut results = Vec::new();
        let mut visited = std::collections::HashSet::new();
        visited.insert(entity_id.to_string());

        let hop1 = self.get_neighbors(entity_id)?;
        for (_edge, neighbor1) in &hop1 {
            visited.insert(neighbor1.id.clone());
        }

        for (edge1, neighbor1) in &hop1 {
            let hop2 = self.get_neighbors(&neighbor1.id)?;
            for (edge2, neighbor2) in hop2 {
                if !visited.contains(&neighbor2.id) {
                    visited.insert(neighbor2.id.clone());
                    results.push((vec![edge1.clone(), edge2], neighbor2));
                }
            }
        }

        Ok(results)
    }

    /// Get all chunk IDs referenced by an entity.
    pub fn get_entity_chunks(&self, entity_id: &str) -> Result<Vec<String>> {
        if let Some(entity) = self.get_entity_by_id(entity_id)? {
            Ok(entity.chunk_ids)
        } else {
            Ok(Vec::new())
        }
    }

    /// Find related chunks via graph traversal from query entities (stream-scoped).
    /// Returns (chunk_id, proximity_score) where score = 1.0 for direct, 0.5 for 1-hop.
    pub fn find_related_chunks(
        &self,
        entity_names: &[String],
        max_hops: usize,
        stream_id: &str,
    ) -> Result<Vec<(String, f64)>> {
        let mut chunk_scores: std::collections::HashMap<String, f64> =
            std::collections::HashMap::new();

        for name in entity_names {
            if let Some(entity) = self.get_entity_by_name(name, stream_id)? {
                // Direct chunks: score 1.0
                for cid in &entity.chunk_ids {
                    let entry = chunk_scores.entry(cid.clone()).or_insert(0.0);
                    *entry = entry.max(1.0);
                }

                // 1-hop neighbors
                if max_hops >= 1 {
                    let neighbors = self.get_neighbors(&entity.id)?;
                    for (_, neighbor) in &neighbors {
                        for cid in &neighbor.chunk_ids {
                            let entry = chunk_scores.entry(cid.clone()).or_insert(0.0);
                            *entry = entry.max(0.5);
                        }
                    }
                }

                // 2-hop neighbors
                if max_hops >= 2 {
                    let hop2 = self.get_neighbors_2hop(&entity.id)?;
                    for (_, neighbor) in &hop2 {
                        for cid in &neighbor.chunk_ids {
                            let entry = chunk_scores.entry(cid.clone()).or_insert(0.0);
                            *entry = entry.max(0.25);
                        }
                    }
                }
            }
        }

        let mut results: Vec<_> = chunk_scores.into_iter().collect();
        results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        Ok(results)
    }

    // --- Reverse index ---

    /// Get all entity IDs for a chunk.
    pub fn get_entities_for_chunk(&self, chunk_id: &str) -> Result<Vec<String>> {
        let key = format!("graph:chunk:{}", chunk_id);
        match self.store.get(key.as_bytes())? {
            Some(bytes) => {
                let ids: Vec<String> = serde_json::from_slice(&bytes)
                    .context("Failed to deserialize chunk entity index")?;
                Ok(ids)
            }
            None => Ok(Vec::new()),
        }
    }

    /// Remove all graph references for a deleted chunk.
    ///
    /// Entities that lose their last chunk are pruned outright (entity +
    /// edges + indexes), not left behind with an empty `chunk_ids`. A ghost
    /// entity with `Linked chunks: 0` surviving `memory_delete` is a
    /// GDPR/erasure correctness bug — the deletion contract promises the graph
    /// is cleaned too.
    pub fn remove_chunk_references(&self, chunk_id: &str) -> Result<()> {
        let entity_ids = self.get_entities_for_chunk(chunk_id)?;
        let mut to_prune: Vec<String> = Vec::new();

        for entity_id in &entity_ids {
            if let Some(mut entity) = self.get_entity_by_id(entity_id)? {
                entity.chunk_ids.retain(|c| c != chunk_id);
                if entity.chunk_ids.is_empty() {
                    // Orphaned: defer to delete_entity below (which also tears
                    // down edges/indexes). Skip store_entity — pointless write.
                    to_prune.push(entity_id.clone());
                } else {
                    entity.updated_at = now_secs();
                    self.store_entity(&entity)?;
                }
            }
        }

        // Remove reverse index
        let key = format!("graph:chunk:{}", chunk_id);
        self.store.delete(key.as_bytes())?;

        // Prune orphaned entities (best-effort: a failure here leaves an
        // orphan that a later delete can retry, but must not abort the rest).
        for entity_id in &to_prune {
            if let Err(err) = self.delete_entity(entity_id) {
                tracing::error!(
                    entity_id = %entity_id,
                    error = %err,
                    "graph: delete_entity failed during chunk reference cleanup — orphan entity may remain, retry required"
                );
            }
        }

        debug!(
            "Removed graph references for chunk {} (pruned {} orphan entities)",
            chunk_id,
            to_prune.len()
        );
        Ok(())
    }

    // --- Stats ---

    /// Count all entities (global — admin use).
    pub fn count_entities(&self) -> Result<usize> {
        Ok(self.store.prefix_scan(b"graph:entity:").count())
    }

    /// Count all edges (global — admin use).
    pub fn count_edges(&self) -> Result<usize> {
        Ok(self.store.prefix_scan(b"graph:edge:").count())
    }

    /// Re-key the reverse-name index from plaintext suffixes to HMAC tokens.
    ///
    /// AC-E3 migration (ADR-013 §4 Decision 2). For each `graph:entity:*` row:
    ///
    /// 1. Decode the entity (may decrypt PII via §D read path).
    /// 2. Compute token keys for canonical_name + all aliases.
    /// 3. Write the token-keyed index rows (idempotent: same token every run).
    /// 4. Scan `{sp}name:*` and `{sp}alias:*` for entries pointing at this
    ///    entity's ID; delete any entry whose key suffix differs from the
    ///    expected token (those are old plaintext-suffix rows).
    ///
    /// Under `NoopProvider`, `index_token` returns `plaintext.to_lowercase()` —
    /// the new key equals the old key, so no net change. The migration is safe
    /// to run repeatedly (idempotent). Returns `(migrated, already_current)`.
    pub fn rekey_name_index(&self) -> Result<(usize, usize)> {
        let entity_ids: Vec<String> = self
            .store
            .prefix_scan(b"graph:entity:")
            .map(|(k, _)| String::from_utf8_lossy(&k).to_string())
            .collect();

        let mut migrated = 0usize;
        let mut already_current = 0usize;

        for entity_key in &entity_ids {
            let entity_id = entity_key
                .strip_prefix("graph:entity:")
                .unwrap_or(entity_key);
            let entity = match self.get_entity_by_id(entity_id)? {
                Some(e) => e,
                None => continue, // corrupted row — skip, fail-graceful
            };
            let sp = self.stream_prefix(&entity.stream_id);
            let result = self.rekey_entity_indexes(&entity, &sp)?;
            migrated += result.0;
            already_current += result.1;
        }
        Ok((migrated, already_current))
    }

    /// Re-key name and alias index rows for a single entity.
    ///
    /// Returns `(rows_migrated, rows_already_current)`.
    fn rekey_entity_indexes(&self, entity: &EntityNode, sp: &str) -> Result<(usize, usize)> {
        let stream_id = &entity.stream_id;
        // Compute expected token for canonical_name.
        let name_token = self.name_token(stream_id, &entity.canonical_name)?;
        // Tokens for all aliases (and canonical_name-as-alias).
        let mut alias_tokens: Vec<String> = Vec::with_capacity(entity.aliases.len() + 1);
        for alias in &entity.aliases {
            alias_tokens.push(self.name_token(stream_id, alias)?);
        }
        alias_tokens.push(name_token.clone()); // canonical_name-as-alias

        // Write token-keyed entries (idempotent).
        let name_key = format!("{}name:{}", sp, name_token);
        self.put_string(&name_key, &entity.id)?;
        // alias_tokens[0..aliases.len()] align with entity.aliases; write each.
        for token in alias_tokens.iter().take(entity.aliases.len()) {
            let alias_key = format!("{}alias:{}", sp, token);
            self.put_string(&alias_key, &entity.id)?;
        }
        // Also write canonical_name-as-alias token.
        let cn_alias_key = format!("{}alias:{}", sp, name_token);
        self.put_string(&cn_alias_key, &entity.id)?;

        // Collect all expected key suffixes (for old-key pruning).
        let expected_name_suffix = name_token.clone();
        let expected_alias_suffixes: Vec<String> = alias_tokens;

        // Prune old plaintext-suffix name entries pointing at this entity.
        let mut migrated = 0usize;
        let mut already_current = 0usize;
        let name_prefix = format!("{}name:", sp);
        let old_name_keys: Vec<String> = self
            .store
            .prefix_scan(name_prefix.as_bytes())
            .filter(|(_, v)| v.as_ref() == entity.id.as_bytes())
            .map(|(k, _)| String::from_utf8_lossy(&k).to_string())
            .collect();
        for old_key in old_name_keys {
            let suffix = old_key
                .strip_prefix(&name_prefix)
                .unwrap_or(&old_key)
                .to_string();
            if suffix == expected_name_suffix {
                already_current += 1;
            } else {
                let _ = self.store.delete(old_key.as_bytes());
                migrated += 1;
            }
        }

        // Prune old plaintext-suffix alias entries pointing at this entity.
        let alias_prefix = format!("{}alias:", sp);
        let old_alias_keys: Vec<String> = self
            .store
            .prefix_scan(alias_prefix.as_bytes())
            .filter(|(_, v)| v.as_ref() == entity.id.as_bytes())
            .map(|(k, _)| String::from_utf8_lossy(&k).to_string())
            .collect();
        for old_key in old_alias_keys {
            let suffix = old_key
                .strip_prefix(&alias_prefix)
                .unwrap_or(&old_key)
                .to_string();
            if expected_alias_suffixes.contains(&suffix) {
                already_current += 1;
            } else {
                let _ = self.store.delete(old_key.as_bytes());
                migrated += 1;
            }
        }

        Ok((migrated, already_current))
    }

    /// Scan all entity nodes in a stream (via the name index).
    ///
    /// Used by `alias::find_person_alias` to iterate existing Person entities
    /// for substring-match dedup. Results are sorted by `created_at` ascending
    /// to implement existing-first (D3) rule.
    pub(crate) fn scan_entities_in_stream(&self, stream_id: &str) -> Result<Vec<EntityNode>> {
        let sp = self.stream_prefix(stream_id);
        let name_prefix = format!("{}name:", sp);
        let mut entities = Vec::new();
        for (_k, v) in self.store.prefix_scan(name_prefix.as_bytes()) {
            let entity_id = String::from_utf8_lossy(&v).to_string();
            if let Some(entity) = self.get_entity_by_id(&entity_id)? {
                entities.push(entity);
            }
        }
        entities.sort_by_key(|e| e.created_at);
        Ok(entities)
    }

    // --- Internal helpers ---

    /// Key prefix for stream-scoped indexes: `graph:s:{stream_id}:`
    pub(crate) fn stream_prefix(&self, stream_id: &str) -> String {
        format!("graph:s:{}:", stream_id)
    }

    pub(crate) fn store_entity(&self, entity: &EntityNode) -> Result<()> {
        let key = format!("graph:entity:{}", entity.id);
        // /138 phase 3b: field-level encryption (replaces /134 §C whole-blob).
        // The PII fields (canonical_name, aliases) are serialized as a serde_json
        // tuple, encrypted under the entity's stream_id DEK, and stored in
        // `encrypted_payload`; their plaintext copies are cleared. The routing
        // envelope (id, stream_id, entity_type, chunk_ids, created_at, updated_at)
        // stays plaintext so readers resolve scope without decrypting. NoopProvider
        // (encryption disabled) keeps the pre-/138 plaintext layout byte-identical.
        // Only the `graph:entity:*` row class is encrypted; adjacency/edge/index
        // rows stay plaintext per ADR-013 scope.
        let provider = self.store.encryption_provider();
        let value = if provider.is_enabled() {
            let payload = serde_json::to_vec(&(&entity.canonical_name, &entity.aliases))
                .context("Failed to serialize entity payload")?;
            let encrypted = provider
                .encrypt(&entity.stream_id, &payload)
                .context("Failed to encrypt entity payload")?;
            let mut cleared = entity.clone();
            cleared.canonical_name = String::new();
            cleared.aliases = Vec::new();
            serde_json::to_vec(&StoredEntity {
                entity: &cleared,
                encrypted_payload: encrypted,
            })
            .context("Failed to serialize entity envelope")?
        } else {
            serde_json::to_vec(entity).context("Failed to serialize entity")?
        };
        self.store.put(key.as_bytes(), &value)?;
        Ok(())
    }

    fn store_edge(&self, edge: &Edge) -> Result<()> {
        let key = format!("graph:edge:{}", edge.id);
        let value = serde_json::to_vec(edge).context("Failed to serialize edge")?;
        self.store.put(key.as_bytes(), &value)?;
        Ok(())
    }

    fn get_string(&self, key: &str) -> Result<Option<String>> {
        match self.store.get(key.as_bytes())? {
            Some(bytes) => Ok(Some(String::from_utf8_lossy(&bytes).to_string())),
            None => Ok(None),
        }
    }

    fn put_string(&self, key: &str, value: &str) -> Result<()> {
        self.store.put(key.as_bytes(), value.as_bytes())?;
        Ok(())
    }

    /// Probe the canonical-name index.
    ///
    /// Returns:
    /// - `Resolved(entity)` — index hit + entity bytes readable; aliases merged.
    /// - `DanglingId(id)` — index pointed at `id` but `get_entity_by_id` returned
    ///   `Ok(None)` (key may be absent or corrupt — callers disambiguate via
    ///   `entity_key_present`).
    /// - `NoHit` — no name-index entry.
    fn try_resolve_by_name(
        &self,
        sp: &str,
        stream_id: &str,
        canonical_name: &str,
        aliases: &[String],
    ) -> Result<ResolveOutcome> {
        let token = self.name_token(stream_id, canonical_name)?;
        let name_key = format!("{}name:{}", sp, token);
        if let Some(entity_id) = self.get_string(&name_key)? {
            if let Some(entity) = self.get_entity_by_id(&entity_id)? {
                let merged = self.merge_aliases_if_new(entity, sp, aliases)?;
                return Ok(ResolveOutcome::Resolved(merged));
            }
            return Ok(ResolveOutcome::DanglingId(entity_id));
        }
        Ok(ResolveOutcome::NoHit)
    }

    /// Probe each alias against the alias index.
    ///
    /// Returns one `ResolveOutcome` per alias that has an index entry.
    /// Aliases with no index entry produce no element in the output.
    /// Short-circuits on the first `Resolved` hit: remaining aliases are not
    /// read from RocksDB (avoids per-call read-amplification regression for
    /// entities with many aliases). `DanglingId` outcomes accumulated before
    /// the first resolved hit are included in the returned `Vec`.
    fn try_resolve_by_alias(
        &self,
        sp: &str,
        stream_id: &str,
        aliases: &[String],
    ) -> Result<Vec<ResolveOutcome>> {
        let mut outcomes = Vec::new();
        for alias in aliases {
            let token = self.name_token(stream_id, alias)?;
            let alias_key = format!("{}alias:{}", sp, token);
            if let Some(entity_id) = self.get_string(&alias_key)? {
                if let Some(entity) = self.get_entity_by_id(&entity_id)? {
                    outcomes.push(ResolveOutcome::Resolved(entity));
                    // short-circuit: remaining aliases unneeded (AC-18)
                    return Ok(outcomes);
                }
                outcomes.push(ResolveOutcome::DanglingId(entity_id));
            }
        }
        Ok(outcomes)
    }

    /// Run name probe then alias probes, accumulate dangling IDs, and
    /// short-circuit as soon as a resolved entity is found.
    ///
    /// Returns `(Some(entity), _)` when resolved, or `(None, probed_ids)`
    /// when all probes miss or hit only dangling/corrupt index targets.
    /// Extracted from `get_or_create_entity` to keep the orchestrator CC ≤ 10.
    fn probe_entity_indexes(
        &self,
        sp: &str,
        stream_id: &str,
        canonical_name: &str,
        aliases: &[String],
    ) -> Result<(Option<EntityNode>, Vec<String>)> {
        let mut probed_ids: Vec<String> = Vec::new();

        match self.try_resolve_by_name(sp, stream_id, canonical_name, aliases)? {
            ResolveOutcome::Resolved(entity) => return Ok((Some(entity), Vec::new())),
            ResolveOutcome::DanglingId(id) => probed_ids.push(id),
            ResolveOutcome::NoHit => {}
        }

        for outcome in self.try_resolve_by_alias(sp, stream_id, aliases)? {
            match outcome {
                ResolveOutcome::Resolved(entity) => return Ok((Some(entity), Vec::new())),
                ResolveOutcome::DanglingId(id) => probed_ids.push(id),
                ResolveOutcome::NoHit => {}
            }
        }

        probed_ids.sort();
        probed_ids.dedup();
        Ok((None, probed_ids))
    }

    /// Raw RocksDB existence probe — does NOT deserialize.
    ///
    /// Distinguishes:
    /// - `Ok(false)` → key absent (dangling index → safe to overwrite).
    /// - `Ok(true)` → key bytes physically present (combined with
    ///   `get_entity_by_id == Ok(None)` in caller context → corruption).
    fn entity_key_present(&self, id: &str) -> Result<bool> {
        let key = format!("graph:entity:{}", id);
        Ok(self.store.get(key.as_bytes())?.is_some())
    }

    /// Guard: if any of `probed_ids` maps to a `graph:entity:*` key that
    /// exists in storage but is unreadable, return `Err` naming all offenders.
    ///
    /// Called by `get_or_create_entity` after the probe phase and before
    /// `create_new_entity_with_indexes` to prevent silent index overwrite
    /// on corrupted-but-present entity bytes.
    fn refuse_if_any_unreadable_present(&self, probed_ids: &[String]) -> Result<()> {
        let mut unreadable = Vec::new();
        for id in probed_ids {
            if self.entity_key_present(id)? {
                unreadable.push(id.clone());
            }
        }
        if !unreadable.is_empty() {
            anyhow::bail!(
                "graph: refusing get_or_create — index target entity present but unreadable: {:?}",
                unreadable
            );
        }
        Ok(())
    }

    /// Mint a new entity, persist it, and write all index entries
    /// (name index + per-alias indexes + canonical-name-as-alias index).
    fn create_new_entity_with_indexes(
        &self,
        sp: &str,
        canonical_name: &str,
        entity_type: &str,
        aliases: &[String],
        stream_id: &str,
    ) -> Result<EntityNode> {
        let id = uuid::Uuid::new_v4().to_string();
        let now = now_secs();
        let entity = EntityNode {
            id: id.clone(),
            canonical_name: canonical_name.to_string(),
            entity_type: entity_type.to_string(),
            aliases: aliases.to_vec(),
            chunk_ids: Vec::new(),
            stream_id: stream_id.to_string(),
            created_at: now,
            updated_at: now,
        };

        self.store_entity(&entity)?;

        // Name index
        let name_token = self.name_token(stream_id, canonical_name)?;
        self.put_string(&format!("{}name:{}", sp, name_token), &id)?;

        // Alias indexes
        for alias in aliases {
            let alias_token = self.name_token(stream_id, alias)?;
            self.put_string(&format!("{}alias:{}", sp, alias_token), &id)?;
        }
        // Also index the canonical name as alias
        self.put_string(&format!("{}alias:{}", sp, name_token), &id)?;

        debug!(
            "Created graph entity: {} ({}) [stream={}]",
            canonical_name, entity_type, stream_id
        );
        Ok(entity)
    }

    /// Merge any aliases not already present (case-insensitive) into `entity`.
    /// Writes the alias index entry and bumps `updated_at` + persists only if
    /// at least one alias was actually added.
    fn merge_aliases_if_new(
        &self,
        mut entity: EntityNode,
        sp: &str,
        new_aliases: &[String],
    ) -> Result<EntityNode> {
        let mut changed = false;
        for alias in new_aliases {
            // In-memory dedup check remains .to_lowercase() (case-insensitive
            // comparison of the stored alias strings — not an index key lookup).
            let lower = alias.to_lowercase();
            if !entity.aliases.iter().any(|a| a.to_lowercase() == lower) {
                entity.aliases.push(alias.clone());
                // Index key uses the provider-owned token (AC-E1 §E).
                let token = self.name_token(&entity.stream_id, alias)?;
                self.put_string(&format!("{}alias:{}", sp, token), &entity.id)?;
                changed = true;
            }
        }
        if changed {
            entity.updated_at = now_secs();
            self.store_entity(&entity)?;
        }
        Ok(entity)
    }

    /// Compute the HMAC index token for a name/alias under `stream_id`.
    /// Delegates to `EncryptionProvider::index_token`; lowercasing is
    /// provider-owned (ADR-013 §4, AC-E1). Error is wrapped with context.
    fn name_token(&self, stream_id: &str, name: &str) -> Result<String> {
        self.store
            .encryption_provider()
            .index_token(stream_id, name)
            .with_context(|| format!("index_token failed for stream {stream_id}"))
    }

    fn add_to_chunk_index(&self, chunk_id: &str, entity_id: &str) -> Result<()> {
        let key = format!("graph:chunk:{}", chunk_id);
        let mut ids = self.get_entities_for_chunk(chunk_id)?;
        if !ids.contains(&entity_id.to_string()) {
            ids.push(entity_id.to_string());
            let value =
                serde_json::to_vec(&ids).context("Failed to serialize chunk entity index")?;
            self.store.put(key.as_bytes(), &value)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RocksDbConfig;

    fn test_store() -> (Arc<RocksDbStore>, tempfile::TempDir) {
        let tmp = tempfile::TempDir::new().unwrap();
        let config = RocksDbConfig {
            max_open_files: 100,
            compression: "none".to_string(),
            write_buffer_size: 4 * 1024 * 1024,
            max_write_buffer_number: 2,
        };
        let store = Arc::new(RocksDbStore::open(tmp.path(), &config).unwrap());
        (store, tmp)
    }

    const STREAM: &str = "test-stream";

    // /138 phase 3b: field-level encryption write path under MasterKeyEnvProvider.
    // The envelope (id/stream_id/entity_type/chunk_ids/timestamps) stays plaintext;
    // canonical_name + aliases are cleared and round-trip through encrypted_payload.
    #[test]
    fn entity_field_level_encryption_roundtrip() {
        use crate::crypto::at_rest::MAGIC;
        use crate::crypto::provider::{EncryptionProvider, MasterKeyEnvProvider};

        let tmp = tempfile::TempDir::new().unwrap();
        let config = RocksDbConfig {
            max_open_files: 100,
            compression: "none".to_string(),
            write_buffer_size: 4 * 1024 * 1024,
            max_write_buffer_number: 2,
        };
        let store = RocksDbStore::open(tmp.path(), &config).unwrap();
        let provider = Arc::new(MasterKeyEnvProvider::new([7u8; 32], store.db_arc()));
        let store = Arc::new(store.with_encryption_provider(provider.clone()));
        let graph = GraphStore::new(store.clone());

        let entity = EntityNode {
            id: "ent-1".to_string(),
            canonical_name: "Alice Smith".to_string(),
            entity_type: "Person".to_string(),
            aliases: vec!["Ali".to_string(), "A. Smith".to_string()],
            chunk_ids: vec!["chunk-1".to_string()],
            stream_id: "test-scope-138".to_string(),
            created_at: 1000,
            updated_at: 1000,
        };
        graph.store_entity(&entity).unwrap();

        // Read the raw graph:entity envelope (plaintext routing + payload).
        let key = format!("graph:entity:{}", entity.id);
        let raw = store.db().get(key.as_bytes()).unwrap().unwrap();

        #[derive(serde::Deserialize)]
        struct Envelope {
            stream_id: String,
            canonical_name: String,
            aliases: Vec<String>,
            entity_type: String,
            encrypted_payload: Option<Vec<u8>>,
        }
        let env: Envelope = serde_json::from_slice(&raw).unwrap();

        // Envelope is plaintext; PII fields cleared; payload present.
        assert_eq!(env.stream_id, "test-scope-138");
        assert_eq!(env.entity_type, "Person"); // routing stays cleartext
        assert!(env.encrypted_payload.is_some());
        assert_eq!(env.canonical_name, "");
        assert!(env.aliases.is_empty());

        // Payload is a valid AES-GCM blob (MAGIC prefix).
        let ep = env.encrypted_payload.unwrap();
        assert_eq!(&ep[..4], &MAGIC[..]);

        // Manual decrypt yields the original (canonical_name, aliases).
        let plaintext = provider.decrypt("test-scope-138", &ep).unwrap();
        let (canonical_name, aliases): (String, Vec<String>) =
            serde_json::from_slice(&plaintext).unwrap();
        assert_eq!(canonical_name, "Alice Smith");
        assert_eq!(aliases, vec!["Ali".to_string(), "A. Smith".to_string()]);
    }

    // /138 §D AC-D6 #3: end-to-end Pattern B round-trip. store_entity encrypts
    // (canonical_name, aliases) under MasterKeyEnvProvider; get_entity_by_id →
    // decode_entity decrypts and repopulates the cleared PII fields.
    #[test]
    fn entity_write_then_read_roundtrip() {
        use crate::crypto::provider::MasterKeyEnvProvider;

        let tmp = tempfile::TempDir::new().unwrap();
        let config = RocksDbConfig {
            max_open_files: 100,
            compression: "none".to_string(),
            write_buffer_size: 4 * 1024 * 1024,
            max_write_buffer_number: 2,
        };
        let store = RocksDbStore::open(tmp.path(), &config).unwrap();
        let provider = Arc::new(MasterKeyEnvProvider::new([7u8; 32], store.db_arc()));
        let store = Arc::new(store.with_encryption_provider(provider));
        let graph = GraphStore::new(store);

        let entity = EntityNode {
            id: "ent-rt".to_string(),
            canonical_name: "Alice Smith".to_string(),
            entity_type: "Person".to_string(),
            aliases: vec!["Ali".to_string(), "A. Smith".to_string()],
            chunk_ids: vec!["c1".to_string()],
            stream_id: "scope-ent".to_string(),
            created_at: 1000,
            updated_at: 1000,
        };
        graph.store_entity(&entity).unwrap();

        let got = graph
            .get_entity_by_id("ent-rt")
            .unwrap()
            .expect("entity present");
        assert_eq!(got.canonical_name, "Alice Smith");
        assert_eq!(got.aliases, vec!["Ali".to_string(), "A. Smith".to_string()]);
        assert_eq!(got.stream_id, "scope-ent");
        assert_eq!(got.entity_type, "Person");
    }

    // /138 §D AC-D6 #4: cross-provider legacy fall-through. A plaintext entity
    // row (pre-/138 Noop write format) is read back correctly by an
    // encryption-enabled graph via decode_entity's empty-payload branch.
    #[test]
    fn entity_legacy_plaintext_fallthrough() {
        use crate::crypto::provider::MasterKeyEnvProvider;

        let tmp = tempfile::TempDir::new().unwrap();
        let config = RocksDbConfig {
            max_open_files: 100,
            compression: "none".to_string(),
            write_buffer_size: 4 * 1024 * 1024,
            max_write_buffer_number: 2,
        };
        let store = RocksDbStore::open(tmp.path(), &config).unwrap();

        let entity = EntityNode {
            id: "ent-leg".to_string(),
            canonical_name: "Legacy Person".to_string(),
            entity_type: "Person".to_string(),
            aliases: vec!["LP".to_string()],
            chunk_ids: vec!["c1".to_string()],
            stream_id: "scope-leg".to_string(),
            created_at: 1000,
            updated_at: 1000,
        };
        // Plant a legacy plaintext row (the Noop write branch's format).
        store
            .put(
                format!("graph:entity:{}", entity.id).as_bytes(),
                &serde_json::to_vec(&entity).unwrap(),
            )
            .unwrap();

        // Read with encryption enabled over the same DB.
        let provider = Arc::new(MasterKeyEnvProvider::new([7u8; 32], store.db_arc()));
        let graph = GraphStore::new(Arc::new(store.with_encryption_provider(provider)));
        let got = graph
            .get_entity_by_id("ent-leg")
            .unwrap()
            .expect("entity present");
        assert_eq!(got.canonical_name, "Legacy Person");
        assert_eq!(got.aliases, vec!["LP".to_string()]);
    }

    // /138 phase 3b: with the default NoopProvider the entity is stored plaintext,
    // with no encrypted_payload — byte-compatible with pre-/138 behavior.
    #[test]
    fn entity_noop_provider_no_encryption() {
        let (store, _tmp) = test_store();
        let graph = GraphStore::new(store.clone());

        let entity = EntityNode {
            id: "ent-noop".to_string(),
            canonical_name: "Bob".to_string(),
            entity_type: "Person".to_string(),
            aliases: vec!["Bobby".to_string()],
            chunk_ids: Vec::new(),
            stream_id: "test-scope-138".to_string(),
            created_at: 1000,
            updated_at: 1000,
        };
        graph.store_entity(&entity).unwrap();

        let key = format!("graph:entity:{}", entity.id);
        let raw = store.db().get(key.as_bytes()).unwrap().unwrap();

        #[derive(serde::Deserialize)]
        struct Envelope {
            stream_id: String,
            canonical_name: String,
            encrypted_payload: Option<Vec<u8>>,
        }
        let env: Envelope = serde_json::from_slice(&raw).unwrap();
        assert_eq!(env.canonical_name, "Bob");
        assert!(env.encrypted_payload.is_none());
        assert_eq!(env.stream_id, "test-scope-138");
    }

    #[test]
    fn test_create_entity() {
        let (store, _tmp) = test_store();
        let graph = GraphStore::new(store);

        let entity = graph
            .get_or_create_entity(
                "John Doe",
                "Person",
                &["John".to_string(), "JD".to_string()],
                STREAM,
            )
            .unwrap();

        assert_eq!(entity.canonical_name, "John Doe");
        assert_eq!(entity.entity_type, "Person");
        assert_eq!(entity.stream_id, STREAM);

        // Retrieve by name
        let found = graph.get_entity_by_name("John Doe", STREAM).unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().id, entity.id);
    }

    #[test]
    fn test_entity_dedup_by_name() {
        let (store, _tmp) = test_store();
        let graph = GraphStore::new(store);

        let e1 = graph
            .get_or_create_entity("John Doe", "Person", &[], STREAM)
            .unwrap();
        let e2 = graph
            .get_or_create_entity("John Doe", "Person", &[], STREAM)
            .unwrap();

        assert_eq!(e1.id, e2.id, "Same name should return same entity");
    }

    #[test]
    fn test_entity_stream_isolation() {
        let (store, _tmp) = test_store();
        let graph = GraphStore::new(store);

        let e1 = graph
            .get_or_create_entity("Cursor", "Tool", &[], "stream-a")
            .unwrap();
        let e2 = graph
            .get_or_create_entity("Cursor", "Tool", &[], "stream-b")
            .unwrap();

        assert_ne!(
            e1.id, e2.id,
            "Same name in different streams must be separate entities"
        );

        // Each stream sees only its own entity
        assert!(graph
            .get_entity_by_name("Cursor", "stream-a")
            .unwrap()
            .is_some());
        assert!(graph
            .get_entity_by_name("Cursor", "stream-b")
            .unwrap()
            .is_some());
        assert_eq!(
            graph
                .get_entity_by_name("Cursor", "stream-a")
                .unwrap()
                .unwrap()
                .id,
            e1.id,
        );
    }

    #[test]
    fn test_entity_dedup_by_alias() {
        let (store, _tmp) = test_store();
        let graph = GraphStore::new(store);

        let e1 = graph
            .get_or_create_entity("John Doe", "Person", &["JD".to_string()], STREAM)
            .unwrap();

        // Different canonical name but matching alias
        let e2 = graph.get_entity_by_name("JD", STREAM).unwrap();
        assert!(e2.is_some());
        assert_eq!(e2.unwrap().id, e1.id);
    }

    #[test]
    fn test_create_edge() {
        let (store, _tmp) = test_store();
        let graph = GraphStore::new(store);

        let e1 = graph
            .get_or_create_entity("John", "Person", &[], STREAM)
            .unwrap();
        let e2 = graph
            .get_or_create_entity("Acme Corp", "Organization", &[], STREAM)
            .unwrap();

        let edge = graph
            .get_or_create_edge(&e1.id, &e2.id, "works_at", STREAM)
            .unwrap();
        assert_eq!(edge.relation_type, "works_at");
        assert_eq!(edge.source_entity_id, e1.id);
        assert_eq!(edge.target_entity_id, e2.id);
        assert_eq!(edge.stream_id, STREAM);

        // Dedup: same edge returned
        let edge2 = graph
            .get_or_create_edge(&e1.id, &e2.id, "works_at", STREAM)
            .unwrap();
        assert_eq!(edge.id, edge2.id);
    }

    #[test]
    fn test_edge_dedup_matches_preexisting_raw_relation_type() {
        // Regression: edges written before relation-type normalization shipped
        // hold raw strings (e.g. "works_at_tantivy"). The dedup scan must
        // normalize the stored value before comparing, otherwise re-ingestion
        // writes a duplicate canonical-type edge for every legacy triplet.
        let (store, _tmp) = test_store();
        let graph = GraphStore::new(store);

        let e1 = graph
            .get_or_create_entity("Alice", "Person", &[], STREAM)
            .unwrap();
        let e2 = graph
            .get_or_create_entity("Acme Corp", "Organization", &[], STREAM)
            .unwrap();

        // Seed a pre-PR raw-type edge directly, bypassing normalization.
        let raw_id = uuid::Uuid::new_v4().to_string();
        let now = now_secs();
        let raw_edge = Edge {
            id: raw_id.clone(),
            source_entity_id: e1.id.clone(),
            target_entity_id: e2.id.clone(),
            relation_type: "works_at_tantivy".to_string(),
            chunk_ids: Vec::new(),
            stream_id: STREAM.to_string(),
            created_at: now,
            updated_at: now,
        };
        graph.store_edge(&raw_edge).unwrap();
        graph
            .put_string(&format!("graph:adj:{}:{}", e1.id, raw_id), &e2.id)
            .unwrap();
        graph
            .put_string(&format!("graph:radj:{}:{}", e2.id, raw_id), &e1.id)
            .unwrap();

        // Re-ingestion sends the same raw string; both sides normalize to
        // "related_to" and must dedup onto the seeded edge.
        let returned = graph
            .get_or_create_edge(&e1.id, &e2.id, "works_at_tantivy", STREAM)
            .unwrap();
        assert_eq!(
            returned.id, raw_id,
            "dedup must return the pre-existing raw-type edge, not create a new one"
        );

        // No second adjacency row should have been written.
        let adj_prefix = format!("graph:adj:{}:", e1.id);
        let adj_count = graph.store.prefix_scan(adj_prefix.as_bytes()).count();
        assert_eq!(adj_count, 1, "no duplicate adjacency row may be created");
    }

    #[test]
    fn test_1hop_neighbors() {
        let (store, _tmp) = test_store();
        let graph = GraphStore::new(store);

        let e1 = graph
            .get_or_create_entity("Alice", "Person", &[], STREAM)
            .unwrap();
        let e2 = graph
            .get_or_create_entity("Bob", "Person", &[], STREAM)
            .unwrap();
        graph
            .get_or_create_edge(&e1.id, &e2.id, "knows", STREAM)
            .unwrap();

        let neighbors = graph.get_neighbors(&e1.id).unwrap();
        assert_eq!(neighbors.len(), 1);
        assert_eq!(neighbors[0].1.canonical_name, "Bob");

        // Bob should see Alice as incoming neighbor
        let bob_neighbors = graph.get_neighbors(&e2.id).unwrap();
        assert_eq!(bob_neighbors.len(), 1);
        assert_eq!(bob_neighbors[0].1.canonical_name, "Alice");
    }

    #[test]
    fn test_2hop_neighbors() {
        let (store, _tmp) = test_store();
        let graph = GraphStore::new(store);

        let a = graph
            .get_or_create_entity("A", "Person", &[], STREAM)
            .unwrap();
        let b = graph
            .get_or_create_entity("B", "Person", &[], STREAM)
            .unwrap();
        let c = graph
            .get_or_create_entity("C", "Person", &[], STREAM)
            .unwrap();

        graph
            .get_or_create_edge(&a.id, &b.id, "knows", STREAM)
            .unwrap();
        graph
            .get_or_create_edge(&b.id, &c.id, "knows", STREAM)
            .unwrap();

        let hop2 = graph.get_neighbors_2hop(&a.id).unwrap();
        assert!(!hop2.is_empty(), "A should reach C via B");
        assert!(hop2.iter().any(|(_, n)| n.canonical_name == "C"));
    }

    #[test]
    fn test_chunk_reverse_index() {
        let (store, _tmp) = test_store();
        let graph = GraphStore::new(store);

        let entity = graph
            .get_or_create_entity("Test", "Person", &[], STREAM)
            .unwrap();
        graph.add_chunk_to_entity(&entity.id, "chunk-1").unwrap();
        graph.add_chunk_to_entity(&entity.id, "chunk-2").unwrap();

        let chunks = graph.get_entity_chunks(&entity.id).unwrap();
        assert_eq!(chunks.len(), 2);

        let entities_for_chunk = graph.get_entities_for_chunk("chunk-1").unwrap();
        assert_eq!(entities_for_chunk.len(), 1);
        assert_eq!(entities_for_chunk[0], entity.id);
    }

    /// Cycle/117 (a): `get_entity_by_id` must treat corrupted/malformed
    /// entity bytes as missing (return `Ok(None)`) instead of propagating
    /// the deserialize error up the delete chain. This is the read-path
    /// fail-graceful fix — pre-fix, this returned Err and the delete
    /// handler responded HTTP 500.
    #[test]
    fn test_get_entity_corrupted_bytes_returns_none() {
        let (store, _tmp) = test_store();
        let graph = GraphStore::new(store.clone());

        // Plant malformed bytes under graph:entity:<id> — not valid JSON.
        let entity_id = "corrupted-entity-id";
        let key = format!("graph:entity:{}", entity_id);
        store
            .put(key.as_bytes(), b"\xff\xfe definitely not json {")
            .unwrap();

        // Pre-fix this returned Err("Failed to deserialize entity node"),
        // which propagated to delete_memory_fully → HTTP 500.
        let result = graph.get_entity_by_id(entity_id);
        assert!(
            result.is_ok(),
            "fail-graceful: corrupted entity must NOT return Err, got {:?}",
            result
        );
        assert!(
            result.unwrap().is_none(),
            "corrupted entity must be treated as missing (Ok(None))"
        );
    }

    /// Cycle/117 (b): `EntityNode` deserialize must accept JSON with extra
    /// fields not in the current schema (forward-compat) and JSON missing
    /// fields that have defaults (backward-compat). This validates the
    /// `#[serde(default)]` per-field hooks and the absence of
    /// `#[serde(deny_unknown_fields)]`.
    #[test]
    fn test_get_entity_legacy_schema_no_unknown_fields() {
        let (store, _tmp) = test_store();
        let graph = GraphStore::new(store.clone());

        let entity_id = "legacy-entity-id";
        let key = format!("graph:entity:{}", entity_id);

        // Legacy JSON: known fields plus a "future_field" the current
        // struct does not know about. Also omits `aliases` (must default
        // to empty Vec via #[serde(default)]).
        let legacy_json = serde_json::json!({
            "id": entity_id,
            "canonical_name": "Legacy Entity",
            "entity_type": "Person",
            "chunk_ids": ["c1", "c2"],
            "stream_id": "test-stream",
            "created_at": 1_700_000_000_u64,
            "updated_at": 1_700_000_000_u64,
            "future_field": {"nested": [1, 2, 3]},
        });
        store
            .put(key.as_bytes(), legacy_json.to_string().as_bytes())
            .unwrap();

        let entity = graph
            .get_entity_by_id(entity_id)
            .expect("legacy schema must deserialize OK")
            .expect("entity must be Some");

        assert_eq!(entity.id, entity_id);
        assert_eq!(entity.canonical_name, "Legacy Entity");
        assert_eq!(entity.entity_type, "Person");
        assert_eq!(entity.chunk_ids, vec!["c1", "c2"]);
        assert!(
            entity.aliases.is_empty(),
            "missing `aliases` field must default to empty Vec via #[serde(default)]"
        );
    }

    #[test]
    fn test_remove_chunk_references() {
        let (store, _tmp) = test_store();
        let graph = GraphStore::new(store);

        let entity = graph
            .get_or_create_entity("Test", "Person", &[], STREAM)
            .unwrap();
        graph.add_chunk_to_entity(&entity.id, "chunk-1").unwrap();

        graph.remove_chunk_references("chunk-1").unwrap();

        // Singleton entity loses its last chunk → pruned, not left as a ghost.
        assert!(
            graph.get_entity_by_id(&entity.id).unwrap().is_none(),
            "orphaned entity must be pruned, not retained with empty chunk_ids"
        );

        let reverse = graph.get_entities_for_chunk("chunk-1").unwrap();
        assert!(reverse.is_empty());
    }

    /// GDPR/erasure: deleting the only chunk of an entity must prune the
    /// entity entirely — no ghost entity, no surviving edges/indexes.
    /// (Smoke-test repro: `memory_graph` for a deleted Person still returned
    /// the entity with `Linked chunks: 0`.)
    #[test]
    fn remove_chunk_references_prunes_singleton_entity() {
        let (store, _tmp) = test_store();
        let graph = GraphStore::new(store.clone());
        let stream = "stream-prune-singleton";

        // Person with one chunk, linked by an edge to an Org (also 1 chunk).
        let person = graph
            .get_or_create_entity("Zenozar Qwybbleton", "Person", &["ZQ".to_string()], stream)
            .unwrap();
        let org = graph
            .get_or_create_entity("Wibblecorp", "Organization", &[], stream)
            .unwrap();
        graph.add_chunk_to_entity(&person.id, "chunk-z").unwrap();
        graph.add_chunk_to_entity(&org.id, "chunk-o").unwrap();
        graph
            .get_or_create_edge(&person.id, &org.id, "works_at", stream)
            .unwrap();

        graph.remove_chunk_references("chunk-z").unwrap();

        // Entity itself is gone.
        assert!(
            graph.get_entity_by_id(&person.id).unwrap().is_none(),
            "orphaned Person must be pruned"
        );

        // No edges reference the pruned entity (outgoing adjacency).
        let adj_prefix = format!("graph:adj:{}:", person.id);
        assert_eq!(
            store.prefix_scan(adj_prefix.as_bytes()).count(),
            0,
            "outgoing adjacency for pruned entity must be gone"
        );
        // Incoming adjacency (reverse) for the pruned entity is gone too.
        let radj_prefix = format!("graph:radj:{}:", person.id);
        assert_eq!(
            store.prefix_scan(radj_prefix.as_bytes()).count(),
            0,
            "reverse adjacency for pruned entity must be gone"
        );
        // The edge row itself is gone (only the Org's chunk-bearing entity
        // remains, with no edges).
        assert_eq!(
            store.prefix_scan(b"graph:edge:").count(),
            0,
            "edge involving pruned entity must be removed"
        );

        // Name/alias index rows for the pruned entity are gone.
        let sp = format!("graph:s:{}:", stream);
        let name_orphans = store
            .prefix_scan(format!("{}name:", sp).as_bytes())
            .filter(|(_, v)| v.as_ref() == person.id.as_bytes())
            .count();
        let alias_orphans = store
            .prefix_scan(format!("{}alias:", sp).as_bytes())
            .filter(|(_, v)| v.as_ref() == person.id.as_bytes())
            .count();
        assert_eq!(name_orphans, 0, "name index for pruned entity must be gone");
        assert_eq!(
            alias_orphans, 0,
            "alias index for pruned entity must be gone"
        );

        // The Org (still chunk-bearing) survives untouched.
        assert!(
            graph.get_entity_by_id(&org.id).unwrap().is_some(),
            "chunk-bearing Org entity must survive"
        );
    }

    /// An entity sharing more than one chunk must NOT be pruned when only one
    /// of its chunks is removed — its remaining chunk and edges stay intact.
    #[test]
    fn remove_chunk_references_keeps_shared_entity() {
        let (store, _tmp) = test_store();
        let graph = GraphStore::new(store.clone());
        let stream = "stream-keep-shared";

        let person = graph
            .get_or_create_entity("Multi Chunk", "Person", &[], stream)
            .unwrap();
        let org = graph
            .get_or_create_entity("ShareCorp", "Organization", &[], stream)
            .unwrap();
        graph.add_chunk_to_entity(&person.id, "chunk-1").unwrap();
        graph.add_chunk_to_entity(&person.id, "chunk-2").unwrap();
        let edge = graph
            .get_or_create_edge(&person.id, &org.id, "works_at", stream)
            .unwrap();

        graph.remove_chunk_references("chunk-1").unwrap();

        let updated = graph
            .get_entity_by_id(&person.id)
            .unwrap()
            .expect("shared entity must survive losing one of two chunks");
        assert_eq!(
            updated.chunk_ids,
            vec!["chunk-2".to_string()],
            "only the removed chunk should be dropped"
        );

        // Edge untouched.
        assert!(
            graph.get_edge_by_id(&edge.id).unwrap().is_some(),
            "edge must remain when neither endpoint is pruned"
        );
        let adj_prefix = format!("graph:adj:{}:", person.id);
        assert_eq!(
            store.prefix_scan(adj_prefix.as_bytes()).count(),
            1,
            "outgoing adjacency must be untouched for a surviving entity"
        );
    }

    /// Calling `remove_chunk_references` twice for the same chunk must not
    /// error — the second call is a no-op (entity already pruned, reverse
    /// index already gone).
    #[test]
    fn remove_chunk_references_idempotent() {
        let (store, _tmp) = test_store();
        let graph = GraphStore::new(store);
        let stream = "stream-idempotent";

        let person = graph
            .get_or_create_entity("Once Only", "Person", &[], stream)
            .unwrap();
        graph.add_chunk_to_entity(&person.id, "chunk-x").unwrap();

        graph.remove_chunk_references("chunk-x").unwrap();
        // Second call must not panic or return Err.
        graph
            .remove_chunk_references("chunk-x")
            .expect("second remove_chunk_references must be a no-op, not an error");

        assert!(
            graph.get_entity_by_id(&person.id).unwrap().is_none(),
            "entity stays pruned after a repeated removal"
        );
    }

    // --- Cycle/123 strict-existence guard tests ---

    /// Cycle/123 (1): `get_or_create_entity` must refuse to overwrite the name
    /// index when the indexed entity UUID has bytes physically present in
    /// storage but is unreadable (corrupt). Without this guard, a fresh UUID
    /// would silently overwrite the name index, orphaning all
    /// `graph:chunk:*` reverse-index entries pointing at the corrupt UUID.
    ///
    /// Demonstration (AC-14):
    ///   Pre-state:  `graph:entity:OLD` has poison bytes; `{sp}name:acme` → `OLD`.
    ///   Call:       `get_or_create_entity("Acme", ...)`.
    ///   Expected:   `Err` whose message contains `OLD`; name-index unchanged;
    ///               `graph:entity:*` prefix-scan count unchanged (no new entity).
    #[test]
    fn test_get_or_create_entity_refuses_overwrite_on_corrupt_existing_name_hit() {
        let (store, _tmp) = test_store();
        let graph = GraphStore::new(store.clone());

        let old_uuid = "aaaaaaaa-0000-0000-0000-000000000001";
        let sp = format!("graph:s:{}:", STREAM);

        // Plant corrupt bytes under graph:entity:OLD.
        let entity_key = format!("graph:entity:{}", old_uuid);
        store
            .put(entity_key.as_bytes(), b"\xff\xfe corrupt entity bytes")
            .unwrap();

        // Write name index pointing at OLD_UUID.
        let name_key = format!("{}name:acme", sp);
        store.put(name_key.as_bytes(), old_uuid.as_bytes()).unwrap();

        // Capture pre-call entity prefix count.
        let count_before = store.prefix_scan(b"graph:entity:").count();
        assert_eq!(count_before, 1, "pre-state: exactly one graph:entity:* key");

        // Call get_or_create_entity with the same name — must return Err.
        let result = graph.get_or_create_entity("Acme", "Org", &[], STREAM);
        assert!(
            result.is_err(),
            "must refuse when name-index target is present-but-unreadable"
        );
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains(old_uuid),
            "error message must contain the offending UUID: got {:?}",
            err_msg
        );

        // Name-index value must be byte-equal pre-state (not overwritten).
        let name_val = store
            .get(name_key.as_bytes())
            .unwrap()
            .expect("name index must still exist");
        assert_eq!(
            name_val.as_slice(),
            old_uuid.as_bytes(),
            "name-index value must be byte-equal pre-state (not overwritten)"
        );

        // No new entity was minted.
        let count_after = store.prefix_scan(b"graph:entity:").count();
        assert_eq!(
            count_after, count_before,
            "prefix-scan count of graph:entity:* must be unchanged (no new entity minted): before={} after={}",
            count_before, count_after
        );
    }

    /// Cycle/123 (2): Same guard fires when the corruption is reached via the
    /// alias index rather than the name index.
    #[test]
    fn test_get_or_create_entity_refuses_overwrite_on_corrupt_existing_alias_hit() {
        let (store, _tmp) = test_store();
        let graph = GraphStore::new(store.clone());

        let old_uuid = "bbbbbbbb-0000-0000-0000-000000000002";
        let sp = format!("graph:s:{}:", STREAM);

        // Plant corrupt bytes under graph:entity:OLD.
        let entity_key = format!("graph:entity:{}", old_uuid);
        store
            .put(entity_key.as_bytes(), b"\xff\xfe corrupt entity bytes")
            .unwrap();

        // Only alias index points at OLD (name index absent — simulates a
        // scenario where only the alias was written, not the canonical name).
        let alias_key = format!("{}alias:acme-alias", sp);
        store
            .put(alias_key.as_bytes(), old_uuid.as_bytes())
            .unwrap();

        let count_before = store.prefix_scan(b"graph:entity:").count();

        // Call with a matching alias — must return Err.
        let result =
            graph.get_or_create_entity("Acme Corp", "Org", &["Acme-Alias".to_string()], STREAM);
        assert!(
            result.is_err(),
            "must refuse when alias-index target is present-but-unreadable"
        );
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains(old_uuid),
            "error message must contain the offending UUID: got {:?}",
            err_msg
        );

        // Alias-index value must be byte-equal pre-state.
        let alias_val = store
            .get(alias_key.as_bytes())
            .unwrap()
            .expect("alias index must still exist");
        assert_eq!(
            alias_val.as_slice(),
            old_uuid.as_bytes(),
            "alias-index value must be byte-equal pre-state (not overwritten)"
        );

        // No new entity minted.
        let count_after = store.prefix_scan(b"graph:entity:").count();
        assert_eq!(
            count_after, count_before,
            "no new entity must be minted: before={} after={}",
            count_before, count_after
        );
    }

    /// `probe_entity_indexes` dedups probed IDs so a corrupt entity with
    /// both a name-index and an alias-index entry pointing at the same UUID
    /// only appears once in the refuse-error message and only triggers one
    /// `entity_key_present` probe.
    #[test]
    fn test_get_or_create_entity_refuse_error_dedups_uuid_across_name_and_alias() {
        let (store, _tmp) = test_store();
        let graph = GraphStore::new(store.clone());

        let old_uuid = "dddddddd-0000-0000-0000-000000000004";
        let sp = format!("graph:s:{}:", STREAM);

        let entity_key = format!("graph:entity:{}", old_uuid);
        store
            .put(entity_key.as_bytes(), b"\xff\xfe corrupt entity bytes")
            .unwrap();

        let name_key = format!("{}name:acme", sp);
        store.put(name_key.as_bytes(), old_uuid.as_bytes()).unwrap();
        let alias_key = format!("{}alias:acme-alias", sp);
        store
            .put(alias_key.as_bytes(), old_uuid.as_bytes())
            .unwrap();

        let result = graph.get_or_create_entity("Acme", "Org", &["Acme-Alias".to_string()], STREAM);
        let err_msg = result.unwrap_err().to_string();
        let occurrences = err_msg.matches(old_uuid).count();
        assert_eq!(
            occurrences, 1,
            "UUID must appear exactly once in error message (dedup): got {:?}",
            err_msg
        );
    }

    /// Cycle/123 (3): Dangling-index recovery must be preserved.
    ///
    /// When the name index points at a UUID whose `graph:entity:*` key is
    /// entirely absent (no bytes — truly dangling), `get_or_create_entity`
    /// must create a new entity, NOT return Err. This was the pre-/123
    /// behavior for the dangling case and must continue working.
    #[test]
    fn test_get_or_create_entity_creates_new_when_indexes_dangling_to_absent_keys() {
        let (store, _tmp) = test_store();
        let graph = GraphStore::new(store.clone());

        let absent_uuid = "cccccccc-0000-0000-0000-000000000003";
        let sp = format!("graph:s:{}:", STREAM);

        // Write name index pointing at ABSENT_UUID — no entity bytes written.
        let name_key = format!("{}name:widget", sp);
        store
            .put(name_key.as_bytes(), absent_uuid.as_bytes())
            .unwrap();

        // Confirm key is absent.
        let entity_key = format!("graph:entity:{}", absent_uuid);
        assert!(
            store.get(entity_key.as_bytes()).unwrap().is_none(),
            "pre-state: entity key must be absent"
        );

        // get_or_create_entity must succeed (dangling → absent = safe overwrite).
        let result = graph.get_or_create_entity("Widget", "Thing", &[], STREAM);
        assert!(
            result.is_ok(),
            "dangling index must allow create-new: {:?}",
            result.err()
        );
        let node = result.unwrap();

        // New entity must have a different id (not the dangling absent_uuid).
        assert_ne!(
            node.id, absent_uuid,
            "new entity id must not equal the previously-dangling absent uuid"
        );

        // Name index now points at the new id.
        let name_val = store
            .get(name_key.as_bytes())
            .unwrap()
            .expect("name index must exist after create-new");
        assert_eq!(
            name_val.as_slice(),
            node.id.as_bytes(),
            "name index must point at new entity id after dangling-index recovery"
        );
    }

    /// Cycle/123 (4): Regression guard — clean create-new on no index hits.
    ///
    /// Fresh DB, no prior entity. `get_or_create_entity` must succeed, and
    /// the name and canonical-alias indexes must be populated.
    #[test]
    fn test_get_or_create_entity_creates_new_when_no_index_hits() {
        let (store, _tmp) = test_store();
        let graph = GraphStore::new(store.clone());
        let sp = format!("graph:s:{}:", STREAM);

        let result = graph.get_or_create_entity("X Corp", "Org", &["X".to_string()], STREAM);
        assert!(
            result.is_ok(),
            "clean create-new must succeed: {:?}",
            result.err()
        );
        let node = result.unwrap();

        // Name index populated.
        let name_key = format!("{}name:x corp", sp);
        let name_val = store.get(name_key.as_bytes()).unwrap();
        assert_eq!(
            name_val.as_deref(),
            Some(node.id.as_bytes()),
            "name index must point at new entity"
        );

        // Alias index populated.
        let alias_key = format!("{}alias:x", sp);
        let alias_val = store.get(alias_key.as_bytes()).unwrap();
        assert_eq!(
            alias_val.as_deref(),
            Some(node.id.as_bytes()),
            "alias index must point at new entity"
        );
    }

    /// Cycle/123 (5): Regression guard — alias-merge happy path.
    ///
    /// Pre-create entity with `get_or_create_entity("X")`, then call again
    /// with the same name plus a new alias. Must return the existing entity
    /// (no Err) and merge the alias.
    #[test]
    fn test_get_or_create_entity_alias_merge_still_works_after_guard() {
        let (store, _tmp) = test_store();
        let graph = GraphStore::new(store.clone());
        let sp = format!("graph:s:{}:", STREAM);

        // First call — create entity with one alias.
        let e1 = graph
            .get_or_create_entity("Entity X", "Thing", &["ex".to_string()], STREAM)
            .unwrap();

        // Second call — same canonical name, add new alias "ex2".
        let result = graph.get_or_create_entity(
            "Entity X",
            "Thing",
            &["ex".to_string(), "ex2".to_string()],
            STREAM,
        );
        assert!(
            result.is_ok(),
            "alias-merge must not fire the guard: {:?}",
            result.err()
        );
        let e2 = result.unwrap();

        // Must return same entity.
        assert_eq!(e1.id, e2.id, "alias-merge must return existing entity");

        // New alias index must be written.
        let alias_key = format!("{}alias:ex2", sp);
        let alias_val = store.get(alias_key.as_bytes()).unwrap();
        assert_eq!(
            alias_val.as_deref(),
            Some(e1.id.as_bytes()),
            "new alias ex2 must point at existing entity"
        );
    }

    /// Cycle/123 AC-18: `try_resolve_by_alias` must short-circuit on the first
    /// `Resolved` alias hit — subsequent aliases must not be read.
    ///
    /// Observable consequence: returned entity id equals the entity pointed at
    /// by the first matching alias (a1 → E1), not by later aliases (a2 → E2,
    /// a3 → E3).
    #[test]
    fn test_try_resolve_by_alias_short_circuits_on_first_resolved() {
        let (store, _tmp) = test_store();
        let graph = GraphStore::new(store.clone());
        let sp = format!("graph:s:{}:", STREAM);

        // Pre-create three distinct entities, each with a unique alias.
        let e1 = graph
            .get_or_create_entity("Entity-E1", "Thing", &["a1".to_string()], STREAM)
            .unwrap();
        let e2 = graph
            .get_or_create_entity("Entity-E2", "Thing", &["a2".to_string()], STREAM)
            .unwrap();
        let e3 = graph
            .get_or_create_entity("Entity-E3", "Thing", &["a3".to_string()], STREAM)
            .unwrap();

        // Verify all three alias keys exist independently.
        assert!(store
            .get(format!("{}alias:a1", sp).as_bytes())
            .unwrap()
            .is_some());
        assert!(store
            .get(format!("{}alias:a2", sp).as_bytes())
            .unwrap()
            .is_some());
        assert!(store
            .get(format!("{}alias:a3", sp).as_bytes())
            .unwrap()
            .is_some());

        // Call get_or_create_entity with a canonical name that has NO name-index entry
        // (distinct from E1/E2/E3 canonical names) but whose alias list matches a1, a2, a3.
        // The name index for "Umbrella" does not exist; probing falls through to alias probe.
        let result = graph.get_or_create_entity(
            "Umbrella",
            "Thing",
            &["a1".to_string(), "a2".to_string(), "a3".to_string()],
            STREAM,
        );

        assert!(
            result.is_ok(),
            "alias probe must resolve: {:?}",
            result.err()
        );
        let resolved = result.unwrap();

        // The first readable alias (a1 → E1) must win — short-circuit means
        // a2 and a3 are never probed.
        assert_eq!(
            resolved.id, e1.id,
            "short-circuit: first alias hit (a1 → E1) must be returned, not E2 or E3. \
             got id={} expected id={}",
            resolved.id, e1.id
        );

        // Confirm E2 and E3 were not returned.
        assert_ne!(resolved.id, e2.id, "must not return E2");
        assert_ne!(resolved.id, e3.id, "must not return E3");
    }

    // ── §E tests (AC-E4) ────────────────────────────────────────────────────────

    /// Build an `Arc<RocksDbStore>` backed by a `MasterKeyEnvProvider`.
    fn test_store_with_master_key() -> (Arc<RocksDbStore>, tempfile::TempDir) {
        use crate::crypto::provider::MasterKeyEnvProvider;
        let tmp = tempfile::TempDir::new().unwrap();
        let config = crate::config::RocksDbConfig {
            max_open_files: 100,
            compression: "none".to_string(),
            write_buffer_size: 4 * 1024 * 1024,
            max_write_buffer_number: 2,
        };
        let store = RocksDbStore::open(tmp.path(), &config).unwrap();
        let provider = Arc::new(MasterKeyEnvProvider::new([42u8; 32], store.db_arc()));
        // `with_encryption_provider` consumes the owned store; wrap in Arc after.
        let store = Arc::new(store.with_encryption_provider(provider));
        (store, tmp)
    }

    /// AC-E4: round-trip — create entity under MasterKey, look it up by name.
    #[test]
    fn entity_name_index_token_roundtrip_by_name() {
        let (store, _tmp) = test_store_with_master_key();
        let graph = GraphStore::new(store.clone());

        let entity = graph
            .get_or_create_entity("Alice Smith", "Person", &[], "stream-e4")
            .unwrap();

        let found = graph
            .get_entity_by_name("Alice Smith", "stream-e4")
            .unwrap()
            .expect("entity must be findable by name after token-indexed write");
        assert_eq!(found.id, entity.id);
    }

    /// AC-E4: case-insensitive round-trip — "ALICE SMITH" finds the entity.
    #[test]
    fn entity_name_index_token_case_insensitive_lookup() {
        let (store, _tmp) = test_store_with_master_key();
        let graph = GraphStore::new(store.clone());

        let entity = graph
            .get_or_create_entity("Alice Smith", "Person", &[], "stream-e4b")
            .unwrap();

        let found = graph
            .get_entity_by_name("ALICE SMITH", "stream-e4b")
            .unwrap()
            .expect("uppercase lookup must resolve via token (same token as lowercase)");
        assert_eq!(found.id, entity.id);
    }

    /// AC-E4: alias round-trip — entity findable by alias under MasterKey.
    #[test]
    fn entity_alias_index_token_roundtrip() {
        let (store, _tmp) = test_store_with_master_key();
        let graph = GraphStore::new(store.clone());

        let entity = graph
            .get_or_create_entity("Alice Smith", "Person", &["Ali".to_string()], "stream-e4c")
            .unwrap();

        let found = graph
            .get_entity_by_name("Ali", "stream-e4c")
            .unwrap()
            .expect("entity must be findable by alias after token-indexed write");
        assert_eq!(found.id, entity.id);
    }

    /// AC-E4: raw key scan shows no plaintext name in MasterKey index rows.
    ///
    /// Under MasterKey, `graph:s:{stream_id}:name:*` key suffixes must be
    /// 64-char lowercase hex strings, not the plaintext canonical name.
    #[test]
    fn entity_name_index_key_is_hex_token_not_plaintext_under_master_key() {
        let (store, _tmp) = test_store_with_master_key();
        let graph = GraphStore::new(store.clone());
        let stream = "stream-e4-scan";

        graph
            .get_or_create_entity("Alice Smith", "Person", &["Ali".to_string()], stream)
            .unwrap();

        // Scan all name: and alias: index keys in this stream.
        let sp = format!("graph:s:{}:", stream);
        let name_prefix = format!("{}name:", sp);
        let alias_prefix = format!("{}alias:", sp);

        for (k, _) in store.prefix_scan(name_prefix.as_bytes()) {
            let key_str = String::from_utf8_lossy(&k).to_string();
            let suffix = key_str
                .strip_prefix(&name_prefix)
                .expect("key must start with name prefix");
            assert_eq!(
                suffix.len(),
                64,
                "name index suffix must be 64-char hex: {suffix}"
            );
            assert!(
                suffix.chars().all(|c| c.is_ascii_hexdigit()),
                "name index suffix must be hex (not plaintext name): {suffix}"
            );
            assert!(
                !suffix.contains("alice"),
                "plaintext name must not appear in index key: {suffix}"
            );
        }
        for (k, _) in store.prefix_scan(alias_prefix.as_bytes()) {
            let key_str = String::from_utf8_lossy(&k).to_string();
            let suffix = key_str
                .strip_prefix(&alias_prefix)
                .expect("key must start with alias prefix");
            assert_eq!(
                suffix.len(),
                64,
                "alias index suffix must be 64-char hex: {suffix}"
            );
            assert!(
                suffix.chars().all(|c| c.is_ascii_hexdigit()),
                "alias index suffix must be hex: {suffix}"
            );
        }
    }

    /// AC-E4: delete_entity removes token-keyed index rows (no orphan rows).
    #[test]
    fn entity_delete_removes_token_index_rows() {
        let (store, _tmp) = test_store_with_master_key();
        let graph = GraphStore::new(store.clone());
        let stream = "stream-e4-del";

        let entity = graph
            .get_or_create_entity("Bob Jones", "Person", &["BJ".to_string()], stream)
            .unwrap();

        // Verify index rows exist before delete.
        let sp = format!("graph:s:{}:", stream);
        let name_prefix = format!("{}name:", sp);
        let alias_prefix = format!("{}alias:", sp);
        let name_count_before = store.prefix_scan(name_prefix.as_bytes()).count();
        let alias_count_before = store.prefix_scan(alias_prefix.as_bytes()).count();
        assert!(
            name_count_before >= 1,
            "name index must exist before delete"
        );
        assert!(
            alias_count_before >= 1,
            "alias index must exist before delete"
        );

        graph.delete_entity(&entity.id).unwrap();

        // All index rows pointing to this entity must be gone.
        let name_orphans = store
            .prefix_scan(name_prefix.as_bytes())
            .filter(|(_, v)| v.as_ref() == entity.id.as_bytes())
            .count();
        let alias_orphans = store
            .prefix_scan(alias_prefix.as_bytes())
            .filter(|(_, v)| v.as_ref() == entity.id.as_bytes())
            .count();
        assert_eq!(
            name_orphans, 0,
            "delete_entity must remove all name index rows"
        );
        assert_eq!(
            alias_orphans, 0,
            "delete_entity must remove all alias index rows"
        );
    }

    /// AC-E4: cross-provider/legacy re-key migration.
    ///
    /// 1. Write entity index rows using NoopProvider (plaintext keys).
    /// 2. Switch to MasterKeyEnvProvider (same DB).
    /// 3. Run rekey_name_index().
    /// 4. Verify entity is findable by name via token keys.
    /// 5. Verify old plaintext key is gone from the index.
    #[test]
    fn rekey_migration_upgrades_plaintext_index_to_tokens() {
        use crate::config::RocksDbConfig;
        use crate::crypto::provider::MasterKeyEnvProvider;

        let tmp = tempfile::TempDir::new().unwrap();
        let config = RocksDbConfig {
            max_open_files: 100,
            compression: "none".to_string(),
            write_buffer_size: 4 * 1024 * 1024,
            max_write_buffer_number: 2,
        };
        let stream = "stream-rekey";

        // Step 1: write under NoopProvider (plaintext index keys).
        // Open once, write with Noop, then switch provider in-place via
        // `with_encryption_provider` (same open DB handle — no re-open needed).
        let store_raw = RocksDbStore::open(tmp.path(), &config).unwrap();
        let db_arc = store_raw.db_arc();
        let graph_noop = GraphStore::new(Arc::new(store_raw));
        let entity = graph_noop
            .get_or_create_entity("Carol White", "Person", &["CW".to_string()], stream)
            .unwrap();

        // Confirm plaintext key exists via the shared DB arc.
        let sp = format!("graph:s:{}:", stream);
        let plaintext_name_key = format!("{}name:carol white", sp);
        assert!(
            db_arc.get(plaintext_name_key.as_bytes()).unwrap().is_some(),
            "plaintext name key must exist after NoopProvider write"
        );

        // Step 2: switch to MasterKeyEnvProvider on the same open DB handle.
        // Consume graph_noop to get the underlying Arc; unwrap to get the owned
        // RocksDbStore, then attach the provider.
        let store_owned = Arc::try_unwrap(graph_noop.store)
            .unwrap_or_else(|_| panic!("no other Arc references to store at this point"));
        let provider = Arc::new(MasterKeyEnvProvider::new([42u8; 32], store_owned.db_arc()));
        let store_master = Arc::new(store_owned.with_encryption_provider(provider));
        let graph_master = GraphStore::new(store_master.clone());

        // Before migration: entity NOT findable (token != plaintext key).
        let found_before = graph_master
            .get_entity_by_name("Carol White", stream)
            .unwrap();
        assert!(
            found_before.is_none(),
            "entity must NOT be findable by name before re-key migration"
        );

        // Step 3: run migration.
        let (migrated, _already_current) = graph_master.rekey_name_index().unwrap();
        assert!(
            migrated > 0,
            "migration must report at least one row migrated"
        );

        // Step 4: entity IS findable after migration.
        let found_after = graph_master
            .get_entity_by_name("Carol White", stream)
            .unwrap()
            .expect("entity must be findable by name after re-key migration");
        assert_eq!(found_after.id, entity.id);

        // Step 5: old plaintext key is gone.
        assert!(
            store_master
                .get(plaintext_name_key.as_bytes())
                .unwrap()
                .is_none(),
            "old plaintext name key must be deleted after re-key migration"
        );
    }

    /// AC-E4: rekey_name_index under NoopProvider is a no-op
    /// (rows_migrated=0, rows_already_current=N).
    #[test]
    fn rekey_migration_noop_under_noop_provider() {
        let (store, _tmp) = test_store(); // NoopProvider
        let graph = GraphStore::new(store.clone());
        let stream = "stream-noop-rekey";

        graph
            .get_or_create_entity("Dave Brown", "Person", &["DB".to_string()], stream)
            .unwrap();

        let (migrated, already_current) = graph.rekey_name_index().unwrap();
        assert_eq!(migrated, 0, "NoopProvider: no rows should be migrated");
        assert!(
            already_current > 0,
            "NoopProvider: all rows should be already current (token==plaintext)"
        );
    }

    /// Regression for cycle/131 Person alias-merge path: caller-supplied
    /// `aliases` must be persisted, not silently dropped when the substring
    /// gate fires. Mirrors the resolved-via-name and create paths.
    #[test]
    fn test_get_or_create_entity_person_alias_merge_preserves_caller_aliases() {
        let (store, _tmp) = test_store();
        let graph = GraphStore::new(store.clone());
        let sp = format!("graph:s:{}:", STREAM);

        // Seed: Person "Anna" with one alias "LG".
        let e1 = graph
            .get_or_create_entity("Anna", "Person", &["LG".to_string()], STREAM)
            .unwrap();

        // Substring gate fires ("Anna" ⊂ "Anna Nowak"); caller passes
        // a new alias "Luki" that previously was silently dropped.
        let e2 = graph
            .get_or_create_entity("Anna Nowak", "Person", &["Luki".to_string()], STREAM)
            .unwrap();

        // Same entity returned via alias-merge.
        assert_eq!(e1.id, e2.id, "alias-merge must return existing Person");

        // Caller-supplied alias persisted on entity.
        let has_luki = e2.aliases.iter().any(|a| a.eq_ignore_ascii_case("Luki"));
        assert!(
            has_luki,
            "caller-supplied alias 'Luki' must be persisted, got: {:?}",
            e2.aliases
        );

        // Caller-supplied alias written to alias index.
        let luki_idx = store.get(format!("{}alias:luki", sp).as_bytes()).unwrap();
        assert_eq!(
            luki_idx.as_deref(),
            Some(e1.id.as_bytes()),
            "caller alias 'Luki' must be written to alias index"
        );

        // Pre-existing alias remains.
        let lg_idx = store.get(format!("{}alias:lg", sp).as_bytes()).unwrap();
        assert_eq!(
            lg_idx.as_deref(),
            Some(e1.id.as_bytes()),
            "pre-existing alias 'LG' must remain in alias index"
        );

        // Canonical name "Anna Nowak" still indexed as alias of e1.
        let lg_full_idx = store
            .get(format!("{}alias:anna nowak", sp).as_bytes())
            .unwrap();
        assert_eq!(
            lg_full_idx.as_deref(),
            Some(e1.id.as_bytes()),
            "merged canonical_name must be written to alias index"
        );
    }
}
