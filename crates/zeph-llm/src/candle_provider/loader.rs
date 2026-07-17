// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::path::{Path, PathBuf};

use candle_core::Device;

use crate::error::LlmError;
use candle_core::quantized::gguf_file;
use candle_transformers::models::quantized_llama::ModelWeights;
use tokenizers::Tokenizer;

#[non_exhaustive]
#[derive(Debug, Clone)]
pub enum ModelSource {
    Local {
        path: PathBuf,
    },
    HuggingFace {
        repo_id: String,
        filename: Option<String>,
        /// Optional expected SHA-256 hex digest of the downloaded model file.
        ///
        /// When set, [`load_chat_model`] verifies the digest before parsing the GGUF
        /// weights. Mismatch aborts the load — see `classifier::verify_sha256`.
        sha256: Option<String>,
    },
}

impl ModelSource {
    /// Returns a model identifier suitable for `is_reasoning_model` classification.
    ///
    /// `HuggingFace` sources use `repo_id` (e.g. `deepseek-ai/DeepSeek-R1-Distill-Qwen-7B`),
    /// the canonical identity that reasoning-model heuristics match against. `Local` sources
    /// have no such canonical id, so this falls back to the file stem of `path` — a known
    /// limitation: a generically-named local GGUF (e.g. `model-q4.gguf`) yields a false
    /// negative for reasoning-model detection, since the filename carries no model identity.
    #[must_use]
    pub fn model_id(&self) -> String {
        match self {
            ModelSource::Local { path } => path
                .file_stem()
                .and_then(std::ffi::OsStr::to_str)
                .map_or_else(|| path.display().to_string(), ToOwned::to_owned),
            ModelSource::HuggingFace { repo_id, .. } => repo_id.clone(),
        }
    }
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
pub fn load_chat_model(
    source: &ModelSource,
    hf_token: Option<&str>,
    device: &Device,
) -> Result<LoadedModel, LlmError> {
    match source {
        ModelSource::Local { path } => {
            let tokenizer_path = path
                .parent()
                .map(|p| p.join("tokenizer.json"))
                .ok_or_else(|| LlmError::ModelLoad("invalid model path".into()))?;
            let weights = load_gguf_weights(path, device)?;
            let tokenizer = load_tokenizer(&tokenizer_path)?;
            let eos_token_id = resolve_eos_token(&tokenizer);
            Ok(LoadedModel {
                weights,
                tokenizer,
                eos_token_id,
            })
        }
        ModelSource::HuggingFace {
            repo_id,
            filename,
            sha256,
        } => {
            let client = match hf_token {
                Some(token) => hf_hub::HFClientBuilder::new().token(token).build_sync(),
                None => hf_hub::HFClientSync::new(),
            }
            .map_err(|e| {
                LlmError::ModelLoad(format!("failed to create HuggingFace API client: {e}"))
            })?;
            let (owner, name) = hf_hub::split_id(repo_id);
            let repo = client.model(owner, name);

            let model_filename = filename.as_deref().unwrap_or("model.gguf");
            let model_path = repo
                .download_file()
                .filename(model_filename)
                .send()
                .map_err(|e| {
                    LlmError::ModelLoad(format!(
                        "failed to download {model_filename} from {repo_id}: {e}"
                    ))
                })?;

            let tokenizer_path = repo
                .download_file()
                .filename("tokenizer.json")
                .send()
                .map_err(|e| {
                    LlmError::ModelLoad(format!(
                        "failed to download tokenizer.json from {repo_id}: {e}"
                    ))
                })?;

            if let Some(expected_hash) = sha256 {
                crate::classifier::verify_sha256(&model_path, expected_hash)?;
            }

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

fn load_gguf_weights(path: &Path, device: &Device) -> Result<ModelWeights, LlmError> {
    let mut file = std::fs::File::open(path).map_err(|e| {
        LlmError::ModelLoad(format!("failed to open GGUF file {}: {e}", path.display()))
    })?;
    let content = gguf_file::Content::read(&mut file)
        .map_err(|e| LlmError::ModelLoad(format!("failed to parse GGUF file: {e}")))?;
    ModelWeights::from_gguf(content, &mut file, device).map_err(LlmError::Candle)
}

fn load_tokenizer(path: &Path) -> Result<Tokenizer, LlmError> {
    Tokenizer::from_file(path).map_err(|e| {
        LlmError::ModelLoad(format!(
            "failed to load tokenizer from {}: {e}",
            path.display()
        ))
    })
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
            sha256: None,
        };
        let debug = format!("{source:?}");
        assert!(debug.contains("HuggingFace"));
        assert!(debug.contains("TheBloke/Mistral-7B"));
    }

    // ── #6183: model_id() must return an identifier `is_reasoning_model` can match ──

    #[test]
    fn model_source_model_id_local_uses_file_stem() {
        let source = ModelSource::Local {
            path: PathBuf::from("/models/deepseek-r1-distill-qwen-7b.gguf"),
        };
        assert_eq!(source.model_id(), "deepseek-r1-distill-qwen-7b");
    }

    #[test]
    fn model_source_model_id_local_falls_back_to_full_path_when_no_stem() {
        // `Path::file_stem()` returns `None` when there is no file name component
        // (e.g. the root path) — verify the documented fallback to `path.display()`.
        let source = ModelSource::Local {
            path: PathBuf::from("/"),
        };
        assert_eq!(source.model_id(), "/");
    }

    #[test]
    fn model_source_model_id_huggingface_uses_repo_id() {
        let source = ModelSource::HuggingFace {
            repo_id: "deepseek-ai/DeepSeek-R1-Distill-Qwen-7B".into(),
            filename: Some("model.Q4_K_M.gguf".into()),
            sha256: None,
        };
        assert_eq!(source.model_id(), "deepseek-ai/DeepSeek-R1-Distill-Qwen-7B");
    }

    #[test]
    fn model_source_model_id_huggingface_ignores_filename() {
        // repo_id is the canonical identity even when filename carries no signal.
        let source = ModelSource::HuggingFace {
            repo_id: "microsoft/Phi-3-mini-4k-instruct".into(),
            filename: None,
            sha256: None,
        };
        assert_eq!(source.model_id(), "microsoft/Phi-3-mini-4k-instruct");
    }
}
