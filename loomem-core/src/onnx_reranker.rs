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
    use tokenizers::Tokenizer;
    use tracing::{debug, info};
    use tract_onnx::prelude::*;

    type Model = SimplePlan<TypedFact, Box<dyn TypedOp>, Graph<TypedFact, Box<dyn TypedOp>>>;

    pub struct OnnxReranker {
        model: Model,
        tokenizer: Tokenizer,
    }

    impl OnnxReranker {
        /// Load model and tokenizer from a directory containing:
        /// - model.onnx (the cross-encoder)
        /// - tokenizer.json (HuggingFace tokenizer)
        pub fn load(model_dir: &Path) -> Result<Self> {
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

            let tokenizer = Tokenizer::from_file(&tokenizer_path)
                .map_err(|e| anyhow::anyhow!("Failed to load tokenizer: {}", e))?;

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
                .to_array_view::<f32>()
                .context("Failed to extract output tensor")?;

            let score = output.iter().next().copied().unwrap_or(0.0);
            Ok(score)
        }
    }
}

#[cfg(feature = "onnx-rerank")]
pub use inner::OnnxReranker;

/// Stub type when feature is disabled
#[cfg(not(feature = "onnx-rerank"))]
pub struct OnnxReranker;

/// Try to load the reranker. Returns None if model dir doesn't exist
/// or feature is disabled.
// Args used only in #[cfg(feature = "onnx-rerank")] branch below.
// needless_return required by cfg-branch control flow (each cfg-block
// must return independently; clippy can't see across cfg boundaries).
#[cfg_attr(
    not(feature = "onnx-rerank"),
    allow(unused_variables, clippy::needless_return)
)]
pub fn try_load(model_dir: Option<&str>) -> Option<OnnxReranker> {
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
        match OnnxReranker::load(path) {
            Ok(reranker) => Some(reranker),
            Err(e) => {
                tracing::warn!("Failed to load reranker: {}", e);
                None
            }
        }
    }
}

#[cfg(all(test, feature = "onnx-rerank"))]
mod tests {
    use super::*;
    use std::path::Path;

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

        let reranker = OnnxReranker::load(&model_dir).expect("Failed to load reranker");

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
