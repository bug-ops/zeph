// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! ML-backed classifier infrastructure (feature `classifiers`).
//!
//! `ClassifierBackend` is an object-safe async trait. All inference is CPU-bound and must
//! run via `std::thread::spawn` or `tokio::task::spawn_blocking` — never block the async
//! runtime directly.
//!
//! Phase 1: `CandleClassifier` for injection detection.
//! Phase 2: `CandlePiiClassifier` for NER-based PII detection, `LlmClassifier` for feedback.

#[cfg(feature = "classifiers")]
pub mod candle;
#[cfg(feature = "classifiers")]
pub mod candle_pii;
pub mod llm;

use std::future::Future;
use std::pin::Pin;

use crate::error::LlmError;

/// Identifies the type of classifier task for metrics labeling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ClassifierTask {
    /// Sequence classification: whole-text label (INJECTION / SAFE).
    Injection,
    /// Token classification (NER): per-token PII labels.
    Pii,
    /// LLM-backed zero-shot classification: feedback/correction detection.
    Feedback,
}

/// A detected PII span in the input text.
#[derive(Debug, Clone)]
pub struct PiiSpan {
    /// Entity type (e.g. "GIVENNAME", "EMAIL", "PHONE").
    pub entity_type: String,
    /// Start byte offset in original text.
    pub start: usize,
    /// End byte offset in original text (exclusive).
    pub end: usize,
    /// Confidence score (softmax probability of the predicted label).
    pub score: f32,
}

/// Result of a PII detection call.
#[derive(Debug, Clone)]
pub struct PiiResult {
    /// All detected PII spans (merged from regex and NER when both run).
    pub spans: Vec<PiiSpan>,
    /// `true` if any span was detected.
    pub has_pii: bool,
}

/// Object-safe async trait for NER-based PII detection.
///
/// Returns a span list (not a single label) — fundamentally different return type
/// from `ClassifierBackend`, which is why this is a separate trait.
pub trait PiiDetector: Send + Sync {
    /// Detect PII spans in `text`.
    ///
    /// # Errors
    ///
    /// Returns `LlmError::Inference` on tokenization or model forward-pass failure.
    fn detect_pii<'a>(
        &'a self,
        text: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<PiiResult, LlmError>> + Send + 'a>>;

    /// Human-readable backend name used for logging and metrics.
    fn backend_name(&self) -> &'static str;
}

/// Verify the SHA-256 digest of a file against an expected hex string.
///
/// # Errors
///
/// Returns `LlmError::ModelLoad` if the file cannot be read or the digest mismatches.
#[cfg(feature = "classifiers")]
pub(super) fn verify_sha256(path: &std::path::Path, expected: &str) -> Result<(), LlmError> {
    use sha2::{Digest, Sha256};
    use std::io::Read;

    let mut hasher = Sha256::new();
    let mut file = std::fs::File::open(path)
        .map_err(|e| LlmError::ModelLoad(format!("cannot open file for hash check: {e}")))?;
    let mut buf = [0u8; 8192];
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| LlmError::ModelLoad(format!("read error during hash check: {e}")))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let computed = format!("{:x}", hasher.finalize());
    if computed != expected.to_lowercase() {
        return Err(LlmError::ModelLoad(format!(
            "SHA-256 mismatch for {}: expected {}, got {} \
             (file may be corrupt or tampered — do not auto-retry)",
            path.display(),
            expected,
            computed
        )));
    }
    Ok(())
}

/// Result of a single classification call.
///
/// The `is_positive` field means "the classifier's primary detection condition is met."
/// Its interpretation is backend-specific: injection detected, correction signal present, etc.
#[derive(Debug, Clone)]
pub struct ClassificationResult {
    /// Primary predicted label (e.g. `"INJECTION"`, `"SAFE"`).
    pub label: String,
    /// Confidence score in `[0.0, 1.0]`.
    pub score: f32,
    /// `true` when the classifier signals a positive detection (injection / etc.).
    ///
    /// Interpretation is backend-specific — consumers must know which backend produced
    /// this result to interpret the flag correctly.
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

#[cfg(all(test, feature = "classifiers"))]
mod sha256_tests {
    use std::io::Write;

    use super::verify_sha256;

    fn write_tmp(data: &[u8]) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(data).unwrap();
        f
    }

    fn sha256_hex(data: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(data);
        format!("{:x}", h.finalize())
    }

    #[test]
    fn verify_sha256_matching_digest_returns_ok() {
        let data = b"hello world";
        let f = write_tmp(data);
        let expected = sha256_hex(data);
        assert!(verify_sha256(f.path(), &expected).is_ok());
    }

    #[test]
    fn verify_sha256_uppercase_expected_accepted() {
        let data = b"case test";
        let f = write_tmp(data);
        let expected = sha256_hex(data).to_uppercase();
        assert!(verify_sha256(f.path(), &expected).is_ok());
    }

    #[test]
    fn verify_sha256_mismatch_returns_err() {
        let data = b"original";
        let f = write_tmp(data);
        let result = verify_sha256(
            f.path(),
            "0000000000000000000000000000000000000000000000000000000000000000",
        );
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("SHA-256 mismatch"));
    }

    #[test]
    fn verify_sha256_missing_file_returns_err() {
        let result = verify_sha256(std::path::Path::new("/nonexistent/path/file.bin"), "abc");
        assert!(result.is_err());
    }

    #[test]
    fn verify_sha256_empty_file_ok() {
        let f = write_tmp(b"");
        let expected = sha256_hex(b"");
        assert!(verify_sha256(f.path(), &expected).is_ok());
    }
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
