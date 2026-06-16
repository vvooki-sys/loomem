//! Local sentence embeddings via tract (pure Rust ONNX inference).
//!
//! Loads a sentence transformer model (e.g. bge-small-en-v1.5) and generates
//! embeddings locally without API calls.
//!
//! Requires the `local-embeddings` feature flag (shares tract-onnx + tokenizers deps).

#[cfg(feature = "local-embeddings")]
mod inner {
    use anyhow::{Context, Result};
    use std::path::Path;
    use tokenizers::Tokenizer;
    use tracing::{debug, info};
    use tract_onnx::prelude::*;

    type Model = SimplePlan<TypedFact, Box<dyn TypedOp>, Graph<TypedFact, Box<dyn TypedOp>>>;

    pub struct LocalEmbedder {
        model: Model,
        tokenizer: Tokenizer,
        dim: usize,
    }

    impl LocalEmbedder {
        /// Load model and tokenizer from directory (model.onnx + tokenizer.json).
        pub fn load(model_dir: &Path, expected_dim: usize) -> Result<Self> {
            let model_path = model_dir.join("model.onnx");
            let tokenizer_path = model_dir.join("tokenizer.json");

            if !model_path.exists() {
                anyhow::bail!("Embedding model not found: {}", model_path.display());
            }
            if !tokenizer_path.exists() {
                anyhow::bail!("Tokenizer not found: {}", tokenizer_path.display());
            }

            info!("Loading local embedding model from {}", model_dir.display());

            let model = tract_onnx::onnx()
                .model_for_path(&model_path)
                .context("Failed to load ONNX embedding model")?
                .into_optimized()
                .context("Failed to optimize embedding model")?
                .into_runnable()
                .context("Failed to create runnable embedding model")?;

            let tokenizer = Tokenizer::from_file(&tokenizer_path)
                .map_err(|e| anyhow::anyhow!("Failed to load tokenizer: {}", e))?;

            info!("Local embedding model loaded (dim={})", expected_dim);
            Ok(Self {
                model,
                tokenizer,
                dim: expected_dim,
            })
        }

        /// Embed a single text. Returns normalized vector.
        pub fn embed(&self, text: &str) -> Result<Vec<f32>> {
            let encoding = self
                .tokenizer
                .encode(text, true)
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
                tract_ndarray::Array2::from_shape_vec((1, seq_len), attention_mask.clone())?.into();
            let types_tensor: Tensor =
                tract_ndarray::Array2::from_shape_vec((1, seq_len), token_type_ids)?.into();

            let outputs = self
                .model
                .run(tvec![
                    ids_tensor.into(),
                    mask_tensor.into(),
                    types_tensor.into(),
                ])
                .context("Embedding inference failed")?;

            // Output: [1, seq_len, dim] — need mean pooling
            let output = outputs[0]
                .to_array_view::<f32>()
                .context("Failed to extract output tensor")?;

            // Mean pooling with attention mask
            let output_shape = output.shape();
            let actual_dim = if output_shape.len() == 3 {
                output_shape[2]
            } else if output_shape.len() == 2 {
                output_shape[1]
            } else {
                anyhow::bail!("Unexpected output shape: {:?}", output_shape);
            };

            let mut pooled = vec![0.0_f32; actual_dim];

            if output_shape.len() == 3 {
                // [1, seq_len, dim] — mean pool over seq_len with attention mask
                let mut mask_sum = 0.0_f32;
                for t in 0..seq_len {
                    let mask_val = attention_mask[t] as f32;
                    mask_sum += mask_val;
                    for d in 0..actual_dim {
                        pooled[d] += output[[0, t, d]] * mask_val;
                    }
                }
                if mask_sum > 0.0 {
                    for val in pooled.iter_mut().take(actual_dim) {
                        *val /= mask_sum;
                    }
                }
            } else {
                // [1, dim] — CLS token output, no pooling needed
                for d in 0..actual_dim {
                    pooled[d] = output[[0, d]];
                }
            }

            // L2 normalize
            let norm: f32 = pooled.iter().map(|x| x * x).sum::<f32>().sqrt();
            if norm > 0.0 {
                for x in &mut pooled {
                    *x /= norm;
                }
            }

            debug!(
                "Local embed: {} tokens → {}-dim vector",
                seq_len,
                pooled.len()
            );
            Ok(pooled)
        }

        /// Embed multiple texts. Returns vectors in same order.
        pub fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
            // tract doesn't support dynamic batching easily, so process sequentially
            let mut results = Vec::with_capacity(texts.len());
            for text in texts {
                results.push(self.embed(text)?);
            }
            Ok(results)
        }

        pub fn dim(&self) -> usize {
            self.dim
        }
    }
}

#[cfg(feature = "local-embeddings")]
pub use inner::LocalEmbedder;

/// Stub when feature disabled
#[cfg(not(feature = "local-embeddings"))]
pub struct LocalEmbedder;

#[cfg(not(feature = "local-embeddings"))]
impl LocalEmbedder {
    pub fn embed(&self, _text: &str) -> anyhow::Result<Vec<f32>> {
        anyhow::bail!("Local embeddings require local-embeddings feature")
    }
    pub fn embed_batch(&self, _texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
        anyhow::bail!("Local embeddings require local-embeddings feature")
    }
    pub fn dim(&self) -> usize {
        0
    }
}

/// Try to load local embedder. Returns None if model dir missing or feature disabled.
// Args used only in #[cfg(feature = "local-embeddings")] branch below.
// needless_return required by cfg-branch control flow (each cfg-block
// must return independently; clippy can't see across cfg boundaries).
#[cfg_attr(
    not(feature = "local-embeddings"),
    allow(unused_variables, clippy::needless_return)
)]
pub fn try_load(model_dir: &str, expected_dim: usize) -> Option<LocalEmbedder> {
    #[cfg(not(feature = "local-embeddings"))]
    {
        tracing::warn!("Local embeddings configured but local-embeddings feature not enabled");
        return None;
    }

    #[cfg(feature = "local-embeddings")]
    {
        let path = std::path::Path::new(model_dir);
        if !path.exists() {
            tracing::info!(
                "Local embedding model dir not found ({}), skipping",
                model_dir
            );
            return None;
        }
        match LocalEmbedder::load(path, expected_dim) {
            Ok(embedder) => Some(embedder),
            Err(e) => {
                tracing::warn!("Failed to load local embedding model: {}", e);
                None
            }
        }
    }
}

#[cfg(all(test, feature = "local-embeddings"))]
mod tests {
    use super::*;

    // Embeddings are L2-normalized, so cosine similarity == dot product.
    fn cosine(a: &[f32], b: &[f32]) -> f32 {
        a.iter().zip(b).map(|(x, y)| x * y).sum()
    }

    /// Cycle /010 multilingual gate: the default local model must place a
    /// Polish paraphrase closer to the query than an unrelated Polish
    /// sentence. Skipped unless `LOOMEM_TEST_EMBED_MODEL` points at a model
    /// directory (`model.onnx` + `tokenizer.json`), so CI without the model
    /// artifact still passes.
    #[test]
    fn polish_semantic_similarity() {
        let Ok(model_dir) = std::env::var("LOOMEM_TEST_EMBED_MODEL") else {
            eprintln!("skip: set LOOMEM_TEST_EMBED_MODEL=<dir> to run the multilingual gate");
            return;
        };
        let embedder = LocalEmbedder::load(std::path::Path::new(&model_dir), 384)
            .expect("load local embedding model");

        let query = embedder
            .embed("Gdzie zostawiłem kluczyki do samochodu?")
            .unwrap();
        let paraphrase = embedder.embed("Nie mogę znaleźć kluczy od auta.").unwrap();
        let unrelated = embedder
            .embed("Przepis na ciasto drożdżowe z owocami sezonowymi.")
            .unwrap();

        assert_eq!(query.len(), 384, "expected 384-dim vectors");
        let s_para = cosine(&query, &paraphrase);
        let s_unrel = cosine(&query, &unrelated);
        assert!(
            s_para > s_unrel + 0.05,
            "Polish paraphrase ({s_para:.3}) should clearly beat unrelated ({s_unrel:.3})"
        );
    }
}
