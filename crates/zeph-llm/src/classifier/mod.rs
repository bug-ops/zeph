// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! ML-backed classifier infrastructure (feature `classifiers`).
//!
//! `ClassifierBackend` is an object-safe async trait. All inference is CPU-bound and must
//! run via `std::thread::spawn` or `tokio::task::spawn_blocking` — never block the async
//! runtime directly.
//!
//! Phase 1 provides `CandleClassifier` for injection detection and `CandleNerClassifier`
//! for token-level NER (e.g. PII detection via piiranha).

#[cfg(feature = "classifiers")]
pub mod candle;
#[cfg(feature = "classifiers")]
pub mod ner;

use std::future::Future;
use std::pin::Pin;

use crate::error::LlmError;

/// A single token-level entity span from NER inference.
///
/// Character offsets (`start`, `end`) are in the original input string, matching the
/// `HuggingFace` tokenizers library's `Encoding::get_offsets()` output (char offsets, not
/// byte offsets).
#[derive(Debug, Clone)]
pub struct NerSpan {
    /// Entity label (e.g. `"PERSON"`, `"EMAIL"`, `"PHONE"`).
    pub label: String,
    /// Confidence score in `[0.0, 1.0]`.
    pub score: f32,
    /// Character offset of the first character of the span in the original text.
    pub start: usize,
    /// Character offset one past the last character of the span.
    pub end: usize,
}

/// Result of a single classification call.
///
/// The `is_positive` field means "the classifier's primary detection condition is met."
/// Its interpretation is backend-specific: injection detected, PII found, correction
/// signal present, etc.
#[derive(Debug, Clone)]
pub struct ClassificationResult {
    /// Primary predicted label (e.g. `"INJECTION"`, `"SAFE"`).
    pub label: String,
    /// Confidence score in `[0.0, 1.0]`.
    pub score: f32,
    /// `true` when the classifier signals a positive detection (injection / PII / etc.).
    ///
    /// Interpretation is backend-specific — consumers must know which backend produced
    /// this result to interpret the flag correctly.
    pub is_positive: bool,
    /// Token-level entity spans. Populated by NER backends; empty for sequence classifiers.
    pub spans: Vec<NerSpan>,
}

/// Object-safe async classifier interface.
///
/// Use `Pin<Box<dyn Future ...>>` return type so backends can be stored as `Arc<dyn
/// ClassifierBackend>`. RPITIT (`impl Future`) would make the trait non-object-safe.
pub trait ClassifierBackend: Send + Sync {
    /// Classify a text snippet. Returns the highest-confidence label and its score.
    ///
    /// # Errors
    ///
    /// Returns `LlmError::Inference` on tokenization or model forward-pass failure.
    fn classify<'a>(
        &'a self,
        text: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<ClassificationResult, LlmError>> + Send + 'a>>;

    /// Human-readable backend name used for logging and metrics.
    fn backend_name(&self) -> &'static str;
}

#[cfg(test)]
pub mod mock {
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Arc;

    use crate::error::LlmError;

    use super::{ClassificationResult, ClassifierBackend};

    /// Mock backend that returns a fixed result for all inputs. Use in unit tests.
    pub struct MockClassifierBackend {
        pub result: Arc<ClassificationResult>,
    }

    impl MockClassifierBackend {
        #[must_use]
        pub fn new(label: &str, score: f32, is_positive: bool) -> Self {
            Self {
                result: Arc::new(ClassificationResult {
                    label: label.to_owned(),
                    score,
                    is_positive,
                    spans: vec![],
                }),
            }
        }
    }

    impl ClassifierBackend for MockClassifierBackend {
        fn classify<'a>(
            &'a self,
            _text: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<ClassificationResult, LlmError>> + Send + 'a>>
        {
            let result = self.result.as_ref().clone();
            Box::pin(async move { Ok(result) })
        }

        fn backend_name(&self) -> &'static str {
            "mock"
        }
    }
}
