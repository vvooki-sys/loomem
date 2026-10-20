//! Admission and atomic source tombstones share the chunk lifecycle mutex.
use super::lifecycle::generation_key;
use super::{Chunk, ProvenanceRole, RocksDbStore};
use anyhow::{ensure, Context, Result};
use rocksdb::WriteBatch;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

fn marker_kind(chunk: &Chunk) -> Option<&str> {
    let marker = chunk.metadata.as_ref()?.get("_loomem_ingest")?;
    (marker.get("version")?.as_u64()? == 1)
        .then(|| marker.get("kind")?.as_str())
        .flatten()
}

/// Recognize only the server-owned source contract; legacy chunks stay legacy.
#[must_use]
pub fn is_ingest_source(chunk: &Chunk) -> bool {
    marker_kind(chunk) == Some("source")
        && chunk.provenance_role == ProvenanceRole::Evidence
        && chunk.trust_level.as_deref() == Some("a2")
        && chunk.source.as_ref().map(|tag| tag.agent.as_str()) == Some("mcp-ingest-source")
        && source_flags(chunk)
}

fn source_flags(chunk: &Chunk) -> bool {
    chunk.level == 0 && chunk.persistent && chunk.consolidated && !chunk.dormant
}

fn is_ingest_fact(chunk: &Chunk) -> bool {
    marker_kind(chunk) == Some("fact")
        && chunk.source.as_ref().map(|tag| tag.agent.as_str()) == Some("mcp-ingest")
}

/// Strict wire shape for source-linked, server-owned facts. Admission also
/// validates the referenced source records under the lifecycle mutex.
#[must_use]
pub fn is_ingest_fact_shape(chunk: &Chunk) -> bool {
    is_ingest_fact(chunk)
        && chunk.provenance_role == ProvenanceRole::Claim
        && chunk.trust_level.as_deref() == Some("a2")
        && fact_links(chunk).is_some()
}

fn fact_links(chunk: &Chunk) -> Option<&[String]> {
    let ids = chunk.source_ids.as_deref()?;
    let extracted = chunk.extraction_meta.as_ref()?.extracted_from.as_ref()?;
    (!ids.is_empty() && ids.contains(extracted) && ids.iter().all(|id| canonical_uuid(id)))
        .then_some(ids)
}

fn canonical_uuid(id: &str) -> bool {
    uuid::Uuid::parse_str(id).is_ok_and(|uuid| uuid.to_string() == id)
}

// Legacy dream rows also carry source_ids.join(",") as an audit string.
// That composite is not a second foreign key. Keep singular extracted_from
// independently, so other explicit IDs cannot hide an owned-source link.
fn parents(chunk: &Chunk) -> impl Iterator<Item = &str> {
    let extracted = chunk
        .extraction_meta
        .as_ref()
        .and_then(|meta| meta.extracted_from.as_deref())
        .filter(|from| chunk.source_ids.is_none() || !from.contains(','));
    chunk
        .source_ids
        .iter()
        .flatten()
        .map(String::as_str)
        .chain(extracted)
}

struct Ancestry {
    owned: bool,
    invalid: bool,
}

fn immutable_provenance(chunk: &Chunk) -> Result<serde_json::Value> {
    Ok(serde_json::to_value((
        &chunk.content,
        &chunk.stream,
        &chunk.metadata,
        &chunk.source,
        chunk.provenance_role,
        &chunk.trust_level,
        &chunk.source_ids,
        chunk.valid_from,
        &chunk.extraction_meta,
        chunk.level,
        chunk.persistent,
        chunk.timestamp,
        is_ingest_source(chunk).then_some((chunk.consolidated, chunk.dormant)),
    ))?)
}

impl RocksDbStore {
    /// Called only inside store_chunk_locked, before its batch is published.
    pub(super) fn validate_ingest_admission(&self, chunk: &Chunk) -> Result<()> {
        self.validate_owned_update(chunk)?;
        if chunk.deleted_at.is_some() {
            return Ok(());
        }
        self.validate_live_ancestry(chunk)?;
        if is_ingest_fact(chunk) {
            self.validate_fact_sources(chunk)?;
        }
        Ok(())
    }

    fn validate_live_ancestry(&self, chunk: &Chunk) -> Result<()> {
        let ancestry = self.ingest_ancestry(chunk)?;
        let legacy_unchanged = self.get_chunk(&chunk.id)?.is_some_and(|old| {
            old.deleted_at.is_none() && !ancestry.owned && parents(&old).eq(parents(chunk))
        });
        ensure!(
            !ancestry.invalid || legacy_unchanged,
            "lineage ancestor is missing, deleted, or cross-stream"
        );
        Ok(())
    }

    fn validate_fact_sources(&self, chunk: &Chunk) -> Result<()> {
        ensure!(is_ingest_fact_shape(chunk), "invalid ingest fact shape");
        let ids = fact_links(chunk).context("invalid ingest fact lineage")?;
        for id in ids {
            let source = self
                .get_chunk(id)?
                .context("ingest source does not exist")?;
            ensure!(
                is_ingest_source(&source),
                "ingest parent is not a recognized source"
            );
            ensure!(
                source.stream == chunk.stream,
                "ingest source stream mismatch"
            );
            ensure!(source.deleted_at.is_none(), "ingest source is deleted");
        }
        Ok(())
    }

    fn validate_owned_update(&self, chunk: &Chunk) -> Result<()> {
        let Some(old) = self.get_chunk(&chunk.id)? else {
            return Ok(());
        };
        if !self.ingest_ancestry(&old)?.owned {
            return Ok(());
        }
        ensure!(
            immutable_provenance(&old)? == immutable_provenance(chunk)?,
            "ingest content and provenance are immutable; delete and re-ingest corrections"
        );
        ensure!(
            old.deleted_at.is_none() || chunk.deleted_at.is_some(),
            "cannot revive deleted ingest lineage"
        );
        Ok(())
    }

    fn ingest_ancestry(&self, chunk: &Chunk) -> Result<Ancestry> {
        let mut state = Ancestry {
            owned: is_ingest_source(chunk) || is_ingest_fact(chunk),
            invalid: false,
        };
        let mut pending: Vec<String> = parents(chunk).map(str::to_owned).collect();
        let mut visited = BTreeSet::new();
        if !state.owned {
            visited.insert(chunk.id.clone());
        }
        while let Some(id) = pending.pop() {
            if !visited.insert(id.clone()) {
                continue;
            }
            let Some(parent) = self.get_chunk(&id)? else {
                state.invalid = true;
                continue;
            };
            state.owned |= is_ingest_source(&parent) || is_ingest_fact(&parent);
            state.invalid |= parent.deleted_at.is_some() || parent.stream != chunk.stream;
            pending.extend(parents(&parent).map(str::to_owned));
        }
        Ok(state)
    }

    /// Tombstone a recognized source and its same-stream descendant closure in
    /// one RocksDB batch. Includes already tombstoned descendants for cleanup
    /// retries. None preserves the legacy single-delete path. No await/clock.
    pub fn tombstone_ingest_source(&self, id: &str, at: u64) -> Result<Option<Vec<String>>> {
        let _guard = self.lock_lifecycle()?;
        let Some(source) = self.get_chunk(id)? else {
            return Ok(None);
        };
        if !is_ingest_source(&source) {
            return Ok(None);
        }
        let rows = self.lineage_rows()?;
        let ids = descendant_ids(&rows, id, &source.stream);
        self.write_lineage_tombstones(rows, &source.stream, &ids, at)?;
        Ok(Some(ids.into_iter().collect()))
    }

    fn write_lineage_tombstones(
        &self,
        rows: Vec<(Vec<u8>, Chunk)>,
        stream: &str,
        ids: &BTreeSet<String>,
        at: u64,
    ) -> Result<()> {
        let mut batch = WriteBatch::default();
        for (key, mut chunk) in rows {
            if chunk.stream != stream || !ids.contains(&chunk.id) {
                continue;
            }
            if chunk.deleted_at.is_none() {
                chunk.deleted_at = Some(at);
                batch.put(key, self.encode_chunk(&chunk)?);
                batch.put(generation_key(&chunk.id), uuid::Uuid::new_v4().as_bytes());
            }
        }
        self.db
            .write(batch)
            .context("atomically tombstone ingest lineage")
    }

    fn lineage_rows(&self) -> Result<Vec<(Vec<u8>, Chunk)>> {
        let mut rows = Vec::new();
        for row in self.db.prefix_iterator(b"chunk:") {
            let (key, value) = row.context("scan ingest lineage")?;
            if !key.starts_with(b"chunk:") {
                break;
            }
            let chunk = self.decode_chunk(&value).context("decode ingest lineage")?;
            rows.push((key.to_vec(), chunk));
        }
        Ok(rows)
    }
}

fn descendant_ids(rows: &[(Vec<u8>, Chunk)], root: &str, stream: &str) -> BTreeSet<String> {
    let mut reverse: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (_, chunk) in rows.iter().filter(|(_, chunk)| chunk.stream == stream) {
        for parent in parents(chunk) {
            reverse.entry(parent).or_default().push(&chunk.id);
        }
    }
    let mut visited = BTreeSet::new();
    let mut queue = VecDeque::from([root]);
    while let Some(id) = queue.pop_front() {
        if visited.insert(id.to_string()) {
            queue.extend(reverse.get(id).into_iter().flatten().copied());
        }
    }
    visited
}

#[cfg(test)]
mod tests;
