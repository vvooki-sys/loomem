//! Serialize chunk lifecycle and conditional embedding publication. Inference
//! never holds this lock. Generations survive delete/recreate (including ABA).
use super::{Chunk, RocksDbStore, CF_EMBEDDINGS};
use anyhow::{Context, Result};
use rocksdb::WriteBatch;
use std::sync::MutexGuard;

#[derive(Debug)]
pub struct EmbeddingTarget {
    id: String,
    content: String,
    generation: Option<Vec<u8>>,
}

fn generation_key(id: &str) -> String {
    format!("embedding_generation:{id}")
}

impl RocksDbStore {
    pub(crate) fn lock_lifecycle(&self) -> Result<MutexGuard<'_, ()>> {
        self.lifecycle
            .lock()
            .map_err(|_| anyhow::anyhow!("chunk lifecycle lock poisoned"))
    }

    /// Capture before inference; None means the request is already obsolete.
    pub fn embedding_target(
        &self,
        id: &str,
        expected_content: &str,
    ) -> Result<Option<EmbeddingTarget>> {
        let _guard = self.lock_lifecycle()?;
        let Some(chunk) = self.get_chunk(id)? else {
            return Ok(None);
        };
        if chunk.deleted_at.is_some() || chunk.content != expected_content {
            return Ok(None);
        }
        Ok(Some(EmbeddingTarget {
            id: id.to_string(),
            content: expected_content.to_string(),
            generation: self.db.get(generation_key(id))?,
        }))
    }

    /// Validate and publish under the same lock as store/delete/purge. False is
    /// an obsolete result, not a provider/storage error. No lock spans inference.
    pub fn store_embedding_if_current(
        &self,
        target: &EmbeddingTarget,
        vector: Vec<f32>,
    ) -> Result<bool> {
        let _guard = self.lock_lifecycle()?;
        let Some(chunk) = self.get_chunk(&target.id)? else {
            return Ok(false);
        };
        if chunk.deleted_at.is_some()
            || chunk.content != target.content
            || self.db.get(generation_key(&target.id))? != target.generation
        {
            return Ok(false);
        }
        self.write_embedding(&target.id, vector)?;
        Ok(true)
    }

    pub fn store_chunk(&self, chunk: &Chunk) -> Result<()> {
        let _guard = self.lock_lifecycle()?;
        self.store_chunk_locked(chunk)
    }

    pub(super) fn store_chunk_locked(&self, chunk: &Chunk) -> Result<()> {
        let previous = self.get_chunk(&chunk.id)?;
        let changed = previous.as_ref().is_none_or(|old| {
            old.content != chunk.content
                || old.stream != chunk.stream
                || old.version != chunk.version
                || old.deleted_at != chunk.deleted_at
        });
        let mut batch = WriteBatch::default();
        batch.put(
            format!("chunk:L{}:{}", chunk.level, chunk.id),
            self.encode_chunk(chunk)?,
        );
        if changed {
            batch.put(generation_key(&chunk.id), uuid::Uuid::new_v4().as_bytes());
            // Soft deletion retains an existing vector for the recovery window,
            // but the new generation prevents any outstanding work publishing.
            if chunk.deleted_at.is_none() {
                let cf = self
                    .db
                    .cf_handle(CF_EMBEDDINGS)
                    .context("Embeddings column family not found")?;
                batch.delete_cf(&cf, chunk.id.as_bytes());
            }
        }
        self.db
            .write(batch)
            .context("store chunk and embedding generation")
    }

    pub fn delete_by_id(&self, id: &str) -> Result<bool> {
        let _guard = self.lock_lifecycle()?;
        let Some(mut chunk) = self.get_chunk(id)? else {
            return Ok(false);
        };
        chunk.deleted_at = Some(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .context("clock before epoch")?
                .as_secs(),
        );
        self.store_chunk_locked(&chunk)?;
        Ok(true)
    }

    fn append_chunk_delete(&self, batch: &mut WriteBatch, id: &str) -> Result<()> {
        for level in 0..=2 {
            batch.delete(format!("chunk:L{level}:{id}"));
        }
        let cf = self
            .db
            .cf_handle(CF_EMBEDDINGS)
            .context("Embeddings column family not found")?;
        batch.delete_cf(&cf, id.as_bytes());
        batch.delete(format!("entity:{id}"));
        batch.delete(format!("rel:{id}"));
        batch.delete(generation_key(id));
        Ok(())
    }

    pub fn hard_delete_by_id(&self, id: &str) -> Result<bool> {
        let _guard = self.lock_lifecycle()?;
        let found = (0..=2)
            .map(|level| self.db.get(format!("chunk:L{level}:{id}")))
            .collect::<Result<Vec<_>, _>>()?
            .iter()
            .any(Option::is_some);
        let mut batch = WriteBatch::default();
        self.append_chunk_delete(&mut batch, id)?;
        self.db
            .write(batch)
            .context("hard delete chunk and derivatives")?;
        Ok(found)
    }

    /// Atomically delete a stream snapshot and its RocksDB derivatives. Decode
    /// errors abort instead of reporting a successful partial purge.
    pub fn purge_namespace(&self, stream: &str, dry_run: bool) -> Result<Vec<String>> {
        let _guard = self.lock_lifecycle()?;
        let mut ids = Vec::new();
        let mut batch = WriteBatch::default();
        for row in self.db.prefix_iterator(b"chunk:") {
            let (key, value) = row.context("scan chunks for purge")?;
            if !key.starts_with(b"chunk:") {
                break;
            }
            let chunk = self
                .decode_chunk(&value)
                .context("decode chunk during purge")?;
            if chunk.stream == stream {
                self.append_chunk_delete(&mut batch, &chunk.id)?;
                ids.push(chunk.id);
            }
        }
        if !dry_run {
            self.db
                .write(batch)
                .context("purge stream and derivatives")?;
        }
        Ok(ids)
    }
}

#[cfg(test)]
mod tests;
