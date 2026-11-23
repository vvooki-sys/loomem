//! Local cross-encoder reranker using tract (pure Rust ONNX inference).
//!
//! Loads a cross-encoder model (e.g. ms-marco-MiniLM-L-6-v2) and scores
//! (query, document) pairs locally without API calls or native dependencies.
//!
//! Requires the `onnx-rerank` feature flag.

#[cfg(feature = "onnx-rerank")]
mod inner {
    use anyhow::{Context, Result};
    use std::path::Path;
    use tokenizers::{Tokenizer, TruncationParams};
    use tracing::{debug, info};
    use tract_onnx::prelude::*;

    type Model = std::sync::Arc<TypedRunnableModel>;

    pub struct OnnxReranker {
        model: Model,
        tokenizer: Tokenizer,
    }

    impl OnnxReranker {
        /// Load model and tokenizer from a directory containing:
        /// - model.onnx (the cross-encoder)
        /// - tokenizer.json (HuggingFace tokenizer)
        ///
        /// `max_tokens` (`[search] rerank_onnx_max_tokens`): `Some(n)` caps
        /// each (query, document) pair at `n` tokens when `tokenizer.json`
        /// declares no truncation of its own; `None` or `Some(0)` leaves the
        /// tokenizer as shipped (pre-flag behaviour).
        pub fn load(model_dir: &Path, max_tokens: Option<usize>) -> Result<Self> {
            let model_path = model_dir.join("model.onnx");
            let tokenizer_path = model_dir.join("tokenizer.json");

            if !model_path.exists() {
                anyhow::bail!("ONNX model not found: {}", model_path.display());
            }
            if !tokenizer_path.exists() {
                anyhow::bail!("Tokenizer not found: {}", tokenizer_path.display());
            }

            info!("Loading tract reranker from {}", model_dir.display());

            let model = tract_onnx::onnx()
                .model_for_path(&model_path)
                .context("Failed to load ONNX model")?
                .into_optimized()
                .context("Failed to optimize model")?
                .into_runnable()
                .context("Failed to create runnable model")?;

            let mut tokenizer = Tokenizer::from_file(&tokenizer_path)
                .map_err(|e| anyhow::anyhow!("Failed to load tokenizer: {}", e))?;
            apply_truncation(&mut tokenizer, max_tokens)?;

            info!("Tract reranker loaded successfully");
            Ok(Self { model, tokenizer })
        }

        /// Rerank: score all (query, doc) pairs and return indices sorted by score (best first).
        pub fn rerank(
            &self,
            query: &str,
            documents: &[String],
            top_k: usize,
        ) -> Result<Vec<usize>> {
            if documents.is_empty() {
                return Ok(vec![]);
            }

            let mut scored: Vec<(usize, f32)> = Vec::with_capacity(documents.len());

            for (i, doc) in documents.iter().enumerate() {
                let score = self.score_pair(query, doc)?;
                scored.push((i, score));
            }

            scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

            let indices: Vec<usize> = scored.into_iter().take(top_k).map(|(i, _)| i).collect();

            debug!(
                "Tract rerank: {} candidates → top {}",
                documents.len(),
                indices.len()
            );
            Ok(indices)
        }

        /// Token count of the encoded (query, document) pair, special
        /// tokens included — what `score_pair` feeds the model.
        #[cfg(test)]
        pub(super) fn pair_len(&self, query: &str, document: &str) -> Result<usize> {
            let encoding = self
                .tokenizer
                .encode((query, document), true)
                .map_err(|e| anyhow::anyhow!("Tokenization failed: {}", e))?;
            Ok(encoding.get_ids().len())
        }

        fn score_pair(&self, query: &str, document: &str) -> Result<f32> {
            let encoding = self
                .tokenizer
                .encode((query, document), true)
                .map_err(|e| anyhow::anyhow!("Tokenization failed: {}", e))?;

            let input_ids: Vec<i64> = encoding.get_ids().iter().map(|&id| id as i64).collect();
            let attention_mask: Vec<i64> = encoding
                .get_attention_mask()
                .iter()
                .map(|&m| m as i64)
                .collect();
            let token_type_ids: Vec<i64> =
                encoding.get_type_ids().iter().map(|&t| t as i64).collect();

            let seq_len = input_ids.len();

            let ids_tensor: Tensor =
                tract_ndarray::Array2::from_shape_vec((1, seq_len), input_ids)?.into();

            let mask_tensor: Tensor =
                tract_ndarray::Array2::from_shape_vec((1, seq_len), attention_mask)?.into();

            let types_tensor: Tensor =
                tract_ndarray::Array2::from_shape_vec((1, seq_len), token_type_ids)?.into();

            let outputs = self
                .model
                .run(tvec![
                    ids_tensor.into(),
                    mask_tensor.into(),
                    types_tensor.into(),
                ])
                .context("Tract inference failed")?;

            // Output shape: [1, 1] for cross-encoder relevance score
            let output = outputs[0]
                .to_plain_array_view::<f32>()
                .context("Failed to extract output tensor")?;

            let score = output.iter().next().copied().unwrap_or(0.0);
            Ok(score)
        }
    }

    /// Cap (query, document) pairs at `max_tokens` with the tokenizer's
    /// default strategy (longest-first, so the document is cut before the
    /// query). Doing it on the tokenizer keeps the special tokens correct —
    /// `max_length` counts them. A tokenizer that already truncates, and
    /// `None` / `Some(0)`, are left untouched.
    pub(super) fn apply_truncation(
        tokenizer: &mut Tokenizer,
        max_tokens: Option<usize>,
    ) -> Result<()> {
        let Some(max_length) = max_tokens.filter(|&n| n > 0) else {
            return Ok(());
        };
        if tokenizer.get_truncation().is_none() {
            tokenizer
                .with_truncation(Some(TruncationParams {
                    max_length,
                    ..Default::default()
                }))
                .map_err(|e| anyhow::anyhow!("Failed to set reranker truncation: {}", e))?;
            info!(
                "Reranker tokenizer truncates pairs at {} tokens",
                max_length
            );
        }
        Ok(())
    }
}

#[cfg(feature = "onnx-rerank")]
pub use inner::OnnxReranker;

/// `[search] rerank_onnx_max_tokens` -> the cap handed to the loader:
/// `0` means no truncation.
pub fn truncation_limit(max_tokens: usize) -> Option<usize> {
    (max_tokens > 0).then_some(max_tokens)
}

/// Stub type when feature is disabled
#[cfg(not(feature = "onnx-rerank"))]
pub struct OnnxReranker;

/// Try to load the reranker. Returns None if model dir doesn't exist
/// or feature is disabled. `max_tokens` is `[search] rerank_onnx_max_tokens`
/// (0 = no truncation).
// Args used only in #[cfg(feature = "onnx-rerank")] branch below.
// needless_return required by cfg-branch control flow (each cfg-block
// must return independently; clippy can't see across cfg boundaries).
#[cfg_attr(
    not(feature = "onnx-rerank"),
    allow(unused_variables, clippy::needless_return)
)]
pub fn try_load(model_dir: Option<&str>, max_tokens: usize) -> Option<OnnxReranker> {
    #[cfg(not(feature = "onnx-rerank"))]
    {
        if model_dir.is_some() {
            tracing::warn!("ONNX reranker configured but onnx-rerank feature not enabled");
        }
        return None;
    }

    #[cfg(feature = "onnx-rerank")]
    {
        let dir = model_dir?;
        let path = std::path::Path::new(dir);
        if !path.exists() {
            tracing::info!("Reranker model dir not found ({}), skipping", dir);
            return None;
        }
        match OnnxReranker::load(path, truncation_limit(max_tokens)) {
            Ok(reranker) => Some(reranker),
            Err(e) => {
                tracing::warn!("Failed to load reranker: {}", e);
                None
            }
        }
    }
}

#[cfg(test)]
mod limit_tests {
    use super::truncation_limit;

    #[test]
    fn zero_means_no_truncation() {
        assert_eq!(truncation_limit(0), None);
        assert_eq!(truncation_limit(512), Some(512));
    }
}

#[cfg(all(test, feature = "onnx-rerank"))]
mod tests {
    use super::*;
    use std::path::Path;
    use std::str::FromStr;
    use tokenizers::Tokenizer;

    /// A whitespace word-level tokenizer with no post-processor, so a pair
    /// encodes to exactly `words(query) + words(document)` tokens.
    fn word_tokenizer(truncation: &str) -> Tokenizer {
        let json = format!(
            r#"{{"version":"1.0","truncation":{truncation},"padding":null,
            "added_tokens":[],"normalizer":null,
            "pre_tokenizer":{{"type":"WhitespaceSplit"}},"post_processor":null,
            "decoder":null,"model":{{"type":"WordLevel",
            "vocab":{{"[UNK]":0,"q":1,"d":2}},"unk_token":"[UNK]"}}}}"#
        );
        Tokenizer::from_str(&json).expect("build word tokenizer")
    }

    fn pair_ids(tokenizer: &Tokenizer, query: &str, document: &str) -> Vec<u32> {
        tokenizer
            .encode((query, document), true)
            .expect("encode pair")
            .get_ids()
            .to_vec()
    }

    #[test]
    fn no_cap_leaves_the_tokenizer_untouched() {
        let document = "d ".repeat(40);
        for cap in [None, Some(0)] {
            let mut tokenizer = word_tokenizer("null");
            inner::apply_truncation(&mut tokenizer, cap).expect("apply");
            assert!(tokenizer.get_truncation().is_none(), "{cap:?}");
            assert_eq!(pair_ids(&tokenizer, "q q", &document).len(), 42);
        }
    }

    #[test]
    fn cap_truncates_the_document_before_the_query() {
        let mut tokenizer = word_tokenizer("null");
        inner::apply_truncation(&mut tokenizer, Some(8)).expect("apply");
        let ids = pair_ids(&tokenizer, "q q", &"d ".repeat(40));
        // Longest-first: the 40-token document shrinks, the query survives.
        assert_eq!(ids, vec![1, 1, 2, 2, 2, 2, 2, 2]);
    }

    #[test]
    fn tokenizer_with_its_own_truncation_is_not_overridden() {
        let own = r#"{"direction":"Right","max_length":4,"strategy":"LongestFirst","stride":0}"#;
        let mut tokenizer = word_tokenizer(own);
        inner::apply_truncation(&mut tokenizer, Some(8)).expect("apply");
        assert_eq!(tokenizer.get_truncation().map(|t| t.max_length), Some(4));
        assert_eq!(pair_ids(&tokenizer, "q q", &"d ".repeat(40)).len(), 4);
    }

    #[test]
    #[ignore = "requires LOOMEM_TEST_RERANK_MODEL pointing to the ms-marco cross-encoder"]
    fn real_model_truncates_long_pairs_only_when_capped() {
        let model_dir = std::env::var("LOOMEM_TEST_RERANK_MODEL")
            .expect("set LOOMEM_TEST_RERANK_MODEL to the cross-encoder model directory");
        let query = "What is the capital of France?";
        let long_doc = "Paris is the capital of France. ".repeat(200);

        let uncapped = OnnxReranker::load(Path::new(&model_dir), None).expect("load");
        assert!(uncapped.pair_len(query, &long_doc).expect("encode") > 512);

        let capped = OnnxReranker::load(Path::new(&model_dir), Some(512)).expect("load");
        assert_eq!(capped.pair_len(query, &long_doc).expect("encode"), 512);
        let short_doc = "Paris is the capital of France.";
        assert_eq!(
            capped.pair_len(query, short_doc).expect("encode"),
            uncapped.pair_len(query, short_doc).expect("encode"),
            "short pairs are unchanged"
        );
        let docs = vec![long_doc, "Chocolate cake is a dessert.".to_string()];
        assert!(
            uncapped.rerank(query, &docs, 2).is_err(),
            "over-length pair fails without a cap (pre-flag behaviour)"
        );
        assert_eq!(
            capped.rerank(query, &docs, 2).expect("capped rerank"),
            [0, 1]
        );
    }

    #[test]
    #[ignore = "requires LOOMEM_TEST_RERANK_MODEL pointing to the ms-marco cross-encoder"]
    fn real_model_ranks_relevant_document_first() {
        let model_dir = std::env::var("LOOMEM_TEST_RERANK_MODEL")
            .expect("set LOOMEM_TEST_RERANK_MODEL to the cross-encoder model directory");
        let reranker = OnnxReranker::load(Path::new(&model_dir), None).expect("load reranker");
        let documents = vec![
            "Berlin is the capital of Germany.".to_string(),
            "Paris is the capital of France.".to_string(),
            "Chocolate cake is a dessert.".to_string(),
        ];
        let ranking = reranker
            .rerank(
                "What is the capital of France?",
                &documents,
                documents.len(),
            )
            .expect("rerank documents");
        assert_eq!(ranking, vec![1, 0, 2]);
    }

    #[test]
    #[ignore] // requires model files in models/reranker/
    fn benchmark_rerank_20_candidates() {
        let model_dir = if Path::new("models/reranker/model.onnx").exists() {
            Path::new("models/reranker").to_path_buf()
        } else if Path::new("../models/reranker/model.onnx").exists() {
            Path::new("../models/reranker").to_path_buf()
        } else {
            println!("SKIPPED: model.onnx not found. Place in models/reranker/");
            return;
        };

        let reranker = OnnxReranker::load(&model_dir, None).expect("Failed to load reranker");

        let query = "What is the capital of France?";
        let docs: Vec<String> = (0..20).map(|i| {
            format!("Document {} discusses various topics including geography and history. Paris is mentioned in some contexts.", i)
        }).collect();

        // Warmup
        let _ = reranker.rerank(query, &docs[..2], 2);

        // Benchmark
        let start = std::time::Instant::now();
        let indices = reranker.rerank(query, &docs, 10).expect("Rerank failed");
        let elapsed = start.elapsed();

        println!("Rerank 20 candidates -> top 10: {:?}", elapsed);
        println!("Per-candidate: {:?}", elapsed / 20);
        println!("Top indices: {:?}", indices);

        assert!(!indices.is_empty(), "Should return results");
        assert!(indices.len() <= 10, "Should return at most top_k");
        assert!(
            elapsed.as_millis() < 5000,
            "Rerank should complete within 5s: {:?}",
            elapsed
        );
    }
}
