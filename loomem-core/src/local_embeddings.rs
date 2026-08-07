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
    use tokenizers::{Tokenizer, TruncationParams};
    use tracing::{debug, info};
    use tract_onnx::prelude::*;

    type Model = SimplePlan<TypedFact, Box<dyn TypedOp>, Graph<TypedFact, Box<dyn TypedOp>>>;

    /// Sentence transformers in the BERT / XLM-R family — including the default
    /// multilingual-e5-small — bake 512 learned position embeddings into the
    /// ONNX graph, so a longer sequence fails inside `model.run()` with
    /// `Invalid range 0..N for slicing 1,512`. Used only when the checkpoint's
    /// `tokenizer.json` declares no truncation of its own.
    const DEFAULT_MAX_SEQUENCE_TOKENS: usize = 512;

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

            let mut tokenizer = Tokenizer::from_file(&tokenizer_path)
                .map_err(|e| anyhow::anyhow!("Failed to load tokenizer: {}", e))?;

            // Cycle /015: cap the sequence at the model's position window so a
            // long chunk gets its head embedded instead of failing. Doing it on
            // the tokenizer (rather than slicing tensors afterwards) keeps the
            // post-processor's special tokens correct — max_length counts them.
            if tokenizer.get_truncation().is_none() {
                tokenizer
                    .with_truncation(Some(TruncationParams {
                        max_length: DEFAULT_MAX_SEQUENCE_TOKENS,
                        ..Default::default()
                    }))
                    .map_err(|e| anyhow::anyhow!("Failed to set tokenizer truncation: {}", e))?;
            }

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

        /// Embed multiple texts, one `Result` per input, in the same order.
        ///
        /// Unlike [`Self::embed_batch`], a single failing text does not discard
        /// the whole batch — the caller decides what to do per element. This is
        /// what the embedding queue uses: cycle /015 traced the "buried entity"
        /// incident to one oversized chunk aborting a 50-chunk flush.
        /// Failures are returned, not logged — the caller owns the chunk id and
        /// can say *which* chunk failed.
        pub fn embed_batch_lenient(&self, texts: &[String]) -> Vec<Result<Vec<f32>>> {
            texts.iter().map(|text| self.embed(text)).collect()
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
    pub fn embed_batch_lenient(&self, texts: &[String]) -> Vec<anyhow::Result<Vec<f32>>> {
        texts
            .iter()
            .map(|_| {
                Err(anyhow::anyhow!(
                    "Local embeddings require local-embeddings feature"
                ))
            })
            .collect()
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

    fn load_test_embedder() -> Option<LocalEmbedder> {
        let Ok(model_dir) = std::env::var("LOOMEM_TEST_EMBED_MODEL") else {
            eprintln!("skip: set LOOMEM_TEST_EMBED_MODEL=<dir> to run this test");
            return None;
        };
        Some(
            LocalEmbedder::load(std::path::Path::new(&model_dir), 384)
                .expect("load local embedding model"),
        )
    }

    /// Cycle /015: input longer than the model's 512-position window must be
    /// truncated, not rejected. Before the fix this returned
    /// `Err(Embedding inference failed: Invalid range 0..N for slicing 1,512)`.
    #[test]
    #[ignore = "requires LOOMEM_TEST_EMBED_MODEL"]
    fn embed_truncates_oversized_input() {
        let Some(embedder) = load_test_embedder() else {
            return;
        };

        let long = "Ala ma kota i bardzo lubi spacery po lesie. ".repeat(240);
        assert!(
            long.len() > 10_000,
            "fixture must exceed the 512-token window"
        );

        let vector = embedder.embed(&long).expect("oversized input must embed");
        assert_eq!(vector.len(), 384, "truncated input must keep the model dim");
        let norm: f32 = vector.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-3, "vector must stay L2-normalized");
    }

    /// Cycle /015 regression for the "buried entity" incident: one oversized
    /// chunk used to abort `embed_batch` at the first `?`, so the whole flush
    /// batch (up to 50 chunks) was dropped without any embedding. The lenient
    /// path must return one result per input.
    #[test]
    #[ignore = "requires LOOMEM_TEST_EMBED_MODEL"]
    fn lenient_batch_survives_oversized_member() {
        let Some(embedder) = load_test_embedder() else {
            return;
        };

        let oversized = "Szczegółowy raport z zebrania zarządu w sprawie budżetu. ".repeat(90);
        assert!(oversized.len() > 5_000, "fixture must be ~5000+ chars");
        let texts: Vec<String> = vec![
            "Pierwszy krótki fakt o projekcie.".to_string(),
            "Drugi krótki fakt o projekcie.".to_string(),
            oversized,
            "Czwarty krótki fakt o projekcie.".to_string(),
            "Piąty krótki fakt o projekcie.".to_string(),
        ];

        let results = embedder.embed_batch_lenient(&texts);
        assert_eq!(results.len(), texts.len(), "one result per input");
        let ok = results.iter().filter(|r| r.is_ok()).count();
        assert_eq!(ok, 5, "all five chunks must embed, not zero");
        for result in &results {
            let vector = result.as_ref().expect("every chunk embeds");
            assert_eq!(vector.len(), 384);
        }
    }
}
