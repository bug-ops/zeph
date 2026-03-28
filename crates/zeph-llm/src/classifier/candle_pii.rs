// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Candle-backed DeBERTa-v2 NER classifier for PII detection.
//!
//! Uses `iiiorg/piiranha-v1-detect-personal-information` (or any compatible NER model)
//! from `HuggingFace` Hub. Returns per-span PII results via [`PiiDetector`].
//!
//! Inference runs in `tokio::task::spawn_blocking`. Model is loaded lazily on first call.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::debertav2::{Config as DebertaConfig, DebertaV2NERModel};
use tokenizers::Tokenizer;

use crate::error::LlmError;

use super::{PiiDetector, PiiResult, PiiSpan, verify_sha256};

/// Maximum number of tokens per chunk for NER inference.
const MAX_CHUNK_TOKENS: usize = 448;
/// Token overlap between adjacent chunks for NER.
const CHUNK_OVERLAP_TOKENS: usize = 64;

struct CandlePiiInner {
    model: DebertaV2NERModel,
    tokenizer: Tokenizer,
    device: Device,
    /// Index → BIO label string (e.g. `0 → "O"`, `1 → "B-GIVENNAME"`).
    id2label: Vec<String>,
}

/// `CandlePiiClassifier` wraps a DeBERTa-v2 NER model for token-level PII detection.
///
/// Model weights are loaded lazily on first call via `OnceLock`.
/// Instances are cheaply cloneable (`Arc` inside).
#[derive(Clone)]
pub struct CandlePiiClassifier {
    repo_id: Arc<str>,
    threshold: f32,
    expected_sha256: Option<Arc<str>>,
    hf_token: Option<Arc<str>>,
    inner: Arc<OnceLock<Result<Arc<CandlePiiInner>, String>>>,
}

impl std::fmt::Debug for CandlePiiClassifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CandlePiiClassifier")
            .field("repo_id", &self.repo_id)
            .field("threshold", &self.threshold)
            .finish_non_exhaustive()
    }
}

impl CandlePiiClassifier {
    /// Create a new PII classifier. Model loads lazily on first call.
    #[must_use]
    pub fn new(repo_id: impl Into<Arc<str>>, threshold: f32) -> Self {
        Self {
            repo_id: repo_id.into(),
            threshold,
            expected_sha256: None,
            hf_token: None,
            inner: Arc::new(OnceLock::new()),
        }
    }

    /// Attach an expected SHA-256 hex digest for model verification.
    #[must_use]
    pub fn with_sha256(mut self, hash: impl Into<Arc<str>>) -> Self {
        self.expected_sha256 = Some(hash.into());
        self
    }

    /// Attach a resolved `HuggingFace` Hub API token for authenticated model downloads.
    #[must_use]
    pub fn with_hf_token(mut self, token: impl Into<Arc<str>>) -> Self {
        self.hf_token = Some(token.into());
        self
    }

    #[allow(unsafe_code)]
    fn load_inner(
        repo_id: &str,
        expected_sha256: Option<&str>,
        hf_token: Option<&str>,
    ) -> Result<CandlePiiInner, LlmError> {
        tracing::info!(repo_id, "loading PII classifier model (first inference)…");
        let load_t0 = std::time::Instant::now();
        let api = hf_hub::api::sync::ApiBuilder::new()
            .with_token(hf_token.map(str::to_owned))
            .build()
            .map_err(|e| {
                LlmError::ModelLoad(format!("failed to create HuggingFace API client: {e}"))
            })?;
        let repo = api.model(repo_id.to_owned());

        let config_path = repo.get("config.json").map_err(|e| {
            LlmError::ModelLoad(format!(
                "failed to download config.json from {repo_id}: {e}"
            ))
        })?;
        let tokenizer_path = repo.get("tokenizer.json").map_err(|e| {
            LlmError::ModelLoad(format!(
                "failed to download tokenizer.json from {repo_id}: {e}"
            ))
        })?;
        let weights_path = repo.get("model.safetensors").map_err(|e| {
            LlmError::ModelLoad(format!(
                "failed to download model.safetensors from {repo_id}: {e}"
            ))
        })?;

        if let Some(expected) = expected_sha256 {
            verify_sha256(&weights_path, expected)?;
        }

        let config_str = std::fs::read_to_string(&config_path)
            .map_err(|e| LlmError::ModelLoad(format!("failed to read DeBERTa config: {e}")))?;
        let config: DebertaConfig = serde_json::from_str(&config_str)?;

        let id2label: Vec<String> = config.id2label.as_ref().map_or_else(
            || vec!["O".into()],
            |m| {
                let mut sorted: Vec<(u32, String)> =
                    m.iter().map(|(k, v)| (*k, v.clone())).collect();
                sorted.sort_by_key(|(k, _)| *k);
                sorted.into_iter().map(|(_, v)| v).collect()
            },
        );

        let tokenizer = Tokenizer::from_file(&tokenizer_path)
            .map_err(|e| LlmError::ModelLoad(format!("failed to load tokenizer: {e}")))?;

        super::candle::CandleClassifier::validate_safetensors_path(&weights_path)?;

        let device = Device::Cpu;
        // SAFETY: validated safetensors header above; file not modified during VarBuilder lifetime
        let vb =
            unsafe { VarBuilder::from_mmaped_safetensors(&[weights_path], DType::F32, &device)? };

        let model = DebertaV2NERModel::load(vb, &config, config.id2label.clone())
            .map_err(|e| LlmError::ModelLoad(format!("failed to load DeBERTa NER model: {e}")))?;

        let load_ms = load_t0.elapsed().as_millis();
        tracing::info!(repo_id, load_ms, "PII classifier model loaded");
        Ok(CandlePiiInner {
            model,
            tokenizer,
            device,
            id2label,
        })
    }

    /// Run NER inference on a single token chunk.
    ///
    /// Returns per-token `(label_idx, score)` pairs for non-special tokens.
    /// `special_token_mask`: 1 for special tokens (`[CLS]`, `[SEP]`, `[PAD]`), 0 for real tokens.
    fn run_chunk_ner(
        inner: &CandlePiiInner,
        input_ids: &[u32],
    ) -> Result<Vec<(usize, f32)>, LlmError> {
        let seq_len = input_ids.len();
        let ids_tensor = Tensor::new(input_ids, &inner.device)?.unsqueeze(0)?;
        let token_type_ids = Tensor::zeros((1, seq_len), DType::I64, &inner.device)?;
        let attention_mask = Tensor::ones((1, seq_len), DType::I64, &inner.device)?;

        // forward returns [batch=1, seq_len, num_labels]
        let logits =
            inner
                .model
                .forward(&ids_tensor, Some(token_type_ids), Some(attention_mask))?;

        // Remove batch dim → [seq_len, num_labels]
        let logits_2d = logits.squeeze(0)?;
        let num_tokens = logits_2d.dim(0)?;
        let num_labels = logits_2d.dim(1)?;

        let mut result = Vec::with_capacity(num_tokens);
        for i in 0..num_tokens {
            let token_logits = logits_2d.get(i)?;
            let probs = candle_nn::ops::softmax(&token_logits, 0)?;
            let probs_vec = probs.to_vec1::<f32>().map_err(LlmError::Candle)?;

            let (best_idx, best_score) = probs_vec
                .iter()
                .enumerate()
                .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
                .map_or((0_usize, 0.0_f32), |(i, &s)| (i, s));

            let _ = num_labels; // used implicitly via probs_vec length
            result.push((best_idx, best_score));
        }
        Ok(result)
    }

    /// Main NER pipeline: tokenize → chunked inference with max-confidence merge → BIO decoding.
    fn detect_sync(
        inner: &CandlePiiInner,
        text: &str,
        threshold: f32,
    ) -> Result<PiiResult, LlmError> {
        let encoding = inner
            .tokenizer
            .encode(text, true)
            .map_err(|e| LlmError::Inference(format!("tokenizer encode failed: {e}")))?;

        let ids = encoding.get_ids();
        let offsets = encoding.get_offsets();
        let special_mask = encoding.get_special_tokens_mask();

        let total_len = ids.len();
        if total_len == 0 {
            return Ok(PiiResult {
                spans: vec![],
                has_pii: false,
            });
        }

        // Global predictions array: (label_idx, score). Initialised to (O=0, 0.0).
        // max-confidence merge: for tokens covered by multiple chunks, keep the higher score.
        let mut predictions: Vec<(usize, f32)> = vec![(0, 0.0); total_len];

        let mut chunk_start = 0usize;
        loop {
            let chunk_end = (chunk_start + MAX_CHUNK_TOKENS).min(total_len);
            let chunk_ids = &ids[chunk_start..chunk_end];
            let chunk_preds = Self::run_chunk_ner(inner, chunk_ids)?;

            for (local_pos, (label_idx, score)) in chunk_preds.into_iter().enumerate() {
                let global_pos = chunk_start + local_pos;
                if score > predictions[global_pos].1 {
                    predictions[global_pos] = (label_idx, score);
                }
            }

            if chunk_end == total_len {
                break;
            }
            chunk_start = chunk_end.saturating_sub(CHUNK_OVERLAP_TOKENS);
        }

        // BIO span extraction — special tokens filtered first.
        let spans = extract_bio_spans(
            &predictions,
            offsets,
            special_mask,
            &inner.id2label,
            threshold,
        );
        let has_pii = !spans.is_empty();
        Ok(PiiResult { spans, has_pii })
    }
}

/// Extract BIO spans from per-token predictions.
///
/// CRITICAL: filters out special tokens ([CLS], [SEP], [PAD]) via `special_mask`
/// before span extraction to avoid phantom PII spans at (0, 0) offsets.
fn extract_bio_spans(
    predictions: &[(usize, f32)],
    offsets: &[(usize, usize)],
    special_mask: &[u32],
    id2label: &[String],
    threshold: f32,
) -> Vec<PiiSpan> {
    let mut spans = Vec::new();
    // Current open span: (entity_type, start_byte, end_byte, min_score)
    let mut current: Option<(String, usize, usize, f32)> = None;

    for (i, &(label_idx, score)) in predictions.iter().enumerate() {
        // Skip special tokens ([CLS], [SEP], [PAD]).
        if i < special_mask.len() && special_mask[i] != 0 {
            // Close any open span before special token.
            if let Some((entity_type, start, end, span_score)) = current.take() {
                spans.push(PiiSpan {
                    entity_type,
                    start,
                    end,
                    score: span_score,
                });
            }
            continue;
        }

        let label = id2label.get(label_idx).map_or("O", String::as_str);
        let (tok_start, tok_end) = offsets.get(i).copied().unwrap_or((0, 0));

        // Treat low-confidence predictions as O.
        if score < threshold || label == "O" {
            if let Some((entity_type, start, end, span_score)) = current.take() {
                spans.push(PiiSpan {
                    entity_type,
                    start,
                    end,
                    score: span_score,
                });
            }
            continue;
        }

        if let Some(entity_type) = label.strip_prefix("B-") {
            // Close previous span, start new one.
            if let Some((et, start, end, span_score)) = current.take() {
                spans.push(PiiSpan {
                    entity_type: et,
                    start,
                    end,
                    score: span_score,
                });
            }
            current = Some((entity_type.to_owned(), tok_start, tok_end, score));
        } else if let Some(entity_type) = label.strip_prefix("I-") {
            // Continue span if entity type matches.
            if let Some((ref et, start, _, ref mut span_score)) = current {
                if et == entity_type {
                    // Extend end, keep min score across the span.
                    *span_score = span_score.min(score);
                    current = Some((entity_type.to_owned(), start, tok_end, *span_score));
                } else {
                    // Entity type mismatch — close previous, start new.
                    let (et, start, end, span_score) = current.take().unwrap();
                    spans.push(PiiSpan {
                        entity_type: et,
                        start,
                        end,
                        score: span_score,
                    });
                    current = Some((entity_type.to_owned(), tok_start, tok_end, score));
                }
            } else {
                // Orphan I- without B- — start span anyway.
                current = Some((entity_type.to_owned(), tok_start, tok_end, score));
            }
        } else {
            // Unknown label prefix — close span.
            if let Some((et, start, end, span_score)) = current.take() {
                spans.push(PiiSpan {
                    entity_type: et,
                    start,
                    end,
                    score: span_score,
                });
            }
        }
    }

    // Close any remaining open span.
    if let Some((entity_type, start, end, score)) = current {
        spans.push(PiiSpan {
            entity_type,
            start,
            end,
            score,
        });
    }

    spans
}

impl PiiDetector for CandlePiiClassifier {
    fn detect_pii<'a>(
        &'a self,
        text: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<PiiResult, LlmError>> + Send + 'a>> {
        let text = text.to_owned();
        let inner_lock = Arc::clone(&self.inner);
        let repo_id = Arc::clone(&self.repo_id);
        let threshold = self.threshold;
        let expected_sha256 = self.expected_sha256.clone();
        let hf_token = self.hf_token.clone();

        Box::pin(async move {
            let t0 = std::time::Instant::now();
            let result = tokio::task::spawn_blocking(move || {
                let loaded = inner_lock.get_or_init(|| {
                    CandlePiiClassifier::load_inner(
                        &repo_id,
                        expected_sha256.as_deref().map(|s| s as &str),
                        hf_token.as_deref().map(|s| s as &str),
                    )
                    .map(Arc::new)
                    .map_err(|e| e.to_string())
                });
                match loaded {
                    Ok(inner) => CandlePiiClassifier::detect_sync(inner, &text, threshold),
                    Err(e) => Err(LlmError::ModelLoad(e.clone())),
                }
            })
            .await
            .map_err(|e| LlmError::Inference(format!("PII classifier task panicked: {e}")))?;
            let latency_ms = t0.elapsed().as_millis();
            match &result {
                Ok(r) => tracing::debug!(
                    task = "pii",
                    latency_ms,
                    spans = r.spans.len(),
                    has_pii = r.has_pii,
                    "classifier inference complete"
                ),
                Err(e) => {
                    tracing::warn!(task = "pii", latency_ms, error = %e, "classifier inference failed");
                }
            }
            result
        })
    }

    fn backend_name(&self) -> &'static str {
        "candle-pii-deberta"
    }
}

/// Download PII classifier model weights to the `HuggingFace` Hub cache.
///
/// # Errors
///
/// Returns `LlmError::ModelLoad` if the download fails.
pub fn download_pii_model(
    repo_id: &str,
    hf_token: Option<&str>,
    timeout: Duration,
) -> Result<(), LlmError> {
    let (tx, rx) = std::sync::mpsc::channel();
    let repo_id_owned = repo_id.to_owned();
    let token_owned = hf_token.map(str::to_owned);

    std::thread::spawn(move || {
        let result = CandlePiiClassifier::load_inner(&repo_id_owned, None, token_owned.as_deref())
            .map(|_| ());
        let _ = tx.send(result);
    });

    rx.recv_timeout(timeout)
        .map_err(|_| LlmError::ModelLoad(format!("PII model download timed out for {repo_id}")))?
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── extract_bio_spans unit tests (no model required) ────────────────────

    fn make_id2label(labels: &[&str]) -> Vec<String> {
        labels
            .iter()
            .map(std::string::ToString::to_string)
            .collect()
    }

    #[test]
    fn bio_extraction_single_entity() {
        // Tokens: [CLS] John Smith [SEP]
        // special_mask: [1, 0, 0, 1]
        // predictions: CLS=O, John=B-GIVENNAME(0.9), Smith=I-GIVENNAME(0.85), SEP=O
        let id2label = make_id2label(&["O", "B-GIVENNAME", "I-GIVENNAME", "B-EMAIL"]);
        let predictions = vec![(0, 0.99), (1, 0.90), (2, 0.85), (0, 0.99)];
        let offsets = vec![(0, 0), (0, 4), (5, 10), (10, 10)];
        let special_mask = vec![1u32, 0, 0, 1];

        let spans = extract_bio_spans(&predictions, &offsets, &special_mask, &id2label, 0.75);

        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].entity_type, "GIVENNAME");
        assert_eq!(spans[0].start, 0);
        assert_eq!(spans[0].end, 10);
        // min score across span tokens
        assert!((spans[0].score - 0.85).abs() < 1e-5);
    }

    #[test]
    fn bio_extraction_special_tokens_filtered() {
        // Special tokens with noisy logits (label_idx=1 = B-GIVENNAME) must NOT produce spans.
        let id2label = make_id2label(&["O", "B-GIVENNAME", "I-GIVENNAME"]);
        // CLS has label 1 (B-GIVENNAME, score 0.9) — must be filtered via special_mask
        let predictions = vec![(1, 0.9), (0, 0.99)];
        let offsets = vec![(0, 0), (0, 4)];
        let special_mask = vec![1u32, 0]; // CLS is special

        let spans = extract_bio_spans(&predictions, &offsets, &special_mask, &id2label, 0.75);
        assert!(spans.is_empty(), "CLS token must not produce PII span");
    }

    #[test]
    fn bio_extraction_threshold_filters_low_confidence() {
        let id2label = make_id2label(&["O", "B-EMAIL"]);
        // Token with B-EMAIL but score 0.60 < threshold 0.75 → treated as O
        let predictions = vec![(1, 0.60)];
        let offsets = vec![(0, 9)];
        let special_mask = vec![0u32];

        let spans = extract_bio_spans(&predictions, &offsets, &special_mask, &id2label, 0.75);
        assert!(spans.is_empty());
    }

    #[test]
    fn bio_extraction_two_entities_in_sequence() {
        // John Smith [at] john@example.com
        // B-GIVENNAME I-GIVENNAME O B-EMAIL
        let id2label = make_id2label(&["O", "B-GIVENNAME", "I-GIVENNAME", "B-EMAIL"]);
        let predictions = vec![(1, 0.9), (2, 0.88), (0, 0.99), (3, 0.95)];
        let offsets = vec![(0, 4), (5, 10), (11, 13), (14, 29)];
        let special_mask = vec![0u32; 4];

        let spans = extract_bio_spans(&predictions, &offsets, &special_mask, &id2label, 0.75);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].entity_type, "GIVENNAME");
        assert_eq!(spans[0].start, 0);
        assert_eq!(spans[0].end, 10);
        assert_eq!(spans[1].entity_type, "EMAIL");
        assert_eq!(spans[1].start, 14);
        assert_eq!(spans[1].end, 29);
    }

    #[test]
    fn bio_extraction_orphan_i_starts_span() {
        // I- without preceding B- should still produce a span (lenient decoding)
        let id2label = make_id2label(&["O", "B-PHONE", "I-PHONE"]);
        let predictions = vec![(2, 0.85), (2, 0.80)];
        let offsets = vec![(0, 5), (6, 11)];
        let special_mask = vec![0u32; 2];

        let spans = extract_bio_spans(&predictions, &offsets, &special_mask, &id2label, 0.75);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].entity_type, "PHONE");
    }

    #[test]
    fn pii_classifier_new_sets_fields() {
        let c = CandlePiiClassifier::new("test/repo", 0.75);
        assert_eq!(&*c.repo_id, "test/repo");
        assert!((c.threshold - 0.75).abs() < 1e-6);
        assert!(c.expected_sha256.is_none());
    }

    #[test]
    fn pii_classifier_with_sha256() {
        let c = CandlePiiClassifier::new("test/repo", 0.75).with_sha256("abc123");
        assert_eq!(c.expected_sha256.as_deref(), Some("abc123"));
    }

    #[test]
    fn pii_classifier_backend_name() {
        let c = CandlePiiClassifier::new("test/repo", 0.75);
        assert_eq!(c.backend_name(), "candle-pii-deberta");
    }

    #[test]
    fn pii_classifier_clone_shares_inner_arc() {
        let c = CandlePiiClassifier::new("test/repo", 0.75);
        let c2 = c.clone();
        assert!(Arc::ptr_eq(&c.inner, &c2.inner));
    }

    #[test]
    fn pii_result_empty_text_has_no_pii() {
        // Empty predictions → empty spans
        let spans = extract_bio_spans(&[], &[], &[], &["O".to_string()], 0.75);
        assert!(spans.is_empty());
    }

    // ── Max-confidence merge test ────────────────────────────────────────────

    #[test]
    fn max_confidence_merge_keeps_higher_score() {
        // Simulate two overlapping chunks both predicting the same token.
        // Token at position 5: chunk1 says (B-EMAIL, 0.70), chunk2 says (B-EMAIL, 0.92).
        // After merge, predictions[5] should be (label=1, score=0.92).
        let mut predictions = [(0usize, 0.0f32); 10];

        // Chunk 1: positions 0-7
        let chunk1 = vec![
            (0, 0.99),
            (0, 0.99),
            (0, 0.99),
            (0, 0.99),
            (0, 0.99),
            (1, 0.70), // position 5 — low confidence from chunk1
            (0, 0.99),
            (0, 0.99),
        ];
        for (local, (label, score)) in chunk1.into_iter().enumerate() {
            let global = local;
            if score > predictions[global].1 {
                predictions[global] = (label, score);
            }
        }

        // Chunk 2: positions 4-9 (overlap at 4-7)
        let chunk2 = vec![
            (0, 0.99), // position 4
            (1, 0.92), // position 5 — higher confidence
            (0, 0.99), // position 6
            (0, 0.99), // position 7
            (0, 0.99), // position 8
            (0, 0.99), // position 9
        ];
        let chunk2_start = 4;
        for (local, (label, score)) in chunk2.into_iter().enumerate() {
            let global = chunk2_start + local;
            if score > predictions[global].1 {
                predictions[global] = (label, score);
            }
        }

        // Position 5 should have the higher score from chunk2.
        assert_eq!(predictions[5].0, 1); // label index for B-EMAIL
        assert!(
            (predictions[5].1 - 0.92).abs() < 1e-5,
            "should keep chunk2's higher score"
        );
    }

    // ── Integration tests requiring model download (#[ignore]) ──────────────

    #[tokio::test]
    #[ignore = "requires model download (~280MB, cached in HF_HOME)"]
    async fn real_model_detects_email() {
        let classifier =
            CandlePiiClassifier::new("iiiorg/piiranha-v1-detect-personal-information", 0.75);
        let result = classifier
            .detect_pii("Contact John Smith at john@example.com for details.")
            .await
            .unwrap();
        assert!(result.has_pii, "expected PII detected");
        assert!(!result.spans.is_empty());
    }
}
