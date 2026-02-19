use std::future::Future;
use std::pin::Pin;

use crate::error::LlmError;
use crate::stt::{SpeechToText, Transcription};

pub struct WhisperProvider {
    client: reqwest::Client,
    api_key: String,
    base_url: String,
    model: String,
}

impl WhisperProvider {
    #[must_use]
    pub fn new(
        client: reqwest::Client,
        api_key: impl Into<String>,
        base_url: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            client,
            api_key: api_key.into(),
            base_url: base_url.into(),
            model: model.into(),
        }
    }
}

impl std::fmt::Debug for WhisperProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WhisperProvider")
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .finish_non_exhaustive()
    }
}

#[derive(serde::Deserialize)]
struct WhisperResponse {
    text: String,
}

impl SpeechToText for WhisperProvider {
    fn transcribe(
        &self,
        audio: &[u8],
        filename: Option<&str>,
    ) -> Pin<Box<dyn Future<Output = Result<Transcription, LlmError>> + Send + '_>> {
        let audio = audio.to_vec();
        let fname = filename.unwrap_or("audio.wav").to_string();
        Box::pin(async move {
            let part = reqwest::multipart::Part::bytes(audio)
                .file_name(fname)
                .mime_str("application/octet-stream")
                .map_err(|e| LlmError::TranscriptionFailed(e.to_string()))?;

            let form = reqwest::multipart::Form::new()
                .text("model", self.model.clone())
                .text("response_format", "json")
                .part("file", part);

            let url = format!(
                "{}/audio/transcriptions",
                self.base_url.trim_end_matches('/')
            );
            let resp = self
                .client
                .post(&url)
                .bearer_auth(&self.api_key)
                .multipart(form)
                .send()
                .await?;

            if !resp.status().is_success() {
                let status = resp.status();
                let mut body = resp.text().await.unwrap_or_default();
                body.truncate(500);
                return Err(LlmError::TranscriptionFailed(format!("{status}: {body}")));
            }

            let parsed: WhisperResponse = resp.json().await?;
            Ok(Transcription {
                text: parsed.text,
                language: None,
                duration_secs: None,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whisper_provider_construction() {
        let client = reqwest::Client::new();
        let provider =
            WhisperProvider::new(client, "sk-test", "https://api.openai.com/v1", "whisper-1");
        assert_eq!(provider.base_url, "https://api.openai.com/v1");
        assert_eq!(provider.model, "whisper-1");
    }

    #[test]
    fn whisper_provider_debug_redacts_key() {
        let client = reqwest::Client::new();
        let provider = WhisperProvider::new(
            client,
            "sk-secret",
            "https://api.openai.com/v1",
            "whisper-1",
        );
        let debug = format!("{provider:?}");
        assert!(!debug.contains("sk-secret"));
    }
}
