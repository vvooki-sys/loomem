//! Read-only snapshot evidence for fresh, immutable evaluation indexes.
//! This does not establish readiness of vectors, primary storage or concurrent writes.
use super::TantivyIndex;
use anyhow::{ensure, Context, Result};
use std::collections::HashSet;
use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, Occur, TermQuery};
use tantivy::schema::{IndexRecordOption, Value};
use tantivy::{Searcher, TantivyDocument, Term};

/// Evaluation protocol bound, not a runtime configuration default.
pub const MAX_READINESS_DOCUMENTS: usize = 1024;

pub struct ExpectedDocument<'a> {
    pub id: &'a str,
    pub stream: &'a str,
    pub content: &'a str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocumentReadiness {
    Missing,
    Duplicate,
    ContentMismatch,
    Current,
}

impl TantivyIndex {
    /// Returns one status per expectation, in input order, from one reader snapshot.
    /// Never commits or reloads the index; duplicate expectations are rejected.
    pub fn probe_expected_documents(
        &self,
        expected: &[ExpectedDocument<'_>],
    ) -> Result<Vec<DocumentReadiness>> {
        ensure!(
            expected.len() <= MAX_READINESS_DOCUMENTS,
            "too many expected documents"
        );
        let mut keys = HashSet::new();
        for doc in expected {
            ensure!(
                keys.insert((doc.id, doc.stream)),
                "duplicate expected document"
            );
        }
        let searcher = self.reader.searcher();
        expected
            .iter()
            .map(|doc| self.probe_document(&searcher, doc))
            .collect()
    }

    fn probe_document(
        &self,
        searcher: &Searcher,
        expected: &ExpectedDocument<'_>,
    ) -> Result<DocumentReadiness> {
        let query = BooleanQuery::new(vec![
            (
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_text(self.id_field, expected.id),
                    IndexRecordOption::Basic,
                )),
            ),
            (
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_text(self.stream_field, expected.stream),
                    IndexRecordOption::Basic,
                )),
            ),
        ]);
        let hits = searcher
            .search(&query, &TopDocs::with_limit(2))
            .context("readiness query failed")?;
        if hits.is_empty() {
            return Ok(DocumentReadiness::Missing);
        }
        if hits.len() > 1 {
            return Ok(DocumentReadiness::Duplicate);
        }
        let doc: TantivyDocument = searcher
            .doc(hits[0].1)
            .context("readiness document read failed")?;
        let mut content = doc.get_all(self.content_field);
        let matches = content.next().and_then(|v| v.as_str()) == Some(expected.content)
            && content.next().is_none();
        Ok(if matches {
            DocumentReadiness::Current
        } else {
            DocumentReadiness::ContentMismatch
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tantivy_index::TantivyConfig;
    use tantivy::doc;

    fn fixture() -> (tempfile::TempDir, TantivyIndex) {
        let dir = tempfile::tempdir().unwrap();
        let index = TantivyIndex::open(
            dir.path(),
            &TantivyConfig {
                enabled: true,
                heap_size_mb: 16,
                drift_warn_pct: 5.0,
                auto_rebuild_on_drift: false,
            },
        )
        .unwrap();
        (dir, index)
    }
    fn add(index: &mut TantivyIndex, id: &str, stream: &str, content: &str) {
        index
            .writer
            .add_document(tantivy::doc!(
                index.id_field => id, index.stream_field => stream, index.content_field => content,
            ))
            .unwrap();
    }
    fn probe(index: &TantivyIndex, id: &str, stream: &str, content: &str) -> DocumentReadiness {
        index
            .probe_expected_documents(&[ExpectedDocument {
                id,
                stream,
                content,
            }])
            .unwrap()[0]
    }
    #[test]
    fn equal_counts_do_not_hide_missing_id_or_wrong_content() {
        let (_dir, mut index) = fixture();
        add(&mut index, "orphan-b", "s", "correct");
        index.commit().unwrap();
        assert_eq!(index.count().unwrap(), 1);
        assert_eq!(
            probe(&index, "a", "s", "correct"),
            DocumentReadiness::Missing
        );
        assert_eq!(
            probe(&index, "orphan-b", "s", "wrong"),
            DocumentReadiness::ContentMismatch
        );
    }
    #[test]
    fn duplicates_and_stream_scope_are_exact() {
        let (_dir, mut index) = fixture();
        add(&mut index, "a", "s", "correct");
        add(&mut index, "a", "other", "wrong");
        index.commit().unwrap();
        assert_eq!(
            probe(&index, "a", "s", "correct"),
            DocumentReadiness::Current
        );
        assert_eq!(
            probe(&index, "a", "absent", "correct"),
            DocumentReadiness::Missing
        );
        add(&mut index, "a", "s", "correct");
        index.commit().unwrap();
        assert_eq!(
            probe(&index, "a", "s", "correct"),
            DocumentReadiness::Duplicate
        );
    }
    #[test]
    fn uncommitted_document_stays_invisible() {
        let (_dir, mut index) = fixture();
        add(&mut index, "a", "s", "correct");
        assert_eq!(
            probe(&index, "a", "s", "correct"),
            DocumentReadiness::Missing
        );
        index.commit().unwrap();
        assert_eq!(
            probe(&index, "a", "s", "correct"),
            DocumentReadiness::Current
        );
    }
    #[test]
    fn validates_duplicate_expectations_and_bound() {
        let (_dir, index) = fixture();
        let expected = (0..=MAX_READINESS_DOCUMENTS)
            .map(|_| ExpectedDocument {
                id: "a",
                stream: "s",
                content: "c",
            })
            .collect::<Vec<_>>();
        assert!(index.probe_expected_documents(&expected[..2]).is_err());
        assert!(index.probe_expected_documents(&expected).is_err());
        assert!(index.probe_expected_documents(&[]).unwrap().is_empty());
    }
}
