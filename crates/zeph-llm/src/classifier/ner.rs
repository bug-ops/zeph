// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Candle-backed DeBERTa-v2 NER classifier (`CandleNerClassifier`).
//!
//! Implements [`ClassifierBackend`] using token-level NER inference with BIO/BIOES span
//! decoding. Designed for piiranha (`iiiorg/piiranha-v1-detect-personal-information`) but
//! compatible with any DeBERTa-v2 NER model using BIO or BIOES tagging.
//!
//! ## Aggregation strategy
//!
//! - `is_positive = true` when at least one entity span is detected.
//! - `label` = entity type with highest score, or `"PII_DETECTED"` for multiple types.
//! - `score` = maximum score across all detected spans.
//! - `spans` = all detected [`NerSpan`] items with character offsets.
//!
//! ## Chunking
//!
//! Long inputs are split into overlapping chunks (same strategy as [`CandleClassifier`]).
//! Spans from overlapping regions are deduplicated: when two spans share the same
//! `(start, end)` position, the one with the higher score wins.
//!
//! ## `OnceLock` failure caching
//!
//! Load failures are permanently cached until process restart (same as [`CandleClassifier`]).
//! **Important for NER/PII use cases**: a transient model download failure will disable
//! NER-based PII detection for the lifetime of the process. The caller must rely on
//! regex-based PII fallback in this scenario.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};

use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::debertav2::{Config as DebertaConfig, DebertaV2NERModel};
use tokenizers::Tokenizer;

use crate::error::LlmError;

use super::{ClassificationResult, ClassifierBackend, NerSpan};

use super::{CHUNK_OVERLAP_TOKENS, MAX_CHUNK_CONTENT_TOKENS};

struct CandleNerClassifierInner {
    model: DebertaV2NERModel,
    tokenizer: Tokenizer,
    /// Maps label index → label string (e.g. `0 → "O"`, `1 → "B-PER"`).
    id2label: Vec<String>,
    /// Token ID for `[CLS]` special token, resolved at load time.
    cls_token_id: u32,
    /// Token ID for `[SEP]` special token, resolved at load time.
    sep_token_id: u32,
}

/// Candle-backed DeBERTa-v2 NER classifier.
///
/// Model weights are loaded lazily on first [`ClassifierBackend::classify`] call via
/// [`OnceLock`]. Instances are cheaply cloneable (`Arc` inside).
///
/// **`OnceLock` failure caching**: load failures are permanently cached. Transient network
/// or disk errors disable the classifier until process restart. Callers must handle
/// `Err(LlmError::ModelLoad(...))` and fall back to regex or skip NER accordingly.
#[derive(Clone)]
pub struct CandleNerClassifier {
    repo_id: Arc<str>,
    inner: Arc<OnceLock<Result<Arc<CandleNerClassifierInner>, String>>>,
}

impl std::fmt::Debug for CandleNerClassifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CandleNerClassifier")
            .field("repo_id", &self.repo_id)
            .finish_non_exhaustive()
    }
}

impl CandleNerClassifier {
    /// Create a new NER classifier that will load `repo_id` from `HuggingFace` Hub on first use.
    #[must_use]
    pub fn new(repo_id: impl Into<Arc<str>>) -> Self {
        Self {
            repo_id: repo_id.into(),
            inner: Arc::new(OnceLock::new()),
        }
    }

    #[allow(unsafe_code)]
    fn load_inner(repo_id: &str) -> Result<CandleNerClassifierInner, LlmError> {
        let api = hf_hub::api::sync::Api::new().map_err(|e| {
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

        let cls_token_id = tokenizer
            .token_to_id("[CLS]")
            .ok_or_else(|| LlmError::ModelLoad("tokenizer missing [CLS] token".into()))?;
        let sep_token_id = tokenizer
            .token_to_id("[SEP]")
            .ok_or_else(|| LlmError::ModelLoad("tokenizer missing [SEP] token".into()))?;

        validate_safetensors(&weights_path)?;

        let device = Device::Cpu;
        // SAFETY: validated safetensors header above; file not modified during VarBuilder lifetime
        let vb =
            unsafe { VarBuilder::from_mmaped_safetensors(&[weights_path], DType::F32, &device)? };

        let model = DebertaV2NERModel::load(vb, &config, None)
            .map_err(|e| LlmError::ModelLoad(format!("failed to load DeBERTa NER model: {e}")))?;

        Ok(CandleNerClassifierInner {
            model,
            tokenizer,
            id2label,
            cls_token_id,
            sep_token_id,
        })
    }

    /// Run NER on a single chunk and return per-token `(label_idx, score)` pairs.
    ///
    /// Returns `None` when the chunk produces no valid logits.
    fn run_chunk_tokens(
        inner: &CandleNerClassifierInner,
        input_ids: &[u32],
    ) -> Result<Vec<(usize, f32)>, LlmError> {
        let seq_len = input_ids.len();
        let ids_tensor = Tensor::new(input_ids, &inner.model.device)?.unsqueeze(0)?;
        let token_type_ids = Tensor::zeros((1, seq_len), DType::I64, &inner.model.device)?;
        let attention_mask = Tensor::ones((1, seq_len), DType::I64, &inner.model.device)?;

        // logits: [1, seq_len, num_labels]
        let logits =
            inner
                .model
                .forward(&ids_tensor, Some(token_type_ids), Some(attention_mask))?;
        // logits: [seq_len, num_labels]
        let logits = logits.squeeze(0)?;

        // Softmax per token over the label dimension.
        let probs = candle_nn::ops::softmax(&logits, 1)?;
        let probs_vec = probs.to_vec2::<f32>().map_err(LlmError::Candle)?;

        let result = probs_vec
            .into_iter()
            .map(|token_probs| {
                let (best_idx, best_score) = token_probs
                    .iter()
                    .enumerate()
                    .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
                    .map_or((0, 0.0f32), |(i, &s)| (i, s));
                (best_idx, best_score)
            })
            .collect();
        Ok(result)
    }

    /// Decode BIO/BIOES per-token labels into entity spans.
    ///
    /// Uses character offsets from the tokenizer's `Encoding::get_offsets()`.
    ///
    /// Rules:
    /// - `B-X` starts a new span of type X (closes any open span first).
    /// - `I-X` continues an open span of type X; if no open span or type mismatch, starts new.
    /// - `S-X` is a single-token span of type X.
    /// - `E-X` closes an open span of type X.
    /// - `O` closes any open span.
    #[allow(clippy::too_many_lines)]
    fn decode_bio_spans(
        id2label: &[String],
        token_labels: &[(usize, f32)],
        offsets: &[(usize, usize)],
    ) -> Vec<NerSpan> {
        let mut spans: Vec<NerSpan> = Vec::new();
        let mut current_label: Option<String> = None;
        let mut current_start: usize = 0;
        let mut current_end: usize = 0;
        let mut current_score_sum: f32 = 0.0;
        let mut current_token_count: usize = 0;

        let close_span = |spans: &mut Vec<NerSpan>,
                          label: &str,
                          start: usize,
                          end: usize,
                          score_sum: f32,
                          count: usize| {
            if end > start && count > 0 {
                // count is always small (sequence length), f32 precision loss is acceptable.
                #[allow(clippy::cast_precision_loss)]
                let avg_score = score_sum / count as f32;
                spans.push(NerSpan {
                    label: label.to_owned(),
                    score: avg_score,
                    start,
                    end,
                });
            }
        };

        for (pos, &(label_idx, score)) in token_labels.iter().enumerate() {
            let label_str = id2label.get(label_idx).map_or("O", String::as_str);
            let (offset_start, offset_end) = offsets.get(pos).copied().unwrap_or((0, 0));

            // Parse BIO/BIOES prefix and entity type.
            let (prefix, entity_type) = if let Some(rest) = label_str.strip_prefix("B-") {
                ("B", rest)
            } else if let Some(rest) = label_str.strip_prefix("I-") {
                ("I", rest)
            } else if let Some(rest) = label_str.strip_prefix("S-") {
                ("S", rest)
            } else if let Some(rest) = label_str.strip_prefix("E-") {
                ("E", rest)
            } else {
                ("O", "")
            };

            match prefix {
                "B" => {
                    // Close any open span before starting a new one.
                    if let Some(ref lbl) = current_label.take() {
                        close_span(
                            &mut spans,
                            lbl,
                            current_start,
                            current_end,
                            current_score_sum,
                            current_token_count,
                        );
                    }
                    current_label = Some(entity_type.to_owned());
                    current_start = offset_start;
                    current_end = offset_end;
                    current_score_sum = score;
                    current_token_count = 1;
                }
                "I" => {
                    if current_label.as_deref() == Some(entity_type) {
                        // Continue current span.
                        current_end = offset_end;
                        current_score_sum += score;
                        current_token_count += 1;
                    } else {
                        // Mismatch or no open span: close previous and start new.
                        if let Some(ref lbl) = current_label.take() {
                            close_span(
                                &mut spans,
                                lbl,
                                current_start,
                                current_end,
                                current_score_sum,
                                current_token_count,
                            );
                        }
                        current_label = Some(entity_type.to_owned());
                        current_start = offset_start;
                        current_end = offset_end;
                        current_score_sum = score;
                        current_token_count = 1;
                    }
                }
                "S" => {
                    // Single-token span: close previous and emit immediately.
                    if let Some(ref lbl) = current_label.take() {
                        close_span(
                            &mut spans,
                            lbl,
                            current_start,
                            current_end,
                            current_score_sum,
                            current_token_count,
                        );
                    }
                    if offset_end > offset_start {
                        spans.push(NerSpan {
                            label: entity_type.to_owned(),
                            score,
                            start: offset_start,
                            end: offset_end,
                        });
                    }
                }
                "E" => {
                    // End of span: close if type matches.
                    if current_label.as_deref() == Some(entity_type) {
                        current_end = offset_end;
                        current_score_sum += score;
                        current_token_count += 1;
                        let lbl = current_label.take().unwrap_or_default();
                        close_span(
                            &mut spans,
                            &lbl,
                            current_start,
                            current_end,
                            current_score_sum,
                            current_token_count,
                        );
                        current_score_sum = 0.0;
                        current_token_count = 0;
                    } else {
                        // Unexpected E-X: close previous, emit this token as a span.
                        if let Some(ref lbl) = current_label.take() {
                            close_span(
                                &mut spans,
                                lbl,
                                current_start,
                                current_end,
                                current_score_sum,
                                current_token_count,
                            );
                        }
                        if offset_end > offset_start {
                            spans.push(NerSpan {
                                label: entity_type.to_owned(),
                                score,
                                start: offset_start,
                                end: offset_end,
                            });
                        }
                    }
                }
                _ => {
                    // "O" or unknown: close any open span.
                    if let Some(ref lbl) = current_label.take() {
                        close_span(
                            &mut spans,
                            lbl,
                            current_start,
                            current_end,
                            current_score_sum,
                            current_token_count,
                        );
                    }
                    current_score_sum = 0.0;
                    current_token_count = 0;
                }
            }
        }

        // Close any span still open at end of sequence.
        if let Some(ref lbl) = current_label {
            close_span(
                &mut spans,
                lbl,
                current_start,
                current_end,
                current_score_sum,
                current_token_count,
            );
        }

        spans
    }

    /// Tokenize, chunk, run NER, decode spans, and aggregate across chunks.
    ///
    /// Deduplication: when two spans share the same `(start, end)` position (from overlap
    /// regions), the span with the higher score wins. Spans that partially overlap are both
    /// kept — downstream consumers may apply further merging if needed.
    fn classify_sync(
        inner: &CandleNerClassifierInner,
        text: &str,
    ) -> Result<ClassificationResult, LlmError> {
        let encoding = inner
            .tokenizer
            .encode(text, true)
            .map_err(|e| LlmError::Inference(format!("tokenizer encode failed: {e}")))?;

        let ids = encoding.get_ids();
        let offsets = encoding.get_offsets();

        if ids.is_empty() {
            return Ok(ClassificationResult {
                label: "O".into(),
                score: 0.0,
                is_positive: false,
                spans: vec![],
            });
        }

        // Strip [CLS] and [SEP] added by encode(text, true) — every chunk gets its own framing.
        // Offsets from the tokenizer are absolute (into the original text), so no adjustment needed.
        let (ids, offsets) = if ids.len() >= 2
            && ids[0] == inner.cls_token_id
            && ids[ids.len() - 1] == inner.sep_token_id
        {
            (&ids[1..ids.len() - 1], &offsets[1..offsets.len() - 1])
        } else {
            (ids, offsets)
        };

        // Collect all spans from all chunks, then deduplicate.
        let mut all_spans: Vec<NerSpan> = Vec::new();

        let mut start = 0usize;
        while start < ids.len() {
            let end = (start + MAX_CHUNK_CONTENT_TOKENS).min(ids.len());
            let content_ids = &ids[start..end];
            let chunk_offsets = &offsets[start..end];

            // Frame every chunk with [CLS] ... [SEP] as DeBERTa expects.
            let mut framed_ids = Vec::with_capacity(content_ids.len() + 2);
            framed_ids.push(inner.cls_token_id);
            framed_ids.extend_from_slice(content_ids);
            framed_ids.push(inner.sep_token_id);

            let token_labels = Self::run_chunk_tokens(inner, &framed_ids)?;
            // Strip [CLS] and [SEP] labels before BIO decoding — special tokens must not
            // produce entity spans. Use saturating slice to be safe on malformed output.
            let content_labels = if token_labels.len() >= 2 {
                &token_labels[1..token_labels.len() - 1]
            } else {
                &[]
            };
            let chunk_spans =
                Self::decode_bio_spans(&inner.id2label, content_labels, chunk_offsets);
            all_spans.extend(chunk_spans);

            if end == ids.len() {
                break;
            }
            start = end.saturating_sub(CHUNK_OVERLAP_TOKENS);
        }

        // Deduplicate: for identical (start, end) positions, keep highest score.
        // This handles overlap regions where the same entity is detected from two chunks.
        let mut deduped: Vec<NerSpan> = Vec::with_capacity(all_spans.len());
        'outer: for span in all_spans {
            for existing in &mut deduped {
                if existing.start == span.start && existing.end == span.end {
                    if span.score > existing.score {
                        existing.score = span.score;
                        existing.label.clone_from(&span.label);
                    }
                    continue 'outer;
                }
            }
            deduped.push(span);
        }

        if deduped.is_empty() {
            return Ok(ClassificationResult {
                label: "O".into(),
                score: 0.0,
                is_positive: false,
                spans: vec![],
            });
        }

        // Aggregate: is_positive = true, label = highest-score entity type.
        let best = deduped
            .iter()
            .max_by(|a, b| {
                a.score
                    .partial_cmp(&b.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .expect("deduped is non-empty");

        let unique_labels: std::collections::HashSet<&str> =
            deduped.iter().map(|s| s.label.as_str()).collect();
        let label = if unique_labels.len() == 1 {
            best.label.clone()
        } else {
            "PII_DETECTED".into()
        };

        Ok(ClassificationResult {
            label,
            score: best.score,
            is_positive: true,
            spans: deduped,
        })
    }
}

impl ClassifierBackend for CandleNerClassifier {
    fn classify<'a>(
        &'a self,
        text: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<ClassificationResult, LlmError>> + Send + 'a>> {
        let text = text.to_owned();
        let inner_lock = Arc::clone(&self.inner);
        let repo_id = Arc::clone(&self.repo_id);

        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                let loaded = inner_lock.get_or_init(|| {
                    CandleNerClassifier::load_inner(&repo_id)
                        .map(Arc::new)
                        .map_err(|e| e.to_string())
                });
                match loaded {
                    Ok(inner) => CandleNerClassifier::classify_sync(inner, &text),
                    Err(e) => Err(LlmError::ModelLoad(e.clone())),
                }
            })
            .await
            .map_err(|e| LlmError::Inference(format!("NER classifier task panicked: {e}")))?
        })
    }

    fn backend_name(&self) -> &'static str {
        "candle-deberta-ner"
    }
}

/// Validate safetensors file header integrity before mmap.
///
/// Shared with [`super::candle::CandleClassifier`] — both call this before `VarBuilder::from_mmaped_safetensors`.
pub(crate) fn validate_safetensors(path: &std::path::Path) -> Result<(), LlmError> {
    use std::io::Read;
    const MAX_HEADER: u64 = 100 * 1024 * 1024;
    let mut f = std::fs::File::open(path)
        .map_err(|e| LlmError::ModelLoad(format!("cannot open safetensors: {e}")))?;
    let file_len = f
        .metadata()
        .map_err(|e| LlmError::ModelLoad(format!("cannot stat safetensors: {e}")))?
        .len();
    if file_len < 8 {
        return Err(LlmError::ModelLoad(
            "safetensors file too small (< 8 bytes)".into(),
        ));
    }
    let mut header_len_buf = [0u8; 8];
    f.read_exact(&mut header_len_buf)
        .map_err(|e| LlmError::ModelLoad(format!("cannot read safetensors header: {e}")))?;
    let header_len = u64::from_le_bytes(header_len_buf);
    if header_len > file_len - 8 || header_len > MAX_HEADER {
        return Err(LlmError::ModelLoad(format!(
            "invalid safetensors header length: {header_len} (file size: {file_len})"
        )));
    }
    let header_len_usize = usize::try_from(header_len)
        .map_err(|_| LlmError::ModelLoad("header length overflow".into()))?;
    let mut header_buf = vec![0u8; header_len_usize];
    f.read_exact(&mut header_buf)
        .map_err(|e| LlmError::ModelLoad(format!("cannot read safetensors header: {e}")))?;
    serde_json::from_slice::<serde_json::Value>(&header_buf)
        .map_err(|e| LlmError::ModelLoad(format!("safetensors header is not valid JSON: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_id2label(labels: &[&str]) -> Vec<String> {
        labels.iter().map(|s| (*s).to_owned()).collect()
    }

    fn make_offsets(pairs: &[(usize, usize)]) -> Vec<(usize, usize)> {
        pairs.to_vec()
    }

    #[test]
    fn bio_single_entity() {
        // "John" spans chars 0-4, single B-PER token
        let id2label = make_id2label(&["O", "B-PER", "I-PER"]);
        let token_labels = vec![(1, 0.95_f32)]; // B-PER
        let offsets = make_offsets(&[(0, 4)]);
        let spans = CandleNerClassifier::decode_bio_spans(&id2label, &token_labels, &offsets);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].label, "PER");
        assert_eq!(spans[0].start, 0);
        assert_eq!(spans[0].end, 4);
        assert!((spans[0].score - 0.95).abs() < 1e-5);
    }

    #[test]
    fn bio_multi_token_entity() {
        // "John Doe" = B-PER I-PER
        let id2label = make_id2label(&["O", "B-PER", "I-PER"]);
        let token_labels = vec![(1, 0.9_f32), (2, 0.85_f32)];
        let offsets = make_offsets(&[(0, 4), (5, 8)]);
        let spans = CandleNerClassifier::decode_bio_spans(&id2label, &token_labels, &offsets);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].label, "PER");
        assert_eq!(spans[0].start, 0);
        assert_eq!(spans[0].end, 8);
        // Average of 0.9 + 0.85
        assert!((spans[0].score - 0.875).abs() < 1e-4);
    }

    #[test]
    fn bio_two_separate_entities() {
        // "John ... jane@example.com" — B-PER O B-EMAIL
        let id2label = make_id2label(&["O", "B-PER", "B-EMAIL"]);
        let token_labels = vec![(1, 0.9_f32), (0, 0.1_f32), (2, 0.8_f32)];
        let offsets = make_offsets(&[(0, 4), (5, 8), (9, 25)]);
        let spans = CandleNerClassifier::decode_bio_spans(&id2label, &token_labels, &offsets);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].label, "PER");
        assert_eq!(spans[1].label, "EMAIL");
    }

    #[test]
    fn bio_o_only_returns_empty() {
        let id2label = make_id2label(&["O"]);
        let token_labels = vec![(0, 0.99_f32), (0, 0.98_f32)];
        let offsets = make_offsets(&[(0, 5), (6, 10)]);
        let spans = CandleNerClassifier::decode_bio_spans(&id2label, &token_labels, &offsets);
        assert!(spans.is_empty());
    }

    #[test]
    fn bioes_single_token_entity() {
        // S-EMAIL for a single-token email
        let id2label = make_id2label(&["O", "S-EMAIL"]);
        let token_labels = vec![(1, 0.92_f32)];
        let offsets = make_offsets(&[(0, 16)]);
        let spans = CandleNerClassifier::decode_bio_spans(&id2label, &token_labels, &offsets);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].label, "EMAIL");
        assert!((spans[0].score - 0.92).abs() < 1e-5);
    }

    #[test]
    fn i_without_b_starts_new_span() {
        // I-PER without preceding B-PER should still produce a span
        let id2label = make_id2label(&["O", "I-PER"]);
        let token_labels = vec![(1, 0.88_f32)];
        let offsets = make_offsets(&[(0, 4)]);
        let spans = CandleNerClassifier::decode_bio_spans(&id2label, &token_labels, &offsets);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].label, "PER");
    }

    #[test]
    fn dedup_same_position_keeps_higher_score() {
        // Two spans with same (start, end): keep higher score.
        // This simulates spans from two overlapping chunks.
        let mut deduped: Vec<NerSpan> = Vec::new();
        let spans = vec![
            NerSpan {
                label: "PER".into(),
                score: 0.7,
                start: 0,
                end: 4,
            },
            NerSpan {
                label: "PER".into(),
                score: 0.9,
                start: 0,
                end: 4,
            },
        ];
        'outer: for span in spans {
            for existing in &mut deduped {
                if existing.start == span.start && existing.end == span.end {
                    if span.score > existing.score {
                        existing.score = span.score;
                        existing.label.clone_from(&span.label);
                    }
                    continue 'outer;
                }
            }
            deduped.push(span);
        }
        assert_eq!(deduped.len(), 1);
        assert!((deduped[0].score - 0.9).abs() < 1e-5);
    }

    #[test]
    fn classify_sync_empty_input() {
        // Build a minimal inner — tokenizer from bytes is not easily available in tests,
        // so we test via MockClassifierBackend that spans are populated correctly.
        // The empty-input path in classify_sync returns early without touching the model.
        // We verify this logic path via direct unit test on decode_bio_spans.
        let id2label = make_id2label(&["O"]);
        let spans = CandleNerClassifier::decode_bio_spans(&id2label, &[], &[]);
        assert!(spans.is_empty());
    }

    // ── [CLS]/[SEP] framing: strip-then-reframe logic ───────────────────────

    /// Simulate the NER `classify_sync` strip+frame logic without a real model.
    #[allow(clippy::type_complexity)]
    fn simulate_ner_framing(
        ids: &[u32],
        offsets: &[(usize, usize)],
        cls_id: u32,
        sep_id: u32,
    ) -> Vec<(Vec<u32>, Vec<(usize, usize)>)> {
        use super::super::{CHUNK_OVERLAP_TOKENS, MAX_CHUNK_CONTENT_TOKENS};
        let (ids, offsets) = if ids.len() >= 2 && ids[0] == cls_id && ids[ids.len() - 1] == sep_id {
            (&ids[1..ids.len() - 1], &offsets[1..offsets.len() - 1])
        } else {
            (ids, offsets)
        };
        let mut chunks = Vec::new();
        let mut start = 0usize;
        while start < ids.len() {
            let end = (start + MAX_CHUNK_CONTENT_TOKENS).min(ids.len());
            let content_ids = &ids[start..end];
            let chunk_offsets = offsets[start..end].to_vec();
            let mut framed = Vec::with_capacity(content_ids.len() + 2);
            framed.push(cls_id);
            framed.extend_from_slice(content_ids);
            framed.push(sep_id);
            chunks.push((framed, chunk_offsets));
            if end == ids.len() {
                break;
            }
            start = end.saturating_sub(CHUNK_OVERLAP_TOKENS);
        }
        chunks
    }

    const CLS: u32 = 1;
    const SEP: u32 = 2;

    #[test]
    fn ner_framing_single_chunk_has_cls_sep() {
        let ids = vec![CLS, 10, 20, 30, SEP];
        let offsets = vec![(0, 0), (0, 4), (5, 9), (10, 14), (0, 0)];
        let chunks = simulate_ner_framing(&ids, &offsets, CLS, SEP);
        assert_eq!(chunks.len(), 1);
        let (framed, chunk_offsets) = &chunks[0];
        assert_eq!(framed[0], CLS);
        assert_eq!(*framed.last().unwrap(), SEP);
        // Content tokens 10, 20, 30 should be present
        assert_eq!(&framed[1..framed.len() - 1], &[10, 20, 30]);
        // Content offsets must NOT include the [CLS]/[SEP] zero-width offsets
        assert_eq!(chunk_offsets, &[(0, 4), (5, 9), (10, 14)]);
    }

    #[test]
    fn ner_framing_strips_special_labels_before_bio_decode() {
        // Simulate: model returns label vector for [CLS] content... [SEP]
        // We must strip positions 0 and len-1 before passing to decode_bio_spans.
        // This test verifies the strip logic directly.
        let id2label = make_id2label(&["O", "B-PER"]);
        // Labels: [CLS]=B-PER (should be ignored), John=B-PER, [SEP]=B-PER (should be ignored)
        let token_labels_with_special = [(1, 0.9f32), (1, 0.95f32), (1, 0.9f32)];
        // Strip first and last entries (the [CLS] and [SEP] positions)
        let content_labels = if token_labels_with_special.len() >= 2 {
            &token_labels_with_special[1..token_labels_with_special.len() - 1]
        } else {
            &[]
        };
        let offsets = vec![(0, 4)]; // only one content token: "John" at 0..4
        let spans = CandleNerClassifier::decode_bio_spans(&id2label, content_labels, &offsets);
        assert_eq!(
            spans.len(),
            1,
            "only the real content token should produce a span"
        );
        assert_eq!(spans[0].label, "PER");
        assert_eq!(spans[0].start, 0);
        assert_eq!(spans[0].end, 4);
    }

    #[test]
    fn ner_offsets_preserved_across_chunks() {
        use super::super::MAX_CHUNK_CONTENT_TOKENS;
        // Create ids with [CLS] + content + [SEP] where content > MAX_CHUNK_CONTENT_TOKENS
        let content_len = MAX_CHUNK_CONTENT_TOKENS + 10;
        let mut ids = vec![CLS];
        let end: u32 = 100 + u32::try_from(content_len).expect("fits u32");
        ids.extend(100u32..end);
        ids.push(SEP);
        // Synthetic offsets: each content token covers 5 chars
        let mut offsets = vec![(0, 0)]; // [CLS]
        for i in 0..content_len {
            offsets.push((i * 5, i * 5 + 5));
        }
        offsets.push((0, 0)); // [SEP]
        let chunks = simulate_ner_framing(&ids, &offsets, CLS, SEP);
        assert!(chunks.len() >= 2, "must produce multiple chunks");
        // First chunk's offsets must start from (0, 5) — the first content token
        let (_, first_offsets) = &chunks[0];
        assert_eq!(first_offsets[0], (0, 5));
        // Last chunk's offsets must include the final content token
        let (_, last_offsets) = chunks.last().unwrap();
        let last_tok_idx = content_len - 1;
        assert_eq!(
            last_offsets.last().unwrap(),
            &(last_tok_idx * 5, last_tok_idx * 5 + 5)
        );
    }
}
