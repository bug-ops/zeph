use std::sync::Arc;

use anyhow::{Context, Result};
use candle_core::{DType, Device, Tensor};
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
    pub fn load(repo_id: &str, device: &Device) -> Result<Self> {
        let api =
            hf_hub::api::sync::Api::new().context("failed to create HuggingFace API client")?;
        let repo = api.model(repo_id.to_owned());

        let config_path = repo
            .get("config.json")
            .with_context(|| format!("failed to download config.json from {repo_id}"))?;
        let tokenizer_path = repo
            .get("tokenizer.json")
            .with_context(|| format!("failed to download tokenizer.json from {repo_id}"))?;
        let weights_path = repo
            .get("model.safetensors")
            .with_context(|| format!("failed to download model.safetensors from {repo_id}"))?;

        let config_str =
            std::fs::read_to_string(&config_path).context("failed to read BERT config")?;
        let config: BertConfig =
            serde_json::from_str(&config_str).context("failed to parse BERT config")?;

        let tokenizer = Tokenizer::from_file(&tokenizer_path)
            .map_err(|e| anyhow::anyhow!("failed to load tokenizer: {e}"))?;

        // SAFETY: file is a valid safetensors downloaded from hf-hub, not modified during
        // VarBuilder lifetime
        let vb = unsafe {
            VarBuilder::from_mmaped_safetensors(&[weights_path], DType::F32, device)
                .context("failed to memory-map safetensors")?
        };

        let model = BertModel::load(vb, &config).context("failed to load BERT model")?;

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
    pub fn embed_sync(&self, text: &str) -> Result<Vec<f32>> {
        let encoding = self
            .tokenizer
            .encode(text, true)
            .map_err(|e| anyhow::anyhow!("tokenizer encode failed: {e}"))?;

        let token_ids = encoding.get_ids();
        let token_type_ids: Vec<u32> = vec![0; token_ids.len()];

        let input_ids = Tensor::new(token_ids, &self.device)?.unsqueeze(0)?;
        let token_type_ids = Tensor::new(token_type_ids.as_slice(), &self.device)?.unsqueeze(0)?;

        let embeddings = self
            .model
            .forward(&input_ids, &token_type_ids, None)
            .context("BERT forward pass failed")?;

        // Mean pooling over sequence dimension
        let seq_len = embeddings.dim(1)?;
        let sum = embeddings.sum(1)?;
        let mean_pooled = (sum / f64::from(u32::try_from(seq_len)?))?;

        // L2 normalization
        let norm = mean_pooled.sqr()?.sum_keepdim(1)?.sqrt()?;
        let normalized = mean_pooled.broadcast_div(&norm)?.squeeze(0)?;

        normalized
            .to_vec1::<f32>()
            .context("failed to convert embedding tensor to vec")
    }
}
