use std::sync::Arc;

use candle_core::{DType, Device, Tensor};

use crate::error::LlmError;
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config as BertConfig};
use tokenizers::Tokenizer;

#[derive(Clone)]
pub struct EmbedModel {
    model: Arc<BertModel>,
    tokenizer: Tokenizer,
    device: Device,
}

impl std::fmt::Debug for EmbedModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EmbedModel")
            .field("device", &self.device)
            .finish_non_exhaustive()
    }
}

impl EmbedModel {
    /// Load a BERT embedding model from `HuggingFace` Hub.
    ///
    /// # Errors
    ///
    /// Returns an error if model download or loading fails.
    pub fn load(repo_id: &str, device: &Device) -> Result<Self, LlmError> {
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
            .map_err(|e| LlmError::ModelLoad(format!("failed to read BERT config: {e}")))?;
        let config: BertConfig = serde_json::from_str(&config_str)?;

        let tokenizer = Tokenizer::from_file(&tokenizer_path)
            .map_err(|e| LlmError::ModelLoad(format!("failed to load tokenizer: {e}")))?;

        // SAFETY: file is a valid safetensors downloaded from hf-hub, not modified during
        // VarBuilder lifetime
        let vb =
            unsafe { VarBuilder::from_mmaped_safetensors(&[weights_path], DType::F32, device)? };

        let model = BertModel::load(vb, &config)?;

        Ok(Self {
            model: Arc::new(model),
            tokenizer,
            device: device.clone(),
        })
    }

    /// Generate embeddings for the given text.
    ///
    /// # Errors
    ///
    /// Returns an error if tokenization or the model forward pass fails.
    pub fn embed_sync(&self, text: &str) -> Result<Vec<f32>, LlmError> {
        let encoding = self
            .tokenizer
            .encode(text, true)
            .map_err(|e| LlmError::Inference(format!("tokenizer encode failed: {e}")))?;

        let token_ids = encoding.get_ids();
        let token_type_ids: Vec<u32> = vec![0; token_ids.len()];

        let input_ids = Tensor::new(token_ids, &self.device)?.unsqueeze(0)?;
        let token_type_ids = Tensor::new(token_type_ids.as_slice(), &self.device)?.unsqueeze(0)?;

        let embeddings = self.model.forward(&input_ids, &token_type_ids, None)?;

        // Mean pooling over sequence dimension
        let seq_len = embeddings.dim(1)?;
        let sum = embeddings.sum(1)?;
        let mean_pooled = (sum
            / f64::from(
                u32::try_from(seq_len)
                    .map_err(|e| LlmError::Inference(format!("sequence length overflow: {e}")))?,
            ))?;

        // L2 normalization
        let norm = mean_pooled.sqr()?.sum_keepdim(1)?.sqrt()?;
        let normalized = mean_pooled.broadcast_div(&norm)?.squeeze(0)?;

        normalized.to_vec1::<f32>().map_err(LlmError::Candle)
    }
}
