// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

pub mod embed;
pub mod generate;
pub mod loader;
pub mod template;
pub mod worker;

pub use candle_core::Device;

use std::time::Duration;
use tokenizers::Tokenizer;

use crate::error::LlmError;

use self::embed::EmbedModel;
use self::generate::GenerationConfig;
use self::loader::{LoadedModel, ModelSource, load_chat_model};
use self::template::ChatTemplate;
use self::worker::{
    DEFAULT_INFERENCE_TIMEOUT_SECS, InferenceRequest, InferenceWorker, WorkerConfig,
};
use crate::provider::{ChatStream, LlmProvider, Message, StreamChunk};

/// Bounded channel capacity for inference requests.
///
/// At most 4 requests may be queued. Callers block (async) when full, providing
/// natural backpressure.  Capacity 4 covers the concurrent `chat` + `chat_stream`
/// + speculative calls edge case without unbounded growth.
const WORKER_CHANNEL_CAPACITY: usize = 4;

pub struct CandleProvider {
    worker: InferenceWorker,
    tokenizer: std::sync::Arc<Tokenizer>,
    eos_token_id: u32,
    template: ChatTemplate,
    generation_config: GenerationConfig,
    embed_model: Option<std::sync::Arc<EmbedModel>>,
    device: Device,
}

impl std::fmt::Debug for CandleProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CandleProvider")
            .field("template", &self.template)
            .field("generation_config", &self.generation_config)
            .field("device", &format!("{:?}", self.device))
            .field("embed_model", &self.embed_model)
            .finish_non_exhaustive()
    }
}

impl Clone for CandleProvider {
    fn clone(&self) -> Self {
        Self {
            // Clone the Sender — both copies route to the same worker.
            worker: InferenceWorker {
                tx: self.worker.tx.clone(),
                inference_timeout: self.worker.inference_timeout,
                // None on clones: the original InferenceWorker owns the JoinHandle.
                _handle: None,
            },
            tokenizer: std::sync::Arc::clone(&self.tokenizer),
            eos_token_id: self.eos_token_id,
            template: self.template,
            generation_config: self.generation_config.clone(),
            embed_model: self.embed_model.clone(),
            device: self.device.clone(),
        }
    }
}

impl CandleProvider {
    /// Create a new `CandleProvider` from a model source.
    ///
    /// # Errors
    ///
    /// Returns an error if model loading or embedding model initialization fails.
    pub fn new(
        source: &ModelSource,
        template: ChatTemplate,
        generation_config: GenerationConfig,
        embedding_repo: Option<&str>,
        hf_token: Option<&str>,
        device: Device,
    ) -> Result<Self, LlmError> {
        Self::new_with_timeout(
            source,
            template,
            generation_config,
            embedding_repo,
            hf_token,
            device,
            Duration::from_secs(DEFAULT_INFERENCE_TIMEOUT_SECS),
        )
    }

    /// Create a new `CandleProvider` with a custom inference timeout.
    ///
    /// # Errors
    ///
    /// Returns an error if model loading or embedding model initialization fails.
    pub fn new_with_timeout(
        source: &ModelSource,
        template: ChatTemplate,
        generation_config: GenerationConfig,
        embedding_repo: Option<&str>,
        hf_token: Option<&str>,
        device: Device,
        inference_timeout: Duration,
    ) -> Result<Self, LlmError> {
        let LoadedModel {
            weights,
            tokenizer,
            eos_token_id,
        } = load_chat_model(source, hf_token, &device)?;

        let embed_model = if let Some(repo) = embedding_repo {
            Some(std::sync::Arc::new(EmbedModel::load(
                repo, hf_token, &device,
            )?))
        } else {
            None
        };

        let tokenizer = std::sync::Arc::new(tokenizer);
        let worker = InferenceWorker::spawn(
            WorkerConfig {
                weights,
                tokenizer: std::sync::Arc::clone(&tokenizer),
                eos_token_id,
                template,
                generation_config: generation_config.clone(),
                device: device.clone(),
            },
            WORKER_CHANNEL_CAPACITY,
            inference_timeout,
        );

        Ok(Self {
            worker,
            tokenizer,
            eos_token_id,
            template,
            generation_config,
            embed_model,
            device,
        })
    }

    #[must_use]
    pub fn device_name(&self) -> &'static str {
        match &self.device {
            Device::Cpu => "cpu",
            Device::Cuda(_) => "cuda",
            Device::Metal(_) => "metal",
        }
    }

    /// Send an inference request to the worker and await the result.
    ///
    /// Applies `inference_timeout` to both the channel send and the oneshot recv.
    /// Maps `RecvError` (worker panic / drop) to `LlmError::Inference`.
    async fn dispatch(&self, messages: Vec<Message>) -> Result<String, LlmError> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        let req = InferenceRequest {
            messages,
            reply: reply_tx,
        };

        // M2: bounded send with timeout — blocks if channel is full.
        tokio::time::timeout(self.worker.inference_timeout, self.worker.tx.send(req))
            .await
            .map_err(|_| LlmError::Inference("inference worker send timed out".into()))?
            .map_err(|_| LlmError::Inference("inference worker channel closed".into()))?;

        // M2: bounded recv with timeout — blocks until worker replies.
        let output = tokio::time::timeout(self.worker.inference_timeout, reply_rx)
            .await
            .map_err(|_| LlmError::Inference("inference worker reply timed out".into()))?
            // M1: RecvError means the worker panicked or was dropped.
            .map_err(|_| LlmError::Inference("inference worker died".into()))??;

        tracing::debug!("generated {} token(s)", output.tokens_generated);
        Ok(output.text)
    }
}

impl LlmProvider for CandleProvider {
    async fn chat(&self, messages: &[Message]) -> Result<String, LlmError> {
        self.dispatch(messages.to_vec()).await
    }

    // NOTE: MVP fake streaming — generates all tokens then chunks
    async fn chat_stream(&self, messages: &[Message]) -> Result<ChatStream, LlmError> {
        let text = self.dispatch(messages.to_vec()).await?;
        let (tx, rx) = tokio::sync::mpsc::channel(64);

        tokio::spawn(async move {
            let mut start = 0;
            while start < text.len() {
                let mut end = (start + 32).min(text.len());
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                let chunk = StreamChunk::Content(text[start..end].to_string());
                if tx.send(Ok(chunk)).await.is_err() {
                    break;
                }
                start = end;
            }
        });

        Ok(Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx)))
    }

    fn supports_streaming(&self) -> bool {
        true
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>, LlmError> {
        let Some(ref embed_model) = self.embed_model else {
            return Err(LlmError::EmbedUnsupported {
                provider: "candle".into(),
            });
        };
        let model = embed_model.clone();
        let text = text.to_owned();
        tokio::task::spawn_blocking(move || model.embed_sync(&text))
            .await
            .map_err(|e| LlmError::Inference(format!("candle embedding task failed: {e}")))?
    }

    fn supports_embeddings(&self) -> bool {
        self.embed_model.is_some()
    }

    fn name(&self) -> &'static str {
        "candle"
    }
}
