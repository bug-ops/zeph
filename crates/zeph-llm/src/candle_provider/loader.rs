use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use candle_core::Device;
use candle_core::quantized::gguf_file;
use candle_transformers::models::quantized_llama::ModelWeights;
use tokenizers::Tokenizer;

#[derive(Debug, Clone)]
pub enum ModelSource {
    Local {
        path: PathBuf,
    },
    HuggingFace {
        repo_id: String,
        filename: Option<String>,
    },
}

pub struct LoadedModel {
    pub weights: ModelWeights,
    pub tokenizer: Tokenizer,
    pub eos_token_id: u32,
}

/// Load a GGUF chat model from the specified source.
///
/// # Errors
///
/// Returns an error if model loading or tokenizer initialization fails.
pub fn load_chat_model(source: &ModelSource, device: &Device) -> Result<LoadedModel> {
    match source {
        ModelSource::Local { path } => {
            let tokenizer_path = path
                .parent()
                .map(|p| p.join("tokenizer.json"))
                .context("invalid model path")?;
            let weights = load_gguf_weights(path, device)?;
            let tokenizer = load_tokenizer(&tokenizer_path)?;
            let eos_token_id = resolve_eos_token(&tokenizer);
            Ok(LoadedModel {
                weights,
                tokenizer,
                eos_token_id,
            })
        }
        ModelSource::HuggingFace { repo_id, filename } => {
            let api =
                hf_hub::api::sync::Api::new().context("failed to create HuggingFace API client")?;
            let repo = api.model(repo_id.clone());

            let model_filename = filename.as_deref().unwrap_or("model.gguf");
            let model_path = repo
                .get(model_filename)
                .with_context(|| format!("failed to download {model_filename} from {repo_id}"))?;

            let tokenizer_path = repo
                .get("tokenizer.json")
                .with_context(|| format!("failed to download tokenizer.json from {repo_id}"))?;

            let weights = load_gguf_weights(&model_path, device)?;
            let tokenizer = load_tokenizer(&tokenizer_path)?;
            let eos_token_id = resolve_eos_token(&tokenizer);
            Ok(LoadedModel {
                weights,
                tokenizer,
                eos_token_id,
            })
        }
    }
}

fn load_gguf_weights(path: &Path, device: &Device) -> Result<ModelWeights> {
    let mut file = std::fs::File::open(path)
        .with_context(|| format!("failed to open GGUF file: {}", path.display()))?;
    let content = gguf_file::Content::read(&mut file).context("failed to parse GGUF file")?;
    ModelWeights::from_gguf(content, &mut file, device)
        .context("failed to load model weights from GGUF")
}

fn load_tokenizer(path: &Path) -> Result<Tokenizer> {
    Tokenizer::from_file(path)
        .map_err(|e| anyhow::anyhow!("failed to load tokenizer from {}: {e}", path.display()))
}

fn resolve_eos_token(tokenizer: &Tokenizer) -> u32 {
    // Common EOS tokens across model families
    const EOS_CANDIDATES: &[&str] = &[
        "</s>",
        "<|endoftext|>",
        "<|eot_id|>",
        "<|im_end|>",
        "<|end|>",
    ];

    for candidate in EOS_CANDIDATES {
        if let Some(id) = tokenizer.token_to_id(candidate) {
            return id;
        }
    }
    // Fallback: token id 2 is EOS in most tokenizers
    2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_source_local_debug() {
        let source = ModelSource::Local {
            path: PathBuf::from("/tmp/model.gguf"),
        };
        let debug = format!("{source:?}");
        assert!(debug.contains("Local"));
        assert!(debug.contains("model.gguf"));
    }

    #[test]
    fn model_source_hf_debug() {
        let source = ModelSource::HuggingFace {
            repo_id: "TheBloke/Mistral-7B".into(),
            filename: Some("model.Q4_K_M.gguf".into()),
        };
        let debug = format!("{source:?}");
        assert!(debug.contains("HuggingFace"));
        assert!(debug.contains("TheBloke/Mistral-7B"));
    }
}
