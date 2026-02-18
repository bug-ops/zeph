use std::future::Future;
use std::pin::Pin;

use crate::error::LlmError;

#[derive(Debug, Clone)]
pub struct Transcription {
    pub text: String,
    pub language: Option<String>,
    pub duration_secs: Option<f32>,
}

/// Async trait for speech-to-text backends.
pub trait SpeechToText: Send + Sync {
    /// Transcribe audio bytes into text.
    ///
    /// # Errors
    ///
    /// Returns `LlmError::TranscriptionFailed` if the backend rejects the request.
    fn transcribe(
        &self,
        audio: &[u8],
        filename: Option<&str>,
    ) -> Pin<Box<dyn Future<Output = Result<Transcription, LlmError>> + Send + '_>>;
}
