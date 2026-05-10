// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Speech-to-text provider for the Cocoon confidential compute sidecar.
//!
//! [`CocoonSttProvider`] implements [`SpeechToText`] by forwarding audio via
//! multipart POST to the sidecar's `/v1/audio/transcriptions` endpoint.
//! The sidecar accepts the standard OpenAI Whisper wire format.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::cocoon::client::CocoonClient;
use crate::error::LlmError;
use crate::stt::{SpeechToText, Transcription};

/// STT provider that routes audio through the Cocoon sidecar.
///
/// Uses the same `CocoonClient` transport as the LLM provider — the sidecar
/// exposes `/v1/audio/transcriptions` in the OpenAI Whisper format.
///
/// # Examples
///
/// ```no_run
/// use std::sync::Arc;
/// use std::time::Duration;
/// use zeph_llm::cocoon::{CocoonClient, CocoonSttProvider};
///
/// let client = Arc::new(CocoonClient::new(
///     "http://localhost:10000",
///     None,
///     Duration::from_secs(30),
/// ));
/// let stt = CocoonSttProvider::new("whisper-1", client);
/// ```
pub struct CocoonSttProvider {
    model: String,
    client: Arc<CocoonClient>,
    language: Option<String>,
}

impl std::fmt::Debug for CocoonSttProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CocoonSttProvider")
            .field("model", &self.model)
            .finish_non_exhaustive()
    }
}

impl CocoonSttProvider {
    /// Construct a new `CocoonSttProvider`.
    ///
    /// - `model` — Whisper model name forwarded in the `model` form field (e.g. `"whisper-1"`).
    /// - `client` — shared transport; must point to the same sidecar as the LLM provider.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use std::sync::Arc;
    /// use std::time::Duration;
    /// use zeph_llm::cocoon::{CocoonClient, CocoonSttProvider};
    ///
    /// let client = Arc::new(CocoonClient::new(
    ///     "http://localhost:10000", None, Duration::from_secs(30),
    /// ));
    /// let stt = CocoonSttProvider::new("whisper-1", client);
    /// ```
    #[must_use]
    pub fn new(model: impl Into<String>, client: Arc<CocoonClient>) -> Self {
        Self {
            model: model.into(),
            client,
            language: None,
        }
    }

    /// Set the transcription language hint.
    ///
    /// `"auto"` and empty strings are treated as no hint (the sidecar auto-detects).
    #[must_use]
    pub fn with_language(mut self, language: impl Into<String>) -> Self {
        let lang = language.into();
        if lang != "auto" && !lang.is_empty() {
            self.language = Some(lang);
        }
        self
    }
}

#[derive(serde::Deserialize)]
struct WhisperResponse {
    text: String,
}

impl SpeechToText for CocoonSttProvider {
    fn transcribe(
        &self,
        audio: &[u8],
        filename: Option<&str>,
    ) -> Pin<Box<dyn Future<Output = Result<Transcription, LlmError>> + Send + '_>> {
        use tracing::Instrument as _;
        let span = tracing::info_span!("llm.cocoon.stt.transcribe", model = %self.model);
        let audio = audio.to_vec();
        let fname = filename.unwrap_or("audio.wav").to_string();
        Box::pin(
            async move {
                let part = reqwest::multipart::Part::bytes(audio)
                    .file_name(fname)
                    .mime_str("application/octet-stream")
                    .map_err(|e| LlmError::TranscriptionFailed(e.to_string()))?;

                let mut form = reqwest::multipart::Form::new()
                    .text("model", self.model.clone())
                    .text("response_format", "json")
                    .part("file", part);
                if let Some(ref lang) = self.language {
                    form = form.text("language", lang.clone());
                }

                let resp = self
                    .client
                    .post_multipart("/v1/audio/transcriptions", form)
                    .await?;

                if !resp.status().is_success() {
                    let status = resp.status();
                    let mut body = resp.text().await.unwrap_or_default();
                    body.truncate(500);
                    return Err(LlmError::TranscriptionFailed(format!("{status}: {body}")));
                }

                let parsed: WhisperResponse = resp.json().await.map_err(LlmError::Http)?;
                Ok(Transcription {
                    text: parsed.text,
                    language: None,
                    duration_secs: None,
                })
            }
            .instrument(span),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn make_client() -> Arc<CocoonClient> {
        Arc::new(CocoonClient::new(
            "http://localhost:10000",
            None,
            Duration::from_secs(30),
        ))
    }

    #[test]
    fn construction_stores_model() {
        let stt = CocoonSttProvider::new("whisper-1", make_client());
        assert_eq!(stt.model, "whisper-1");
        assert!(stt.language.is_none());
    }

    #[test]
    fn with_language_sets_non_auto() {
        let stt = CocoonSttProvider::new("whisper-1", make_client()).with_language("en");
        assert_eq!(stt.language.as_deref(), Some("en"));
    }

    #[test]
    fn with_language_ignores_auto() {
        let stt = CocoonSttProvider::new("whisper-1", make_client()).with_language("auto");
        assert!(stt.language.is_none());
    }

    #[test]
    fn with_language_ignores_empty() {
        let stt = CocoonSttProvider::new("whisper-1", make_client()).with_language("");
        assert!(stt.language.is_none());
    }

    #[test]
    fn debug_does_not_expose_internals() {
        let stt = CocoonSttProvider::new("whisper-1", make_client());
        let debug = format!("{stt:?}");
        assert!(debug.contains("whisper-1"));
    }
}
