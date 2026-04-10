// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Speech-to-text (STT) abstraction and result type.
//!
//! The [`SpeechToText`] trait is implemented by any backend that can transcribe
//! audio bytes into text: `OpenAI` Whisper API, local `Candle` Whisper, etc.

use std::future::Future;
use std::pin::Pin;

use crate::error::LlmError;

/// Transcription result from a speech-to-text backend.
#[derive(Debug, Clone)]
pub struct Transcription {
    /// The transcribed text.
    pub text: String,
    /// Detected language, if reported by the backend (e.g. `"en"`, `"de"`).
    pub language: Option<String>,
    /// Duration of the audio in seconds, if reported by the backend.
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
