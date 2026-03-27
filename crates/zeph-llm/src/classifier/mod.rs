// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! ML-backed classifier infrastructure (feature `classifiers`).
//!
//! `ClassifierBackend` is an object-safe async trait. All inference is CPU-bound and must
//! run via `std::thread::spawn` or `tokio::task::spawn_blocking` — never block the async
//! runtime directly.
//!
//! Phase 1 provides only `CandleClassifier` for injection detection.

#[cfg(feature = "classifiers")]
pub mod candle;

use std::future::Future;
use std::pin::Pin;

use crate::error::LlmError;

/// Result of a single classification call.
#[derive(Debug, Clone)]
pub struct ClassificationResult {
    /// Primary predicted label (e.g. `"INJECTION"`, `"SAFE"`).
    pub label: String,
    /// Confidence score in `[0.0, 1.0]`.
    pub score: f32,
    /// `true` when the classifier signals a positive detection (injection / PII / etc.).
    pub is_positive: bool,
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
