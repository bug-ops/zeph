pub mod embed;
pub mod generate;
pub mod loader;
pub mod template;

pub use candle_core::Device;

use tokenizers::Tokenizer;

use crate::error::LlmError;

use self::embed::EmbedModel;
use self::generate::{GenerationConfig, GenerationOutput, generate_tokens};
use self::loader::{LoadedModel, ModelSource, load_chat_model};
use self::template::ChatTemplate;
use crate::provider::{ChatStream, LlmProvider, Message};

use candle_transformers::models::quantized_llama::ModelWeights;

#[derive(Clone)]
pub struct CandleProvider {
    // NOTE: MVP — std::sync::Mutex serializes inference. Consider per-request model clone
    // or tokio::sync::Mutex for async fairness.
    weights: std::sync::Arc<std::sync::Mutex<ModelWeights>>,
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
        device: Device,
    ) -> Result<Self, LlmError> {
        let LoadedModel {
            weights,
            tokenizer,
            eos_token_id,
        } = load_chat_model(source, &device)?;

        let embed_model = if let Some(repo) = embedding_repo {
            Some(std::sync::Arc::new(EmbedModel::load(repo, &device)?))
        } else {
            None
        };

        Ok(Self {
            weights: std::sync::Arc::new(std::sync::Mutex::new(weights)),
            tokenizer: std::sync::Arc::new(tokenizer),
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

    fn generate_sync(&self, messages: &[Message]) -> Result<String, LlmError> {
        let prompt = self.template.format(messages);
        let encoding = self
            .tokenizer
            .encode(prompt.as_str(), false)
            .map_err(|e| LlmError::Inference(format!("tokenizer encode failed: {e}")))?;
        let input_tokens = encoding.get_ids();

        let weights = self.weights.clone();
        let mut forward_fn =
            |input: &candle_core::Tensor, pos: usize| -> Result<candle_core::Tensor, LlmError> {
                let mut w = weights
                    .lock()
                    .map_err(|e| LlmError::Inference(format!("model lock poisoned: {e}")))?;
                w.forward(input, pos).map_err(LlmError::Candle)
            };

        let GenerationOutput {
            text,
            tokens_generated,
        } = generate_tokens(
            &mut forward_fn,
            &self.tokenizer,
            input_tokens,
            &self.generation_config,
            self.eos_token_id,
            &self.device,
        )?;

        tracing::debug!("generated {tokens_generated} token(s)");
        Ok(text)
    }
}

impl LlmProvider for CandleProvider {
    async fn chat(&self, messages: &[Message]) -> Result<String, LlmError> {
        let provider = self.clone();
        let messages = messages.to_vec();
        tokio::task::spawn_blocking(move || provider.generate_sync(&messages))
            .await
            .map_err(|e| LlmError::Inference(format!("candle generation task failed: {e}")))?
    }

    // NOTE: MVP fake streaming — generates all tokens then chunks
    async fn chat_stream(&self, messages: &[Message]) -> Result<ChatStream, LlmError> {
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        let provider = self.clone();
        let messages = messages.to_vec();

        tokio::task::spawn_blocking(move || match provider.generate_sync(&messages) {
            Ok(text) => {
                let mut start = 0;
                while start < text.len() {
                    let mut end = (start + 32).min(text.len());
                    while !text.is_char_boundary(end) {
                        end -= 1;
                    }
                    if tx.blocking_send(Ok(text[start..end].to_string())).is_err() {
                        break;
                    }
                    start = end;
                }
            }
            Err(e) => {
                let _ = tx.blocking_send(Err(e));
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

    fn name(&self) -> &str {
        "candle"
    }
}
