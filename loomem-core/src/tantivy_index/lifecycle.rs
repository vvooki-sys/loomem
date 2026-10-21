use super::TantivyIndex;
use anyhow::Result;

impl TantivyIndex {
    pub fn delete_document(&mut self, id: &str) -> Result<()> {
        self.writer
            .delete_term(tantivy::Term::from_field_text(self.id_field, id));
        self.commit()
    }

    /// Idempotent stream deletion independent of IDs remaining in primary storage.
    pub fn delete_stream(&mut self, stream: &str) -> Result<()> {
        self.writer
            .delete_term(tantivy::Term::from_field_text(self.stream_field, stream));
        self.commit()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tantivy_index::{TantivyConfig, TextDocument};

    #[test]
    fn stream_delete_is_scoped_immediately_visible_and_retry_safe() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = TantivyIndex::open(
            dir.path(),
            &TantivyConfig {
                enabled: true,
                heap_size_mb: 16,
                drift_warn_pct: 5.0,
                auto_rebuild_on_drift: false,
            },
        )
        .unwrap();
        for stream in ["target", "other"] {
            index
                .index_document(TextDocument {
                    id: stream.into(),
                    content: "synthetic fact".into(),
                    user_id: "default".into(),
                    app_id: "default".into(),
                    level: 0,
                    timestamp: 1,
                    stream: stream.into(),
                    entities: None,
                    relations: None,
                    event_date: None,
                    source_agent: None,
                })
                .unwrap();
        }
        index.commit().unwrap();
        index.delete_stream("target").unwrap();
        assert_eq!(index.count_stream("target").unwrap(), 0);
        assert_eq!(index.count_stream("other").unwrap(), 1);
        index.delete_stream("target").unwrap();
        assert_eq!(index.count().unwrap(), 1);
        index.delete_document("other").unwrap();
        assert_eq!(index.count().unwrap(), 0);
    }
}
