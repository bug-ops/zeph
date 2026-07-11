// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Candle local-inference backend configuration.
//!
//! Covers the `[llm.candle]` section ([`CandleConfig`]) and the inline per-provider
//! variant ([`CandleInlineConfig`]) used inside `[[llm.providers]]`, plus the shared
//! sampling parameters ([`GenerationParams`]) and device/source selectors.

use serde::{Deserialize, Serialize};

fn default_chat_template() -> String {
    "chatml".into()
}

fn default_temperature() -> f64 {
    0.7
}

fn default_max_tokens() -> usize {
    2048
}

fn default_seed() -> u64 {
    42
}

fn default_repeat_penalty() -> f32 {
    1.1
}

fn default_repeat_last_n() -> usize {
    64
}
/// Model source for the Candle local-inference backend.
///
/// Controls whether the model is downloaded from Hugging Face Hub or loaded
/// from a local filesystem path specified in `local_path`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum CandleSource {
    /// Download model weights from Hugging Face Hub using the `repo_id` from
    /// the provider's `model` field.
    #[default]
    Huggingface,
    /// Load model weights from the local filesystem path in `local_path`.
    Local,
}

/// Compute device for the Candle local-inference backend.
///
/// Determines which hardware accelerator is used for inference. Feature flags
/// `candle/metal` and `candle/cuda` must be enabled at compile time for the
/// corresponding variants to succeed at runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum CandleDevice {
    /// Run inference on the CPU.
    #[default]
    Cpu,
    /// Run inference on an NVIDIA CUDA GPU (requires `cuda` feature).
    Cuda,
    /// Run inference on Apple Silicon via Metal (requires `metal` feature).
    Metal,
    /// Auto-detect the best available device: Metal → CUDA → CPU.
    ///
    /// Requires the corresponding `metal` or `cuda` feature to be enabled at compile time.
    /// Falls back to CPU when no GPU features are compiled in.
    Auto,
}

/// Configuration for the Candle local-inference backend.
///
/// Corresponds to the `[llm.candle]` section in `config.toml`. Used when the
/// agent runs inference locally via `HuggingFace` Candle rather than a remote API.
/// For inline provider definitions inside `[[llm.providers]]`, use
/// [`CandleInlineConfig`] instead.
#[derive(Deserialize, Serialize)]
pub struct CandleConfig {
    #[serde(default)]
    pub source: CandleSource,
    #[serde(default)]
    pub local_path: String,
    #[serde(default)]
    pub filename: Option<String>,
    #[serde(default = "default_chat_template")]
    pub chat_template: String,
    #[serde(default)]
    pub device: CandleDevice,
    #[serde(default)]
    pub embedding_repo: Option<String>,
    /// Resolved `HuggingFace` Hub API token for authenticated model downloads.
    ///
    /// Must be the **token value** — resolved by the caller before constructing this config.
    #[serde(default)]
    pub hf_token: Option<String>,
    #[serde(default)]
    pub generation: GenerationParams,
    /// Maximum seconds to wait for each half of a single inference request.
    ///
    /// The timeout is applied **twice** per `chat()` call: once for the channel send
    /// (waiting for a free slot) and once for the oneshot reply (waiting for the worker
    /// to finish). The effective maximum wall-clock wait per request is therefore
    /// `2 × inference_timeout_secs`. CPU inference can be slow; 120s is a conservative
    /// default for large models, giving up to 240s total before an error is returned.
    /// Values of 0 are silently promoted to 1 at bootstrap.
    #[serde(default = "default_inference_timeout_secs")]
    pub inference_timeout_secs: u64,
}

fn default_inference_timeout_secs() -> u64 {
    120
}

impl std::fmt::Debug for CandleConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CandleConfig")
            .field("source", &self.source)
            .field("local_path", &self.local_path)
            .field("filename", &self.filename)
            .field("chat_template", &self.chat_template)
            .field("device", &self.device)
            .field("embedding_repo", &self.embedding_repo)
            .field("hf_token", &self.hf_token.as_ref().map(|_| "[REDACTED]"))
            .field("generation", &self.generation)
            .field("inference_timeout_secs", &self.inference_timeout_secs)
            .finish()
    }
}

/// Sampling / generation parameters for Candle local inference.
///
/// Used inside `[llm.candle.generation]` or a `[[llm.providers]]` Candle entry.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GenerationParams {
    /// Sampling temperature. Higher values produce more creative outputs. Default: `0.7`.
    #[serde(default = "default_temperature")]
    pub temperature: f64,
    /// Nucleus sampling threshold. When set, tokens with cumulative probability above
    /// this value are excluded. Default: `None` (disabled).
    #[serde(default)]
    pub top_p: Option<f64>,
    /// Top-k sampling. When set, only the top-k most probable tokens are considered.
    /// Default: `None` (disabled).
    #[serde(default)]
    pub top_k: Option<usize>,
    /// Maximum number of tokens to generate per response. Capped at [`MAX_TOKENS_CAP`].
    /// Default: `2048`.
    #[serde(default = "default_max_tokens")]
    pub max_tokens: usize,
    /// Random seed for reproducible outputs. Default: `42`.
    #[serde(default = "default_seed")]
    pub seed: u64,
    /// Repetition penalty applied during sampling. Default: `1.1`.
    #[serde(default = "default_repeat_penalty")]
    pub repeat_penalty: f32,
    /// Number of last tokens to consider for the repetition penalty window. Default: `64`.
    #[serde(default = "default_repeat_last_n")]
    pub repeat_last_n: usize,
}

/// Hard upper bound on `GenerationParams::max_tokens` to prevent unbounded generation.
pub const MAX_TOKENS_CAP: usize = 32768;

impl GenerationParams {
    /// Returns `max_tokens` clamped to [`MAX_TOKENS_CAP`].
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_config::GenerationParams;
    ///
    /// let params = GenerationParams::default();
    /// assert!(params.capped_max_tokens() <= 32768);
    /// ```
    #[must_use]
    pub fn capped_max_tokens(&self) -> usize {
        self.max_tokens.min(MAX_TOKENS_CAP)
    }
}

impl Default for GenerationParams {
    fn default() -> Self {
        Self {
            temperature: default_temperature(),
            top_p: None,
            top_k: None,
            max_tokens: default_max_tokens(),
            seed: default_seed(),
            repeat_penalty: default_repeat_penalty(),
            repeat_last_n: default_repeat_last_n(),
        }
    }
}
/// Inline candle config for use inside `ProviderEntry`.
/// Re-uses the generation params from `CandleConfig`.
#[derive(Clone, Deserialize, Serialize)]
pub struct CandleInlineConfig {
    #[serde(default)]
    pub source: CandleSource,
    #[serde(default)]
    pub local_path: String,
    #[serde(default)]
    pub filename: Option<String>,
    /// Optional SHA-256 hex digest of the chat model file (GGUF).
    ///
    /// When set, the file is verified before loading. Mismatch aborts startup with an error.
    /// Useful for security-sensitive deployments to detect corruption or tampering.
    #[serde(default)]
    pub chat_model_sha256: Option<String>,
    #[serde(default = "default_chat_template")]
    pub chat_template: String,
    #[serde(default)]
    pub device: CandleDevice,
    #[serde(default)]
    pub embedding_repo: Option<String>,
    /// Optional SHA-256 hex digest of the embedding model safetensors file.
    ///
    /// When set, the file is verified before loading. Mismatch aborts startup with an error.
    #[serde(default)]
    pub embedding_model_sha256: Option<String>,
    /// Resolved `HuggingFace` Hub API token for authenticated model downloads.
    #[serde(default)]
    pub hf_token: Option<String>,
    #[serde(default)]
    pub generation: GenerationParams,
    /// Maximum wall-clock seconds to wait for a single inference request.
    ///
    /// Effective timeout is `2 × inference_timeout_secs` (send + recv each have this budget).
    /// CPU inference can be slow; 120s is a conservative default. Floored at 1s.
    #[serde(default = "default_inference_timeout_secs")]
    pub inference_timeout_secs: u64,
}

impl Default for CandleInlineConfig {
    fn default() -> Self {
        Self {
            source: CandleSource::default(),
            local_path: String::new(),
            filename: None,
            chat_model_sha256: None,
            chat_template: default_chat_template(),
            device: CandleDevice::default(),
            embedding_repo: None,
            embedding_model_sha256: None,
            hf_token: None,
            generation: GenerationParams::default(),
            inference_timeout_secs: default_inference_timeout_secs(),
        }
    }
}

impl std::fmt::Debug for CandleInlineConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CandleInlineConfig")
            .field("source", &self.source)
            .field("local_path", &self.local_path)
            .field("filename", &self.filename)
            .field("chat_model_sha256", &self.chat_model_sha256)
            .field("chat_template", &self.chat_template)
            .field("device", &self.device)
            .field("embedding_repo", &self.embedding_repo)
            .field("embedding_model_sha256", &self.embedding_model_sha256)
            .field("hf_token", &self.hf_token.as_ref().map(|_| "[REDACTED]"))
            .field("generation", &self.generation)
            .field("inference_timeout_secs", &self.inference_timeout_secs)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candle_config_debug_redacts_hf_token() {
        let cfg = CandleConfig {
            source: CandleSource::default(),
            local_path: String::new(),
            filename: None,
            chat_template: default_chat_template(),
            device: CandleDevice::default(),
            embedding_repo: None,
            hf_token: Some("hf_SUPERSECRET".to_owned()),
            generation: GenerationParams::default(),
            inference_timeout_secs: default_inference_timeout_secs(),
        };
        let dbg = format!("{cfg:?}");
        assert!(!dbg.contains("hf_SUPERSECRET"));
        assert!(dbg.contains("[REDACTED]"));
    }

    #[test]
    fn candle_config_debug_none_hf_token() {
        let cfg = CandleConfig {
            source: CandleSource::default(),
            local_path: String::new(),
            filename: None,
            chat_template: default_chat_template(),
            device: CandleDevice::default(),
            embedding_repo: None,
            hf_token: None,
            generation: GenerationParams::default(),
            inference_timeout_secs: default_inference_timeout_secs(),
        };
        let dbg = format!("{cfg:?}");
        assert!(!dbg.contains("[REDACTED]"));
        assert!(dbg.contains("hf_token: None"));
    }

    #[test]
    fn candle_inline_config_debug_redacts_hf_token() {
        let cfg = CandleInlineConfig {
            hf_token: Some("hf_SUPERSECRET".to_owned()),
            ..CandleInlineConfig::default()
        };
        let dbg = format!("{cfg:?}");
        assert!(!dbg.contains("hf_SUPERSECRET"));
        assert!(dbg.contains("[REDACTED]"));
    }

    #[test]
    fn candle_inline_config_debug_none_hf_token() {
        let cfg = CandleInlineConfig::default();
        let dbg = format!("{cfg:?}");
        assert!(!dbg.contains("[REDACTED]"));
        assert!(dbg.contains("hf_token: None"));
    }
}
