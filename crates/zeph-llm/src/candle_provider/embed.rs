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

        Self::validate_safetensors(&weights_path)?;

        // SAFETY: validated safetensors header above; file not modified during VarBuilder lifetime
        let vb =
            unsafe { VarBuilder::from_mmaped_safetensors(&[weights_path], DType::F32, device)? };

        let model = BertModel::load(vb, &config)?;

        Ok(Self {
            model: Arc::new(model),
            tokenizer,
            device: device.clone(),
        })
    }

    fn validate_safetensors(path: &std::path::Path) -> Result<(), LlmError> {
        use std::io::Read;
        let mut f = std::fs::File::open(path)
            .map_err(|e| LlmError::ModelLoad(format!("cannot open safetensors: {e}")))?;
        let file_len = f
            .metadata()
            .map_err(|e| LlmError::ModelLoad(format!("cannot stat safetensors: {e}")))?
            .len();
        if file_len < 8 {
            return Err(LlmError::ModelLoad(
                "safetensors file too small (< 8 bytes)".into(),
            ));
        }
        let mut header_len_buf = [0u8; 8];
        f.read_exact(&mut header_len_buf)
            .map_err(|e| LlmError::ModelLoad(format!("cannot read safetensors header: {e}")))?;
        let header_len = u64::from_le_bytes(header_len_buf);
        // Header must fit within the file and be under 100 MB
        const MAX_HEADER: u64 = 100 * 1024 * 1024;
        if header_len > file_len - 8 || header_len > MAX_HEADER {
            return Err(LlmError::ModelLoad(format!(
                "invalid safetensors header length: {header_len} (file size: {file_len})"
            )));
        }
        let header_len_usize = usize::try_from(header_len)
            .map_err(|_| LlmError::ModelLoad("header length overflow".into()))?;
        let mut header_buf = vec![0u8; header_len_usize];
        f.read_exact(&mut header_buf)
            .map_err(|e| LlmError::ModelLoad(format!("cannot read safetensors header: {e}")))?;
        serde_json::from_slice::<serde_json::Value>(&header_buf).map_err(|e| {
            LlmError::ModelLoad(format!("safetensors header is not valid JSON: {e}"))
        })?;
        Ok(())
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
