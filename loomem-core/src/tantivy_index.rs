use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;
use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, Occur, QueryParser, RangeQuery};
use tantivy::schema::*;
use tantivy::{doc, Index, IndexReader, IndexWriter, ReloadPolicy};
use tracing::{debug, info, warn};

use crate::storage::RocksDbStore;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TantivyConfig {
    pub enabled: bool,
    pub heap_size_mb: usize,
    /// Drift warning threshold (% difference between tantivy doc count and
    /// RocksDB chunk count). Cycle /39: > threshold at startup → WARN log
    /// recommending rebuild. Default: 5.0%.
    #[serde(default = "default_drift_warn_pct")]
    pub drift_warn_pct: f64,
    /// If true, automatically run `rebuild_from_rocksdb` at startup when
    /// drift exceeds threshold. Default: false (warn only — operator
    /// triggers rebuild manually via POST /v1/rebuild-tantivy).
    #[serde(default)]
    pub auto_rebuild_on_drift: bool,
}

fn default_drift_warn_pct() -> f64 {
    5.0
}

/// Sanitize query text for Tantivy's `QueryParser`.
///
/// Fallback step of [`parse_lexical_query`]: when the raw input fails to
/// parse (an unescaped operator metacharacter makes `parse_query` return
/// `QueryParserError::SyntaxError`), mapping every operator metacharacter to
/// a space lets arbitrary natural-language input parse as plain terms. Lossy
/// by design (terms are split, not interpreted); the vector leg keeps full
/// recall on the raw text. Covers the full Tantivy set
/// `+ - && || ! ( ) { } [ ] ^ " ~ * ? : \ / < > =` plus apostrophes/smart
/// quotes. Character-level only: bare boolean keywords (`AND`, `OR`) pass
/// through and can still fail the parse — [`parse_lexical_query`] handles
/// that residual case by degrading instead of erroring.
fn sanitize_query(q: &str) -> String {
    q.chars()
        .map(|c| match c {
            '\'' | '\u{2019}' | '\u{2018}' | '"' | '\u{201c}' | '\u{201d}' | '/' | '\\' => ' ',
            // Tantivy DSL operators; `&` / `|` also cover the `&&` / `||`
            // digraphs. Mapping to space stops `parse_query` erroring on
            // natural-language input.
            '+' | '-' | '!' | '(' | ')' | '{' | '}' | '[' | ']' | '^' | '~' | '*' | '?' | ':'
            | '<' | '>' | '=' | '&' | '|' => ' ',
            _ => c,
        })
        .collect()
}

/// Which attempt produced the lexical query (observable for tests/logs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LexicalParse {
    /// The raw input parsed as-is — valid Tantivy syntax, so deliberate DSL
    /// (e.g. `"quoted phrases"`) keeps its exact semantics and ranking.
    Raw,
    /// The raw input failed to parse; the [`sanitize_query`]-mapped form
    /// parsed instead (byte-identical results to the previous unconditional
    /// sanitization for every such input).
    Sanitized,
    /// Even the sanitized form failed (bare boolean keywords like `AND`,
    /// which are character-clean); lowercasing folded them into plain terms.
    /// Keeps BM25 recall on the real terms in configurations without a
    /// vector leg (Greptile #45 P1). Query-time lowercasing cannot change
    /// term matching — the default tokenizer already lowercases — it only
    /// stops the parser from recognizing operator keywords.
    Folded,
}

/// Parse the content query for the BM25 leg without ever hard-failing search.
///
/// Three-step contract (query-sanitization brief, roadmap W2):
/// 1. Try the raw input first, so currently-valid queries — including
///    deliberate `"quoted phrases"` — keep identical semantics and ranking.
/// 2. On a parse error, retry with [`sanitize_query`] (operator
///    metacharacters → space). `parse_query_lenient` is available in tantivy
///    0.25 but drops offending tokens wholesale (losing terms like
///    `SIAC_GEE?`); the sanitize-retry keeps every alphanumeric term and
///    reproduces the pre-change fallback results byte-for-byte.
/// 3. If even the sanitized form fails (bare boolean keywords like `AND`,
///    which character sanitization cannot neutralize), retry once more with
///    the sanitized form lowercased: the parser only recognizes uppercase
///    operator keywords, and the default tokenizer lowercases terms at both
///    index and query time, so folding preserves matching for every real
///    term while neutralizing the operators (Greptile #45 P1 — keeps BM25
///    recall in configurations without a usable vector leg).
/// 4. Only if all three attempts fail, log a `warn!` and return `None`:
///    callers skip the lexical leg so hybrid search degrades to the
///    remaining legs (vector/graph) instead of erroring. No known input
///    reaches this — it stays as a defensive last resort.
fn parse_lexical_query(
    parser: &QueryParser,
    query_text: &str,
) -> Option<(Box<dyn tantivy::query::Query>, LexicalParse)> {
    match parser.parse_query(query_text) {
        Ok(q) => Some((q, LexicalParse::Raw)),
        Err(raw_err) => {
            let sanitized = sanitize_query(query_text);
            match parser.parse_query(&sanitized) {
                Ok(q) => Some((q, LexicalParse::Sanitized)),
                Err(sanitized_err) => match parser.parse_query(&sanitized.to_lowercase()) {
                    Ok(q) => Some((q, LexicalParse::Folded)),
                    Err(folded_err) => {
                        warn!(
                            "BM25 leg skipped: query unparsable raw ({raw_err}), sanitized \
                             ({sanitized_err}) and keyword-folded ({folded_err}); degrading \
                             to remaining search legs"
                        );
                        None
                    }
                },
            }
        }
    }
}

#[cfg(test)]
mod sanitize_query_tests {
    //! Sanitization must stay total at the character level: the fallback in
    //! `parse_lexical_query` relies on the sanitized form parsing for any
    //! input whose failure was caused by operator metacharacters. See the
    //! query-sanitization brief (roadmap W2).
    use super::sanitize_query;

    /// Every Tantivy DSL metacharacter is neutralised to whitespace, so the
    /// sanitized output can never be parsed as operators.
    #[test]
    fn strips_all_tantivy_operators() {
        let metachars = "+-&|!(){}[]^\"~*?:\\/<>=";
        let out = sanitize_query(metachars);
        for c in metachars.chars() {
            assert!(
                !out.contains(c),
                "metacharacter {c:?} survived sanitization: {out:?}"
            );
        }
    }

    /// The verbatim question that crashed retrieval (dash, colon, question
    /// mark) is reduced to plain words; alphanumeric terms survive so recall
    /// on them is preserved.
    #[test]
    fn neutralises_natural_language_question() {
        let q = "you mentioned 6S, MAJA, and Sen2Cor - which is implemented in SIAC_GEE?";
        let out = sanitize_query(q);
        assert!(!out.contains('-'));
        assert!(!out.contains('?'));
        assert!(out.contains("Sen2Cor"));
        assert!(out.contains("SIAC_GEE"));
    }

    /// The left smart double quote (U+201C) is neutralised like its closing
    /// counterpart, so a curly-quoted query can never reach the parser as an
    /// unterminated phrase operator.
    #[test]
    fn sanitize_query_handles_left_smart_double_quote() {
        let q = "what did \u{201c}atmospheric correction\u{201d} mean";
        let out = sanitize_query(q);
        assert!(!out.contains('\u{201c}'));
        assert!(!out.contains('\u{201d}'));
        assert!(out.contains("atmospheric correction"));
    }

    /// A query with no special characters is returned unchanged, so existing
    /// clean queries keep their exact behaviour (no regression).
    #[test]
    fn leaves_clean_query_unchanged() {
        let clean = "atmospheric correction methods for Sentinel imagery";
        assert_eq!(sanitize_query(clean), clean);
    }
}

#[cfg(test)]
mod lexical_parse_tests {
    //! Three-step lexical parse contract (query-sanitization brief, W2):
    //! raw first (valid DSL keeps its semantics), sanitize-retry on error,
    //! degrade to the remaining legs when even the sanitized form fails.
    use super::*;
    use crate::config::TantivyConfig;
    use tempfile::TempDir;

    fn cfg() -> TantivyConfig {
        TantivyConfig {
            enabled: true,
            heap_size_mb: 16,
            drift_warn_pct: 5.0,
            auto_rebuild_on_drift: false,
        }
    }

    fn doc(id: &str, content: &str) -> TextDocument {
        TextDocument {
            id: id.to_string(),
            content: content.to_string(),
            user_id: "default".to_string(),
            app_id: "default".to_string(),
            level: 0,
            timestamp: 1_000,
            stream: "s1".to_string(),
            entities: None,
            relations: None,
            event_date: None,
            source_agent: None,
        }
    }

    fn seeded_index(docs: &[(&str, &str)]) -> (TempDir, TantivyIndex) {
        let tmp = TempDir::new().expect("tempdir");
        let mut idx = TantivyIndex::open(tmp.path().join("tantivy"), &cfg()).expect("open");
        for (id, content) in docs {
            idx.index_document(doc(id, content)).unwrap();
        }
        idx.commit().unwrap();
        (tmp, idx)
    }

    fn content_parser(idx: &TantivyIndex) -> QueryParser {
        QueryParser::for_index(&idx.index, vec![idx.content_field])
    }

    /// (a) The verbatim benchmark question that used to hard-fail
    /// `memory_search` with `SyntaxError` retrieves the indexed fact.
    #[test]
    fn verbatim_benchmark_question_retrieves_doc() {
        let (_tmp, idx) = seeded_index(&[
            (
                "fact",
                "6S, MAJA and Sen2Cor are atmospheric correction algorithms; \
                 6S is implemented in the SIAC_GEE tool",
            ),
            ("noise", "unrelated cycling trivia"),
        ]);
        let q = "I was going through our previous conversation about atmospheric \
                 correction methods, and I wanted to confirm - you mentioned that 6S, \
                 MAJA, and Sen2Cor are all algorithms … which one is implemented in \
                 the SIAC_GEE tool?";
        let results = idx.search(q, 10).expect("search must not hard-fail");
        assert!(
            results.iter().any(|r| r.id == "fact"),
            "expected the SIAC_GEE fact to be retrieved, got: {results:?}"
        );
    }

    /// (b) Metachar battery: none of these may return an error, across the
    /// stream/entity/date/agent search entry points as well as plain search.
    #[test]
    fn metachar_battery_never_errors() {
        let (_tmp, idx) = seeded_index(&[("d1", "foo bar narrow wide")]);
        for q in ["foo - bar?", "a:b (c)", "x/y ~z", "wide < narrow"] {
            assert!(idx.search(q, 5).is_ok(), "search({q:?}) errored");
            assert!(
                idx.search_with_stream(q, "s1", 5).is_ok(),
                "search_with_stream({q:?}) errored"
            );
            assert!(
                idx.search_with_date_range(q, 0, 2_000, 5).is_ok(),
                "search_with_date_range({q:?}) errored"
            );
            assert!(
                idx.search_with_agent(AgentSearchParams {
                    query_text: q,
                    stream: None,
                    entity: None,
                    date_range: None,
                    source_agent: None,
                    exclude_source_agents: None,
                    limit: 5,
                })
                .is_ok(),
                "search_with_agent({q:?}) errored"
            );
        }
    }

    /// (c) A deliberate `"quoted phrase"` parses via the first attempt — no
    /// fallback taken — and keeps phrase semantics (adjacency required).
    #[test]
    fn quoted_phrase_parses_raw_with_phrase_semantics() {
        let (_tmp, idx) = seeded_index(&[
            ("adjacent", "the exact phrase lives here"),
            ("scrambled", "phrase then some filler then exact"),
        ]);

        let parser = content_parser(&idx);
        let (_, mode) =
            parse_lexical_query(&parser, "\"exact phrase\"").expect("quoted phrase must parse");
        assert_eq!(mode, LexicalParse::Raw, "phrase must not take the fallback");

        let results = idx.search("\"exact phrase\"", 10).unwrap();
        let ids: Vec<&str> = results.iter().map(|r| r.id.as_str()).collect();
        assert!(ids.contains(&"adjacent"), "phrase must match adjacent doc");
        assert!(
            !ids.contains(&"scrambled"),
            "phrase must not match scrambled doc"
        );
    }

    /// Raw-unparsable input with metacharacters takes the sanitize fallback
    /// (deterministic case: `a:b` targets a nonexistent field).
    #[test]
    fn unknown_field_query_takes_sanitized_fallback() {
        let (_tmp, idx) = seeded_index(&[("d1", "a b c")]);
        let parser = content_parser(&idx);
        let (_, mode) = parse_lexical_query(&parser, "a:b (c)").expect("must parse sanitized");
        assert_eq!(mode, LexicalParse::Sanitized);
    }

    /// A bare boolean keyword survives character sanitization and still fails
    /// that attempt, but the keyword fold (lowercase) turns it into a plain
    /// term: never an error, and the term is searchable like any word.
    #[test]
    fn bare_boolean_keyword_folds_instead_of_erroring() {
        let (_tmp, idx) = seeded_index(&[("d1", "foo bar"), ("d2", "salt and pepper")]);
        let parser = content_parser(&idx);
        let (_, mode) =
            parse_lexical_query(&parser, "AND").expect("bare AND must fold, not degrade");
        assert_eq!(mode, LexicalParse::Folded);
        // The folded keyword is a plain term: it matches the doc containing
        // the word "and" and errors nowhere.
        let results = idx.search("AND", 5).expect("fold must not error");
        assert_eq!(
            results.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            vec!["d2"],
            "folded AND must match the doc containing the term 'and'"
        );
        assert!(idx.search_with_stream("AND", "s1", 5).is_ok());
    }

    /// Greptile #45 P1 scenario: a query with a real term plus a dangling
    /// boolean keyword (`foo AND`) must keep BM25 recall on the term — the
    /// lexical leg is the only leg in vector-less configurations, so it must
    /// not come back empty.
    #[test]
    fn dangling_keyword_keeps_term_recall() {
        let (_tmp, idx) = seeded_index(&[("hit", "foo lives here"), ("miss", "bar elsewhere")]);
        let results = idx.search("foo AND", 5).expect("must not error");
        assert_eq!(
            results.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            vec!["hit"],
            "term recall on 'foo' must survive the dangling keyword"
        );
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextDocument {
    pub id: String,
    pub content: String,
    pub user_id: String,
    pub app_id: String,
    pub level: i32,
    pub timestamp: i64,
    pub stream: String,
    pub entities: Option<String>,
    pub relations: Option<String>,
    pub event_date: Option<i64>,
    pub source_agent: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub id: String,
    pub content: String,
    pub user_id: String,
    pub app_id: String,
    pub level: i32,
    pub timestamp: i64,
    pub stream: String,
    pub score: f32,
}

/// Parameters for an agent-filtered Tantivy search (cycle/258, Option A).
///
/// Composes the existing per-branch filters (single `stream`, `entity`,
/// `date_range`) with the `source_agent` include / `exclude_source_agents`
/// exclude clauses, so the candidate pool is agent-scoped *at the source*
/// rather than reconstructed in the handler after pool truncation. Only the
/// fields relevant to the active retrieval branch are set; the rest are `None`.
/// See [`TantivyIndex::search_with_agent`].
pub struct AgentSearchParams<'a> {
    pub query_text: &'a str,
    pub stream: Option<&'a str>,
    pub entity: Option<&'a str>,
    pub date_range: Option<(i64, i64)>,
    pub source_agent: Option<&'a str>,
    pub exclude_source_agents: Option<&'a [String]>,
    pub limit: usize,
}

pub struct TantivyIndex {
    index: Index,
    reader: IndexReader,
    writer: IndexWriter,
    #[allow(dead_code)]
    schema: Schema,
    id_field: Field,
    content_field: Field,
    user_id_field: Field,
    app_id_field: Field,
    level_field: Field,
    timestamp_field: Field,
    stream_field: Field,
    entities_field: Field,
    relations_field: Field,
    event_date_field: Field,
    source_agent_field: Field,
}

impl TantivyIndex {
    pub fn open<P: AsRef<Path>>(path: P, config: &TantivyConfig) -> Result<Self> {
        let path = path.as_ref();
        info!("Opening Tantivy index at: {}", path.display());

        // Build schema
        let mut schema_builder = Schema::builder();
        let id_field = schema_builder.add_text_field("id", STRING | STORED);
        let content_field = schema_builder.add_text_field("content", TEXT | STORED);
        let user_id_field = schema_builder.add_text_field("user_id", STRING | STORED);
        let app_id_field = schema_builder.add_text_field("app_id", STRING | STORED);
        let level_field = schema_builder.add_i64_field("level", STORED | INDEXED);
        let timestamp_field = schema_builder.add_i64_field("timestamp", STORED | INDEXED | FAST);
        let stream_field = schema_builder.add_text_field("stream", STRING | STORED);
        let entities_field = schema_builder.add_text_field("entities", TEXT | STORED);
        let relations_field = schema_builder.add_text_field("relations", TEXT | STORED);
        let event_date_field = schema_builder.add_i64_field("event_date", STORED | INDEXED | FAST);
        let source_agent_field = schema_builder.add_text_field("source_agent", STRING | STORED);
        let schema = schema_builder.build();

        // Open or create index; on schema mismatch, wipe and recreate
        std::fs::create_dir_all(path)?;
        let index = match Index::open_or_create(
            tantivy::directory::MmapDirectory::open(path)?,
            schema.clone(),
        ) {
            Ok(idx) => idx,
            Err(e) if e.to_string().contains("schema") || e.to_string().contains("Schema") => {
                info!("Tantivy schema mismatch — deleting old index and recreating");
                // Remove all files in tantivy dir, then recreate
                for entry in std::fs::read_dir(path)? {
                    let entry = entry?;
                    let _ = std::fs::remove_file(entry.path());
                }
                Index::open_or_create(
                    tantivy::directory::MmapDirectory::open(path)?,
                    schema.clone(),
                )
                .context("Failed to create fresh Tantivy index after schema migration")?
            }
            Err(e) => return Err(e).context("Failed to open Tantivy index"),
        };

        // Create writer with heap size from config
        let heap_size = config.heap_size_mb * 1024 * 1024;
        let writer = index
            .writer(heap_size)
            .context("Failed to create Tantivy writer")?;

        // Create reader
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()
            .context("Failed to create Tantivy reader")?;

        info!("Tantivy index opened successfully");

        Ok(Self {
            index,
            reader,
            writer,
            schema,
            id_field,
            content_field,
            user_id_field,
            app_id_field,
            level_field,
            timestamp_field,
            stream_field,
            entities_field,
            relations_field,
            event_date_field,
            source_agent_field,
        })
    }

    /// Delete + insert (upsert). Safe to call on existing or new docs.
    pub fn upsert_document(&mut self, doc: TextDocument) -> Result<()> {
        let id_term = tantivy::Term::from_field_text(self.id_field, &doc.id);
        self.writer.delete_term(id_term);
        self.index_document(doc)
    }

    pub fn index_document(&mut self, doc: TextDocument) -> Result<()> {
        debug!("Indexing document: id={}", doc.id);

        let mut tantivy_doc = doc!(
            self.id_field => doc.id,
            self.content_field => doc.content,
            self.user_id_field => doc.user_id,
            self.app_id_field => doc.app_id,
            self.level_field => doc.level as i64,
            self.timestamp_field => doc.timestamp,
            self.stream_field => doc.stream,
            self.event_date_field => doc.event_date.unwrap_or(doc.timestamp),
        );

        if let Some(entities) = doc.entities {
            tantivy_doc.add_text(self.entities_field, entities);
        }

        if let Some(relations) = doc.relations {
            tantivy_doc.add_text(self.relations_field, relations);
        }

        tantivy_doc.add_text(
            self.source_agent_field,
            doc.source_agent.as_deref().unwrap_or("unknown"),
        );

        self.writer
            .add_document(tantivy_doc)
            .context("Failed to add document to Tantivy")?;

        Ok(())
    }

    pub fn commit(&mut self) -> Result<()> {
        debug!("Committing Tantivy index");
        self.writer
            .commit()
            .context("Failed to commit Tantivy index")?;
        self.reader
            .reload()
            .context("Failed to reload Tantivy reader after commit")?;
        Ok(())
    }

    pub fn delete_document(&mut self, id: &str) -> Result<()> {
        debug!("Deleting document from Tantivy: id={}", id);
        let id_term = tantivy::Term::from_field_text(self.id_field, id);
        self.writer.delete_term(id_term);
        self.writer.commit().context("Failed to commit deletion")?;
        Ok(())
    }

    pub fn search(&self, query_text: &str, limit: usize) -> Result<Vec<SearchResult>> {
        debug!("Searching: query='{}', limit={}", query_text, limit);

        let searcher = self.reader.searcher();

        // Parse query: content is primary signal, entities/relations are weak tiebreakers
        let mut query_parser = QueryParser::for_index(
            &self.index,
            vec![
                self.content_field,
                self.entities_field,
                self.relations_field,
            ],
        );
        query_parser.set_field_boost(self.content_field, 1.0);
        query_parser.set_field_boost(self.entities_field, 0.2);
        query_parser.set_field_boost(self.relations_field, 0.2);
        let Some((query, _)) = parse_lexical_query(&query_parser, query_text) else {
            return Ok(Vec::new());
        };

        // Execute search
        let top_docs = searcher
            .search(&query, &TopDocs::with_limit(limit))
            .context("Failed to execute search")?;

        // Convert results
        let mut results = Vec::new();
        for (_score, doc_address) in top_docs {
            let retrieved_doc: tantivy::TantivyDocument = searcher
                .doc(doc_address)
                .context("Failed to retrieve document")?;

            let id = retrieved_doc
                .get_first(self.id_field)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            let content = retrieved_doc
                .get_first(self.content_field)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            let user_id = retrieved_doc
                .get_first(self.user_id_field)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            let app_id = retrieved_doc
                .get_first(self.app_id_field)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            let level = retrieved_doc
                .get_first(self.level_field)
                .and_then(|v| v.as_i64())
                .unwrap_or(0) as i32;

            let timestamp = retrieved_doc
                .get_first(self.timestamp_field)
                .and_then(|v| v.as_i64())
                .unwrap_or(0);

            let stream = retrieved_doc
                .get_first(self.stream_field)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            results.push(SearchResult {
                id,
                content,
                user_id,
                app_id,
                level,
                timestamp,
                stream,
                score: _score,
            });
        }

        Ok(results)
    }

    pub fn search_with_stream(
        &self,
        query_text: &str,
        stream: &str,
        limit: usize,
    ) -> Result<Vec<SearchResult>> {
        debug!(
            "Searching with stream filter: query='{}', stream='{}', limit={}",
            query_text, stream, limit
        );

        let searcher = self.reader.searcher();

        // Parse query (searches in content field) and add stream filter
        let query_parser = QueryParser::for_index(&self.index, vec![self.content_field]);
        let Some((content_query, _)) = parse_lexical_query(&query_parser, query_text) else {
            return Ok(Vec::new());
        };

        // Create term query for stream
        let stream_term = tantivy::Term::from_field_text(self.stream_field, stream);
        let stream_query =
            tantivy::query::TermQuery::new(stream_term, tantivy::schema::IndexRecordOption::Basic);

        // Combine with boolean query (AND)
        let combined_query = tantivy::query::BooleanQuery::new(vec![
            (tantivy::query::Occur::Must, Box::new(content_query)),
            (tantivy::query::Occur::Must, Box::new(stream_query)),
        ]);

        // Execute search
        let top_docs = searcher
            .search(&combined_query, &TopDocs::with_limit(limit))
            .context("Failed to execute search with stream filter")?;

        // Convert results
        let mut results = Vec::new();
        for (_score, doc_address) in top_docs {
            let retrieved_doc: tantivy::TantivyDocument = searcher
                .doc(doc_address)
                .context("Failed to retrieve document")?;

            let id = retrieved_doc
                .get_first(self.id_field)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            let content = retrieved_doc
                .get_first(self.content_field)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            let user_id = retrieved_doc
                .get_first(self.user_id_field)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            let app_id = retrieved_doc
                .get_first(self.app_id_field)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            let level = retrieved_doc
                .get_first(self.level_field)
                .and_then(|v| v.as_i64())
                .unwrap_or(0) as i32;

            let timestamp = retrieved_doc
                .get_first(self.timestamp_field)
                .and_then(|v| v.as_i64())
                .unwrap_or(0);

            let stream = retrieved_doc
                .get_first(self.stream_field)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            results.push(SearchResult {
                id,
                content,
                user_id,
                app_id,
                level,
                timestamp,
                stream,
                score: _score,
            });
        }

        Ok(results)
    }

    pub fn search_with_entity(
        &self,
        query_text: &str,
        entity: &str,
        limit: usize,
    ) -> Result<Vec<SearchResult>> {
        debug!(
            "Searching with entity filter: query='{}', entity='{}', limit={}",
            query_text, entity, limit
        );

        let searcher = self.reader.searcher();

        // Parse content query: entity search keeps entities at 1.0 (user explicitly asked)
        let mut query_parser = QueryParser::for_index(
            &self.index,
            vec![
                self.content_field,
                self.entities_field,
                self.relations_field,
            ],
        );
        query_parser.set_field_boost(self.content_field, 1.0);
        query_parser.set_field_boost(self.entities_field, 1.0);
        query_parser.set_field_boost(self.relations_field, 0.5);
        let Some((content_query, _)) = parse_lexical_query(&query_parser, query_text) else {
            return Ok(Vec::new());
        };

        // Create query for entity filter
        let entity_query_parser = QueryParser::for_index(&self.index, vec![self.entities_field]);
        let entity_query = entity_query_parser
            .parse_query(&sanitize_query(entity))
            .context("Failed to parse entity filter")?;

        // Combine with boolean query (AND)
        let combined_query = tantivy::query::BooleanQuery::new(vec![
            (tantivy::query::Occur::Must, Box::new(content_query)),
            (tantivy::query::Occur::Must, Box::new(entity_query)),
        ]);

        // Execute search
        let top_docs = searcher
            .search(&combined_query, &TopDocs::with_limit(limit))
            .context("Failed to execute search with entity filter")?;

        // Convert results
        let mut results = Vec::new();
        for (_score, doc_address) in top_docs {
            let retrieved_doc: tantivy::TantivyDocument = searcher
                .doc(doc_address)
                .context("Failed to retrieve document")?;

            let id = retrieved_doc
                .get_first(self.id_field)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            let content = retrieved_doc
                .get_first(self.content_field)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            let user_id = retrieved_doc
                .get_first(self.user_id_field)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            let app_id = retrieved_doc
                .get_first(self.app_id_field)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            let level = retrieved_doc
                .get_first(self.level_field)
                .and_then(|v| v.as_i64())
                .unwrap_or(0) as i32;

            let timestamp = retrieved_doc
                .get_first(self.timestamp_field)
                .and_then(|v| v.as_i64())
                .unwrap_or(0);

            let stream = retrieved_doc
                .get_first(self.stream_field)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            results.push(SearchResult {
                id,
                content,
                user_id,
                app_id,
                level,
                timestamp,
                stream,
                score: _score,
            });
        }

        Ok(results)
    }

    pub fn merge_segments(&mut self) -> Result<()> {
        info!("Merging Tantivy segments");
        // Note: Tantivy 0.22 doesn't have merge_segments() API
        // Segments are automatically managed and merged
        // This is a no-op placeholder for future API compatibility
        Ok(())
    }

    pub fn count(&self) -> Result<u64> {
        let searcher = self.reader.searcher();
        let count = searcher.num_docs();
        Ok(count)
    }

    /// Number of documents indexed for a single `stream`. Runs a `TermQuery` on
    /// the exact-match `stream` field with tantivy's `Count` collector — no doc
    /// materialization, cheap enough for a stats request. Surfaced by the
    /// stream-stats endpoint as a per-stream BM25 retrieval-readiness signal:
    /// a gap between this and the chunk-store count means some chunks are
    /// missing from full-text search.
    pub fn count_stream(&self, stream: &str) -> Result<u64> {
        let searcher = self.reader.searcher();
        let stream_term = tantivy::Term::from_field_text(self.stream_field, stream);
        let stream_query =
            tantivy::query::TermQuery::new(stream_term, tantivy::schema::IndexRecordOption::Basic);
        let count = searcher
            .search(&stream_query, &tantivy::collector::Count)
            .context("Failed to count stream docs in tantivy index")?;
        u64::try_from(count).context("tantivy stream doc count exceeds u64")
    }

    /// Tokenize `text` with the analyzer registered for the `content` field —
    /// the same pipeline that produced the index terms, so tokens returned
    /// here agree byte-for-byte with the posting lists that `doc_freq_content`
    /// and `term_candidates` consult (cycle/012 rare-term lane).
    pub fn tokenize_content(&self, text: &str) -> Result<Vec<String>> {
        let mut analyzer = self
            .index
            .tokenizer_for_field(self.content_field)
            .context("No tokenizer registered for content field")?;
        let mut stream = analyzer.token_stream(text);
        let mut tokens = Vec::new();
        while let Some(token) = stream.next() {
            tokens.push(token.text.clone());
        }
        Ok(tokens)
    }

    /// Document frequency of a single already-tokenized term in the `content`
    /// field. Reads the index term dictionary only — no doc materialization.
    pub fn doc_freq_content(&self, token: &str) -> Result<u64> {
        let searcher = self.reader.searcher();
        let term = tantivy::Term::from_field_text(self.content_field, token);
        let df = searcher
            .doc_freq(&term)
            .context("Failed to read doc_freq from tantivy index")?;
        Ok(df)
    }

    /// Stream-scoped document frequency of a `content` term: number of
    /// documents in `stream` whose content contains `token`. Counted via a
    /// posting-list intersection (`content:token AND stream:stream`) with
    /// tantivy's `Count` collector — same index, no doc materialization.
    /// Greptile PR#53 P1: keeps the rarity decision consistent with the
    /// stream-scoped corpus size when a search targets a single stream
    /// (a token rare *inside* the stream must not be masked by global DF).
    pub fn doc_freq_content_in_stream(&self, token: &str, stream: &str) -> Result<u64> {
        let searcher = self.reader.searcher();
        let content_term = tantivy::Term::from_field_text(self.content_field, token);
        let content_query =
            tantivy::query::TermQuery::new(content_term, tantivy::schema::IndexRecordOption::Basic);
        let stream_term = tantivy::Term::from_field_text(self.stream_field, stream);
        let stream_query =
            tantivy::query::TermQuery::new(stream_term, tantivy::schema::IndexRecordOption::Basic);
        let query = tantivy::query::BooleanQuery::new(vec![
            (
                tantivy::query::Occur::Must,
                Box::new(content_query) as Box<dyn tantivy::query::Query>,
            ),
            (
                tantivy::query::Occur::Must,
                Box::new(stream_query) as Box<dyn tantivy::query::Query>,
            ),
        ]);
        let count = searcher
            .search(&query, &tantivy::collector::Count)
            .context("Failed to count stream-scoped doc_freq in tantivy index")?;
        u64::try_from(count).context("stream-scoped doc_freq exceeds u64")
    }

    /// Posting-list retrieval for the rare-term lane (cycle/012): fetch the
    /// top `limit` documents (by BM25 score) containing **any** of `tokens`
    /// in the `content` field, optionally restricted to a single `stream`.
    /// Uses `TermQuery`s over the same index/field as the BM25 channel — no
    /// separate content scan, no query parsing.
    pub fn term_candidates(
        &self,
        tokens: &[String],
        stream: Option<&str>,
        limit: usize,
    ) -> Result<Vec<SearchResult>> {
        if tokens.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let searcher = self.reader.searcher();

        let token_clauses: Vec<(tantivy::query::Occur, Box<dyn tantivy::query::Query>)> = tokens
            .iter()
            .map(|t| {
                let term = tantivy::Term::from_field_text(self.content_field, t);
                let tq = tantivy::query::TermQuery::new(
                    term,
                    tantivy::schema::IndexRecordOption::WithFreqs,
                );
                (
                    tantivy::query::Occur::Should,
                    Box::new(tq) as Box<dyn tantivy::query::Query>,
                )
            })
            .collect();
        let token_query = tantivy::query::BooleanQuery::new(token_clauses);

        let query: Box<dyn tantivy::query::Query> = if let Some(stream) = stream {
            let stream_term = tantivy::Term::from_field_text(self.stream_field, stream);
            let stream_query = tantivy::query::TermQuery::new(
                stream_term,
                tantivy::schema::IndexRecordOption::Basic,
            );
            Box::new(tantivy::query::BooleanQuery::new(vec![
                (tantivy::query::Occur::Must, Box::new(token_query)),
                (tantivy::query::Occur::Must, Box::new(stream_query)),
            ]))
        } else {
            Box::new(token_query)
        };

        let top_docs = searcher
            .search(&query, &TopDocs::with_limit(limit))
            .context("Failed to execute rare-term candidate search")?;

        let mut results = Vec::new();
        for (score, doc_address) in top_docs {
            let doc: tantivy::TantivyDocument = searcher
                .doc(doc_address)
                .context("Failed to retrieve rare-term candidate document")?;
            results.push(self.doc_to_search_result(&doc, score));
        }
        Ok(results)
    }

    /// Compact `TantivyDocument → SearchResult` conversion for cycle/012
    /// methods. Mirrors the inline conversion used by the `search*` family;
    /// intentionally private and additive — existing methods keep their
    /// inline copies untouched (no drive-by refactor).
    fn doc_to_search_result(&self, doc: &tantivy::TantivyDocument, score: f32) -> SearchResult {
        let text = |field: Field| -> String {
            doc.get_first(field)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()
        };
        let level_i64 = doc
            .get_first(self.level_field)
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        SearchResult {
            id: text(self.id_field),
            content: text(self.content_field),
            user_id: text(self.user_id_field),
            app_id: text(self.app_id_field),
            level: i32::try_from(level_i64).unwrap_or(0),
            timestamp: doc
                .get_first(self.timestamp_field)
                .and_then(|v| v.as_i64())
                .unwrap_or(0),
            stream: text(self.stream_field),
            score,
        }
    }

    /// Search with date range filter
    pub fn search_with_date_range(
        &self,
        query_text: &str,
        start_ts: i64,
        end_ts: i64,
        limit: usize,
    ) -> Result<Vec<SearchResult>> {
        debug!(
            "Searching with date range: query='{}', start={}, end={}, limit={}",
            query_text, start_ts, end_ts, limit
        );

        let searcher = self.reader.searcher();

        // Build date range query on event_date (semantic date, falls back to timestamp at index time)
        let lower = std::ops::Bound::Included(tantivy::Term::from_field_i64(
            self.event_date_field,
            start_ts,
        ));
        let upper =
            std::ops::Bound::Included(tantivy::Term::from_field_i64(self.event_date_field, end_ts));
        let range_query = RangeQuery::new(lower, upper);

        // If query text is empty, only use date range
        let combined_query: Box<dyn tantivy::query::Query> = if query_text.trim().is_empty() {
            Box::new(range_query)
        } else {
            // Parse content query: entities/relations as weak tiebreakers
            let mut query_parser = QueryParser::for_index(
                &self.index,
                vec![
                    self.content_field,
                    self.entities_field,
                    self.relations_field,
                ],
            );
            query_parser.set_field_boost(self.content_field, 1.0);
            query_parser.set_field_boost(self.entities_field, 0.2);
            query_parser.set_field_boost(self.relations_field, 0.2);
            let Some((content_query, _)) = parse_lexical_query(&query_parser, query_text) else {
                return Ok(Vec::new());
            };

            // Combine with AND
            Box::new(BooleanQuery::new(vec![
                (Occur::Must, Box::new(content_query)),
                (Occur::Must, Box::new(range_query)),
            ]))
        };

        // Execute search
        let top_docs = searcher
            .search(&*combined_query, &TopDocs::with_limit(limit))
            .context("Failed to execute search with date range")?;

        // Convert results
        let mut results = Vec::new();
        for (_score, doc_address) in top_docs {
            let retrieved_doc: tantivy::TantivyDocument = searcher
                .doc(doc_address)
                .context("Failed to retrieve document")?;

            let id = retrieved_doc
                .get_first(self.id_field)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            let content = retrieved_doc
                .get_first(self.content_field)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            let user_id = retrieved_doc
                .get_first(self.user_id_field)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            let app_id = retrieved_doc
                .get_first(self.app_id_field)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            let level = retrieved_doc
                .get_first(self.level_field)
                .and_then(|v| v.as_i64())
                .unwrap_or(0) as i32;

            let timestamp = retrieved_doc
                .get_first(self.timestamp_field)
                .and_then(|v| v.as_i64())
                .unwrap_or(0);

            let stream = retrieved_doc
                .get_first(self.stream_field)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            results.push(SearchResult {
                id,
                content,
                user_id,
                app_id,
                level,
                timestamp,
                stream,
                score: _score,
            });
        }

        // Sort by timestamp descending if no text query
        if query_text.trim().is_empty() {
            results.sort_by_key(|b| std::cmp::Reverse(b.timestamp));
        }

        Ok(results)
    }

    /// Agent-filtered search (cycle/258, Option A). Builds a single
    /// `BooleanQuery` that pushes the `source_agent` include / `exclude` filter
    /// into Tantivy alongside the active branch's content + (optional)
    /// stream/entity/date-range clauses, so the returned pool is already
    /// agent-scoped — no post-retrieval per-id `get_chunk` and no starvation
    /// when the target agent is a minority in the global ranking.
    ///
    /// Parity with the canonical RocksDB-side filter (`unknown` token at index
    /// time, `tantivy_index.rs:197`): include (`source_agent=X`) is a MUST term
    /// so agent-less chunks (indexed `"unknown"`) do not match and are dropped;
    /// exclude (`exclude_source_agents=[Y]`) is a MUST_NOT term so agent-less
    /// chunks are kept. A query that filters `source_agent="unknown"` selects
    /// the agent-less chunks — the correct consequence of the projection.
    pub fn search_with_agent(&self, params: AgentSearchParams) -> Result<Vec<SearchResult>> {
        let searcher = self.reader.searcher();
        let mut clauses: Vec<(Occur, Box<dyn tantivy::query::Query>)> = Vec::new();

        // Positive anchor: content query (entities/relations weak tiebreakers,
        // mirroring `search`), or match-all when the query text is empty so an
        // exclude-only filter still returns the complement rather than nothing.
        if params.query_text.trim().is_empty() {
            clauses.push((Occur::Must, Box::new(tantivy::query::AllQuery)));
        } else {
            let mut query_parser = QueryParser::for_index(
                &self.index,
                vec![
                    self.content_field,
                    self.entities_field,
                    self.relations_field,
                ],
            );
            // #259 (Greptile P2): when an entity filter is active, mirror
            // search_with_entity's boosts (entities 1.0, relations 0.5) so the
            // agent+entity path ranks entity-rich docs identically to the
            // pure-entity path; otherwise keep the common weak tiebreakers.
            let (ent_boost, rel_boost) = if params.entity.is_some() {
                (1.0, 0.5)
            } else {
                (0.2, 0.2)
            };
            query_parser.set_field_boost(self.content_field, 1.0);
            query_parser.set_field_boost(self.entities_field, ent_boost);
            query_parser.set_field_boost(self.relations_field, rel_boost);
            let Some((content_query, _)) = parse_lexical_query(&query_parser, params.query_text)
            else {
                return Ok(Vec::new());
            };
            clauses.push((Occur::Must, content_query));
        }

        if let Some(stream) = params.stream {
            let term = tantivy::Term::from_field_text(self.stream_field, stream);
            clauses.push((
                Occur::Must,
                Box::new(tantivy::query::TermQuery::new(
                    term,
                    tantivy::schema::IndexRecordOption::Basic,
                )),
            ));
        }

        if let Some(entity) = params.entity {
            let entity_parser = QueryParser::for_index(&self.index, vec![self.entities_field]);
            let entity_query = entity_parser
                .parse_query(&sanitize_query(entity))
                .context("Failed to parse entity filter")?;
            clauses.push((Occur::Must, entity_query));
        }

        if let Some((start_ts, end_ts)) = params.date_range {
            let lower = std::ops::Bound::Included(tantivy::Term::from_field_i64(
                self.event_date_field,
                start_ts,
            ));
            let upper = std::ops::Bound::Included(tantivy::Term::from_field_i64(
                self.event_date_field,
                end_ts,
            ));
            clauses.push((Occur::Must, Box::new(RangeQuery::new(lower, upper))));
        }

        self.push_agent_clauses(
            &mut clauses,
            params.source_agent,
            params.exclude_source_agents,
        );

        let query = BooleanQuery::new(clauses);
        let top_docs = searcher
            .search(&query, &TopDocs::with_limit(params.limit))
            .context("Failed to execute agent-filtered search")?;
        self.collect_results(&searcher, top_docs)
    }

    /// The id set of every chunk matching the agent filter, collected in full
    /// (`DocSetCollector`, not a `top_k`-bounded `TopDocs`). Feeds the vector
    /// path's pre-filter: `all_embeddings.retain(|(id, _)| set.contains(id))`
    /// before scoring, replacing the per-embedding `get_chunk` (cycle/258).
    /// Same `unknown`-token parity as [`Self::search_with_agent`].
    pub fn ids_matching_agent(
        &self,
        source_agent: Option<&str>,
        exclude_source_agents: Option<&[String]>,
    ) -> Result<std::collections::HashSet<String>> {
        let searcher = self.reader.searcher();
        let mut clauses: Vec<(Occur, Box<dyn tantivy::query::Query>)> =
            vec![(Occur::Must, Box::new(tantivy::query::AllQuery))];
        self.push_agent_clauses(&mut clauses, source_agent, exclude_source_agents);

        let query = BooleanQuery::new(clauses);
        let doc_set = searcher
            .search(&query, &tantivy::collector::DocSetCollector)
            .context("Failed to collect agent-matching doc set")?;

        let mut ids = std::collections::HashSet::with_capacity(doc_set.len());
        for doc_address in doc_set {
            let doc: tantivy::TantivyDocument = searcher
                .doc(doc_address)
                .context("Failed to retrieve document")?;
            if let Some(id) = doc.get_first(self.id_field).and_then(|v| v.as_str()) {
                ids.insert(id.to_string());
            }
        }
        Ok(ids)
    }

    /// Append the `source_agent` include (MUST) and `exclude_source_agents`
    /// (MUST_NOT) term clauses shared by [`Self::search_with_agent`] and
    /// [`Self::ids_matching_agent`].
    fn push_agent_clauses(
        &self,
        clauses: &mut Vec<(Occur, Box<dyn tantivy::query::Query>)>,
        source_agent: Option<&str>,
        exclude_source_agents: Option<&[String]>,
    ) {
        if let Some(agent) = source_agent {
            let term = tantivy::Term::from_field_text(self.source_agent_field, agent);
            clauses.push((
                Occur::Must,
                Box::new(tantivy::query::TermQuery::new(
                    term,
                    tantivy::schema::IndexRecordOption::Basic,
                )),
            ));
        }
        if let Some(excludes) = exclude_source_agents {
            for excluded in excludes {
                let term = tantivy::Term::from_field_text(self.source_agent_field, excluded);
                clauses.push((
                    Occur::MustNot,
                    Box::new(tantivy::query::TermQuery::new(
                        term,
                        tantivy::schema::IndexRecordOption::Basic,
                    )),
                ));
            }
        }
    }

    /// Convert `TopDocs` hits into `SearchResult`s. Extracted so the
    /// agent-filtered path stays within the §1 per-fn limits; the pre-existing
    /// `search*` methods keep their own inline copies (untouched, surgical).
    fn collect_results(
        &self,
        searcher: &tantivy::Searcher,
        top_docs: Vec<(f32, tantivy::DocAddress)>,
    ) -> Result<Vec<SearchResult>> {
        let mut results = Vec::with_capacity(top_docs.len());
        for (score, doc_address) in top_docs {
            let retrieved_doc: tantivy::TantivyDocument = searcher
                .doc(doc_address)
                .context("Failed to retrieve document")?;
            results.push(SearchResult {
                id: retrieved_doc
                    .get_first(self.id_field)
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                content: retrieved_doc
                    .get_first(self.content_field)
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                user_id: retrieved_doc
                    .get_first(self.user_id_field)
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                app_id: retrieved_doc
                    .get_first(self.app_id_field)
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                level: i32::try_from(
                    retrieved_doc
                        .get_first(self.level_field)
                        .and_then(|v| v.as_i64())
                        .unwrap_or(0),
                )
                .unwrap_or(0),
                timestamp: retrieved_doc
                    .get_first(self.timestamp_field)
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0),
                stream: retrieved_doc
                    .get_first(self.stream_field)
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                score,
            });
        }
        Ok(results)
    }

    /// Delete all documents from the index
    pub fn delete_all(&mut self) -> Result<()> {
        info!("Deleting all documents from Tantivy index");
        self.writer.delete_all_documents()?;
        self.writer.commit()?;
        Ok(())
    }

    /// Rebuild index from RocksDB
    pub fn rebuild_from_rocksdb(&mut self, store: &RocksDbStore) -> Result<()> {
        info!("Starting Tantivy index rebuild from RocksDB");

        // Delete all existing documents
        self.delete_all()?;

        // Get all chunks from RocksDB (post-migration: all data is in chunk:L0/L1)
        let chunks = store.get_all_chunks()?;
        info!("Found {} chunks to reindex", chunks.len());

        let mut indexed_count = 0;

        for chunk in chunks {
            let entities_str = match store.get_entities(&chunk.id, &chunk.stream) {
                Ok(entities) if !entities.is_empty() => Some(entities.join(",")),
                _ => None,
            };

            let relations_str = match store.get_relations(&chunk.id, &chunk.stream) {
                Ok(rels) if !rels.is_empty() => {
                    let rel_text: Vec<String> = rels
                        .iter()
                        .map(|(s, r, o)| format!("{} {} {}", s.to_lowercase(), r, o.to_lowercase()))
                        .collect();
                    Some(rel_text.join(", "))
                }
                _ => None,
            };

            let event_date_ts: Option<i64> = chunk
                .extraction_meta
                .as_ref()
                .and_then(|m| m.event_date.as_ref())
                .and_then(|d| chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d").ok())
                .map(|d| {
                    d.and_hms_opt(12, 0, 0)
                        .expect("valid static HMS")
                        .and_utc()
                        .timestamp()
                });

            let doc = TextDocument {
                id: chunk.id.clone(),
                content: chunk.content.clone(),
                user_id: "default".to_string(),
                app_id: "default".to_string(),
                level: chunk.level,
                timestamp: chunk.timestamp as i64,
                stream: chunk.stream.clone(),
                entities: entities_str,
                relations: relations_str,
                event_date: event_date_ts,
                source_agent: chunk.source.as_ref().map(|s| s.agent.clone()),
            };

            self.index_document(doc)?;
            indexed_count += 1;
        }

        // Final commit
        self.commit()?;
        info!(
            "Tantivy index rebuild complete: {} documents indexed (events + chunks)",
            indexed_count
        );

        Ok(())
    }
}

#[cfg(test)]
mod agent_search_tests {
    //! Cycle/258 — agent term incl/excl/`unknown`-parity for the two new
    //! Tantivy entry points. These exercise the source-level projection that
    //! Option A relies on; the handler-level e2e tests live in
    //! `loomem-server/src/handlers/search.rs::agent_filter_tests`.
    use super::*;
    use crate::config::TantivyConfig;
    use tempfile::TempDir;

    fn cfg() -> TantivyConfig {
        TantivyConfig {
            enabled: true,
            heap_size_mb: 16,
            drift_warn_pct: 5.0,
            auto_rebuild_on_drift: false,
        }
    }

    /// `agent` = `Some("x")` → indexed as `"x"`; `None` → indexed as `"unknown"`
    /// (`index_document`). Content shares the token "memo" so the content
    /// MUST clause matches every doc and only the agent clause discriminates.
    fn doc(id: &str, agent: Option<&str>) -> TextDocument {
        TextDocument {
            id: id.to_string(),
            content: "memo body".to_string(),
            user_id: "default".to_string(),
            app_id: "default".to_string(),
            level: 0,
            timestamp: 1_000,
            stream: "s1".to_string(),
            entities: None,
            relations: None,
            event_date: None,
            source_agent: agent.map(|a| a.to_string()),
        }
    }

    fn seeded_index() -> (TempDir, TantivyIndex) {
        let tmp = TempDir::new().expect("tempdir");
        let mut idx = TantivyIndex::open(tmp.path().join("tantivy"), &cfg()).expect("open");
        idx.index_document(doc("a1", Some("agentA"))).unwrap();
        idx.index_document(doc("a2", Some("agentA"))).unwrap();
        idx.index_document(doc("b1", Some("agentB"))).unwrap();
        idx.index_document(doc("u1", None)).unwrap(); // -> "unknown"
        idx.commit().unwrap();
        (tmp, idx)
    }

    fn ids(results: &[SearchResult]) -> std::collections::HashSet<String> {
        results.iter().map(|r| r.id.clone()).collect()
    }

    fn params<'a>(agent: Option<&'a str>, exclude: Option<&'a [String]>) -> AgentSearchParams<'a> {
        AgentSearchParams {
            query_text: "memo",
            stream: None,
            entity: None,
            date_range: None,
            source_agent: agent,
            exclude_source_agents: exclude,
            limit: 50,
        }
    }

    #[test]
    fn search_with_agent_include_drops_other_and_absent() {
        // MUST source_agent:agentA → only agentA docs; agentB and the
        // agent-less ("unknown") doc are excluded. Fails pre-change because
        // `search`/`search_with_stream` never constrain source_agent.
        let (_tmp, idx) = seeded_index();
        let got = ids(&idx.search_with_agent(params(Some("agentA"), None)).unwrap());
        assert_eq!(got, ["a1", "a2"].iter().map(|s| s.to_string()).collect());
    }

    #[test]
    fn search_with_agent_exclude_keeps_absent() {
        // MUST_NOT source_agent:agentA → agentB kept, the agent-less doc kept
        // (its "unknown" token != agentA), agentA dropped.
        let (_tmp, idx) = seeded_index();
        let excl = vec!["agentA".to_string()];
        let got = ids(&idx.search_with_agent(params(None, Some(&excl))).unwrap());
        assert_eq!(got, ["b1", "u1"].iter().map(|s| s.to_string()).collect());
    }

    #[test]
    fn search_with_agent_unknown_selects_agentless() {
        // Filtering on the literal "unknown" token selects the agent-less doc
        // (documented consequence of the index-time projection).
        let (_tmp, idx) = seeded_index();
        let got = ids(&idx
            .search_with_agent(params(Some("unknown"), None))
            .unwrap());
        assert_eq!(got, ["u1"].iter().map(|s| s.to_string()).collect());
    }

    #[test]
    fn ids_matching_agent_include_exclude_parity() {
        // Same projection via the full-collect id-set path (vector pre-filter).
        let (_tmp, idx) = seeded_index();
        assert_eq!(
            idx.ids_matching_agent(Some("agentA"), None).unwrap(),
            ["a1", "a2"].iter().map(|s| s.to_string()).collect()
        );
        let excl = vec!["agentA".to_string()];
        assert_eq!(
            idx.ids_matching_agent(None, Some(&excl)).unwrap(),
            ["b1", "u1"].iter().map(|s| s.to_string()).collect()
        );
    }

    /// Doc with explicit content + entities fields (boost-sensitivity tests).
    fn ent_doc(id: &str, agent: &str, content: &str, entities: &str) -> TextDocument {
        TextDocument {
            id: id.to_string(),
            content: content.to_string(),
            user_id: "default".to_string(),
            app_id: "default".to_string(),
            level: 0,
            timestamp: 1_000,
            stream: "s1".to_string(),
            entities: Some(entities.to_string()),
            relations: None,
            event_date: None,
            source_agent: Some(agent.to_string()),
        }
    }

    #[test]
    fn agent_entity_path_weights_entity_match_like_entity_path() {
        // Regression (#259, Greptile P2): a query term matching ONLY in the
        // entities field must be weighted at entities=1.0 on the agent+entity
        // path, exactly as search_with_entity does — not the common 0.2. The
        // agent path only adds the agent MUST term, so for such a doc its score
        // must be >= the entity path's. With the bug (entities=0.2) the agent
        // score drops ~0.8*s below. Background agentA docs keep the agent term's
        // idf low; "zeta" stays rare (high idf) so the boost dominates.
        let tmp = TempDir::new().expect("tempdir");
        let mut idx = TantivyIndex::open(tmp.path().join("tantivy"), &cfg()).expect("open");
        for i in 0..6 {
            idx.index_document(ent_doc(&format!("bg{i}"), "agentA", "other body", "other"))
                .unwrap();
        }
        // Target: the query term "zeta" appears only in the entities field.
        idx.index_document(ent_doc("e1", "agentA", "filler body", "zeta"))
            .unwrap();
        idx.commit().unwrap();

        let entity_hits = idx.search_with_entity("zeta", "zeta", 10).unwrap();
        let p = AgentSearchParams {
            query_text: "zeta",
            stream: None,
            entity: Some("zeta"),
            date_range: None,
            source_agent: Some("agentA"),
            exclude_source_agents: None,
            limit: 10,
        };
        let agent_hits = idx.search_with_agent(p).unwrap();

        assert_eq!(entity_hits.len(), 1, "only e1 matches the entity path");
        assert_eq!(agent_hits.len(), 1, "only e1 matches the agent+entity path");
        assert_eq!(entity_hits[0].id, "e1");
        assert_eq!(agent_hits[0].id, "e1");
        assert!(
            agent_hits[0].score >= entity_hits[0].score,
            "agent+entity must weight the entity-field match like the entity path \
             (agent={}, entity={})",
            agent_hits[0].score,
            entity_hits[0].score
        );
    }
}
