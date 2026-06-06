// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Unified provider entry and pool validation.
//!
//! [`ProviderEntry`] is the flat-union struct deserialized from each `[[llm.providers]]`
//! table; [`validate_pool`] enforces cross-entry invariants (unique names, single
//! default). Also holds the provider-specific sub-structs [`GonkaNode`],
//! [`CocoonPricing`], and the per-session [`ProviderOverrides`].

use serde::{Deserialize, Serialize};

use super::{
    CacheTtl, CandleInlineConfig, GeminiThinkingLevel, ProviderKind, ThinkingConfig, default_true,
    is_true,
};

/// A single Gonka network node endpoint.
///
/// Used in `[[llm.providers]]` entries with `type = "gonka"` to declare
/// the node pool for blockchain inference routing.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct GonkaNode {
    /// HTTP(S) URL of the Gonka node (e.g. `"https://node1.gonka.ai"`).
    pub url: String,
    /// On-chain bech32 address of this node (e.g. `"gonka1w508d6qejxtdg4y5r3zarvary0c5xw7k2gsyg6"`).
    ///
    /// Required for signature construction: every signed request binds to the target node's
    /// on-chain address, making signatures non-replayable across different nodes.
    pub address: String,
    /// Optional human-readable label for `zeph gonka doctor` output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}
/// Per-1K-token pricing for a Cocoon provider, in cents.
///
/// Cocoon model names (e.g. `Qwen/Qwen3-0.6B`) are not in the built-in pricing table.
/// When this struct is present in a provider entry, its values are registered with
/// `CostTracker` at startup so that token costs are tracked accurately.
///
/// Reasoning tokens (when the model uses chain-of-thought) are folded into
/// `completion_tokens` by the Cocoon sidecar and counted at the completion price.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct CocoonPricing {
    /// Prompt (input) token price in cents per 1K tokens.
    #[serde(default)]
    pub prompt_cents_per_1k: f64,
    /// Completion (output) token price in cents per 1K tokens.
    /// Reasoning tokens are counted here since the sidecar folds them into completion tokens.
    #[serde(default)]
    pub completion_cents_per_1k: f64,
}

/// Unified provider entry: one struct replaces `CloudLlmConfig`, `OpenAiConfig`,
/// `GeminiConfig`, `OllamaConfig`, `CompatibleConfig`, and `OrchestratorProviderConfig`.
///
/// Provider-specific fields use `#[serde(default)]` and are ignored by backends
/// that do not use them (flat-union pattern).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[allow(clippy::struct_excessive_bools)] // config struct — boolean flags are idiomatic for TOML-deserialized configuration
pub struct ProviderEntry {
    /// Required: provider backend type.
    #[serde(rename = "type")]
    pub provider_type: ProviderKind,

    /// Optional name for multi-provider configs. Auto-generated from type if absent.
    #[serde(default)]
    pub name: Option<String>,

    /// Model identifier. Required for most types.
    #[serde(default)]
    pub model: Option<String>,

    /// API base URL. Each type has its own default.
    #[serde(default)]
    pub base_url: Option<String>,

    /// Max output tokens.
    #[serde(default)]
    pub max_tokens: Option<u32>,

    /// Embedding model. When set, this provider supports `embed()` calls.
    #[serde(default)]
    pub embedding_model: Option<String>,

    /// STT model. When set, this provider supports speech-to-text via the Whisper API or
    /// Candle-local inference.
    #[serde(default)]
    pub stt_model: Option<String>,

    /// Mark this entry as the embedding provider (handles `embed()` calls).
    #[serde(default)]
    pub embed: bool,

    /// Mark this entry as the default chat provider (overrides position-based default).
    #[serde(default)]
    pub default: bool,

    // --- Claude-specific ---
    #[serde(default)]
    pub thinking: Option<ThinkingConfig>,
    #[serde(default)]
    pub server_compaction: bool,
    #[serde(default)]
    pub enable_extended_context: bool,
    /// Prompt cache TTL variant. `None` keeps the default ~5-minute ephemeral TTL.
    /// Set to `"1h"` to enable the extended 1-hour TTL (beta, ~2× write cost).
    #[serde(default)]
    pub prompt_cache_ttl: Option<CacheTtl>,

    // --- OpenAI-specific ---
    #[serde(default)]
    pub reasoning_effort: Option<String>,

    // --- Gemini-specific ---
    #[serde(default)]
    pub thinking_level: Option<GeminiThinkingLevel>,
    #[serde(default)]
    pub thinking_budget: Option<i32>,
    #[serde(default)]
    pub include_thoughts: Option<bool>,

    // --- Compatible-specific: optional inline api_key ---
    #[serde(default)]
    pub api_key: Option<String>,

    // --- Candle-specific ---
    #[serde(default)]
    pub candle: Option<CandleInlineConfig>,

    // --- Vision ---
    #[serde(default)]
    pub vision_model: Option<String>,

    // --- Gonka-specific ---
    /// Gonka network node pool. Required (non-empty) when `type = "gonka"`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gonka_nodes: Vec<GonkaNode>,
    /// bech32 chain prefix for address encoding. Defaults to `"gonka"` when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gonka_chain_prefix: Option<String>,

    // --- Cocoon-specific ---
    /// Cocoon sidecar HTTP URL. Defaults to `"http://localhost:10000"` when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cocoon_client_url: Option<String>,
    /// Sentinel field for access hash. Leave empty in config; actual value
    /// is resolved from the age vault as `ZEPH_COCOON_ACCESS_HASH`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cocoon_access_hash: Option<String>,
    /// Whether to perform a health check against `/stats` at provider construction time.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub cocoon_health_check: bool,
    /// Manual per-1K-token pricing for this Cocoon provider.
    ///
    /// Cocoon model names (e.g. `Qwen/Qwen3-0.6B`) are not in the built-in pricing table.
    /// When this section is present, the values are registered with `CostTracker` at startup
    /// so that token costs are tracked accurately.
    ///
    /// Example TOML:
    /// ```toml
    /// [llm.providers.cocoon_pricing]
    /// prompt_cents_per_1k = 0.01
    /// completion_cents_per_1k = 0.03
    /// ```
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cocoon_pricing: Option<CocoonPricing>,

    /// Provider-specific instruction file.
    #[serde(default)]
    pub instruction_file: Option<std::path::PathBuf>,

    /// Maximum concurrent LLM calls from orchestrated sub-agents to this provider.
    ///
    /// When set, `DagScheduler` acquires a semaphore permit before dispatching a
    /// sub-agent that targets this provider. Dispatch is deferred (using the existing
    /// `deferral_backoff` mechanism) when the semaphore is saturated.
    ///
    /// `None` (default) = unlimited — no admission control applied.
    ///
    /// # Example (TOML)
    ///
    /// ```toml
    /// [[llm.providers]]
    /// name = "quality"
    /// type = "openai"
    /// model = "gpt-5"
    /// max_concurrent = 3
    /// ```
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_concurrent: Option<u32>,
}

impl Default for ProviderEntry {
    fn default() -> Self {
        Self {
            provider_type: ProviderKind::Ollama,
            name: None,
            model: None,
            base_url: None,
            max_tokens: None,
            embedding_model: None,
            stt_model: None,
            embed: false,
            default: false,
            thinking: None,
            server_compaction: false,
            enable_extended_context: false,
            prompt_cache_ttl: None,
            reasoning_effort: None,
            thinking_level: None,
            thinking_budget: None,
            include_thoughts: None,
            api_key: None,
            candle: None,
            vision_model: None,
            gonka_nodes: Vec::new(),
            gonka_chain_prefix: None,
            cocoon_client_url: None,
            cocoon_access_hash: None,
            cocoon_health_check: true,
            cocoon_pricing: None,
            instruction_file: None,
            max_concurrent: None,
        }
    }
}

impl ProviderEntry {
    /// Resolve the effective name: explicit `name` field or type string.
    #[must_use]
    pub fn effective_name(&self) -> String {
        self.name
            .clone()
            .unwrap_or_else(|| self.provider_type.as_str().to_owned())
    }

    /// Resolve the effective model: explicit `model` field or the provider-type default.
    ///
    /// Defaults mirror those used in `build_provider_from_entry` so that `runtime.model_name`
    /// always reflects the actual model being used rather than the provider type string.
    #[must_use]
    pub fn effective_model(&self) -> String {
        if let Some(ref m) = self.model {
            return m.clone();
        }
        match self.provider_type {
            ProviderKind::Ollama => "qwen3:8b".to_owned(),
            ProviderKind::Claude => "claude-haiku-4-5-20251001".to_owned(),
            ProviderKind::OpenAi => "gpt-4o-mini".to_owned(),
            ProviderKind::Gemini => "gemini-2.0-flash".to_owned(),
            // Compatible/Candle return empty because the model is resolved elsewhere.
            // Gonka returns empty because it is a blockchain provider, not an LLM — there is no model concept.
            ProviderKind::Compatible | ProviderKind::Candle | ProviderKind::Gonka => String::new(),
            ProviderKind::Cocoon => "Qwen/Qwen3-0.6B".to_owned(),
        }
    }

    /// Validate this entry for cross-field consistency.
    ///
    /// # Errors
    ///
    /// Returns `ConfigError` when a fatal invariant is violated (e.g. compatible provider
    /// without a name).
    pub fn validate(&self) -> Result<(), crate::error::ConfigError> {
        use crate::error::ConfigError;

        // B2: compatible provider MUST have name set.
        if self.provider_type == ProviderKind::Compatible && self.name.is_none() {
            return Err(ConfigError::Validation(
                "[[llm.providers]] entry with type=\"compatible\" must set `name`".into(),
            ));
        }

        // B3: gonka provider MUST have name and valid gonka_nodes.
        if self.provider_type == ProviderKind::Gonka {
            if self.name.is_none() {
                return Err(ConfigError::Validation(
                    "[[llm.providers]] entry with type=\"gonka\" must set `name`".into(),
                ));
            }
            self.validate_gonka_nodes()?;
        }

        // B4: cocoon provider MUST have a name.
        if self.provider_type == ProviderKind::Cocoon
            && self.name.as_ref().is_none_or(String::is_empty)
        {
            return Err(ConfigError::Validation(
                "[[llm.providers]] entry with type=\"cocoon\" must set `name`".into(),
            ));
        }

        // B5: cocoon URL must be valid http/https; cocoon model must not be empty.
        if self.provider_type == ProviderKind::Cocoon {
            let name = self.effective_name();
            if let Some(ref url_str) = self.cocoon_client_url {
                match url::Url::parse(url_str) {
                    Err(_) => {
                        return Err(ConfigError::Validation(format!(
                            "[[llm.providers]] entry '{name}': cocoon_client_url \
                             '{url_str}' is not a valid URL; expected format: \
                             http://localhost:10000"
                        )));
                    }
                    Ok(u) if !matches!(u.host_str(), Some("localhost" | "127.0.0.1" | "::1")) => {
                        return Err(ConfigError::Validation(format!(
                            "[[llm.providers]] entry '{name}': cocoon_client_url host must be \
                             localhost or 127.0.0.1, got '{}'",
                            u.host_str().unwrap_or("<none>")
                        )));
                    }
                    Ok(u) if u.scheme() != "http" && u.scheme() != "https" => {
                        return Err(ConfigError::Validation(format!(
                            "[[llm.providers]] entry '{name}': cocoon_client_url \
                             scheme must be http or https, got '{}'",
                            u.scheme()
                        )));
                    }
                    _ => {}
                }
            }
            if self.model.as_deref().is_some_and(|m| m.trim().is_empty()) {
                return Err(ConfigError::Validation(format!(
                    "[[llm.providers]] entry '{name}': model must not be empty \
                     for cocoon provider"
                )));
            }
            if let Some(ref p) = self.cocoon_pricing {
                if !p.prompt_cents_per_1k.is_finite() || p.prompt_cents_per_1k < 0.0 {
                    return Err(ConfigError::Validation(format!(
                        "[[llm.providers]] entry '{name}': cocoon_pricing.prompt_cents_per_1k \
                         must be a finite non-negative number"
                    )));
                }
                if !p.completion_cents_per_1k.is_finite() || p.completion_cents_per_1k < 0.0 {
                    return Err(ConfigError::Validation(format!(
                        "[[llm.providers]] entry '{name}': \
                         cocoon_pricing.completion_cents_per_1k \
                         must be a finite non-negative number"
                    )));
                }
            }
        }

        // B1: warn on irrelevant fields.
        self.warn_irrelevant_fields();

        // W6: Candle STT-only provider (stt_model set, no model) is valid — no warning needed.
        // Warn if Ollama has stt_model set (Ollama does not support Whisper API).
        if self.stt_model.is_some() && self.provider_type == ProviderKind::Ollama {
            tracing::warn!(
                provider = self.effective_name(),
                "field `stt_model` is set on an Ollama provider; Ollama does not support the \
                 Whisper STT API — use OpenAI, compatible, or candle instead"
            );
        }

        Ok(())
    }

    /// Resolve the effective Gonka chain prefix: explicit value or `"gonka"` default.
    #[must_use]
    pub fn effective_gonka_chain_prefix(&self) -> &str {
        self.gonka_chain_prefix.as_deref().unwrap_or("gonka")
    }

    fn warn_irrelevant_fields(&self) {
        let name = self.effective_name();
        match self.provider_type {
            ProviderKind::Ollama => {
                if self.thinking.is_some() {
                    tracing::warn!(
                        provider = name,
                        "field `thinking` is only used by Claude providers"
                    );
                }
                if self.reasoning_effort.is_some() {
                    tracing::warn!(
                        provider = name,
                        "field `reasoning_effort` is only used by OpenAI providers"
                    );
                }
                if self.thinking_level.is_some() || self.thinking_budget.is_some() {
                    tracing::warn!(
                        provider = name,
                        "fields `thinking_level`/`thinking_budget` are only used by Gemini providers"
                    );
                }
            }
            ProviderKind::Claude => {
                if self.reasoning_effort.is_some() {
                    tracing::warn!(
                        provider = name,
                        "field `reasoning_effort` is only used by OpenAI providers"
                    );
                }
                if self.thinking_level.is_some() || self.thinking_budget.is_some() {
                    tracing::warn!(
                        provider = name,
                        "fields `thinking_level`/`thinking_budget` are only used by Gemini providers"
                    );
                }
            }
            ProviderKind::OpenAi => {
                if self.thinking.is_some() {
                    tracing::warn!(
                        provider = name,
                        "field `thinking` is only used by Claude providers"
                    );
                }
                if self.thinking_level.is_some() || self.thinking_budget.is_some() {
                    tracing::warn!(
                        provider = name,
                        "fields `thinking_level`/`thinking_budget` are only used by Gemini providers"
                    );
                }
            }
            ProviderKind::Gemini => {
                if self.thinking.is_some() {
                    tracing::warn!(
                        provider = name,
                        "field `thinking` is only used by Claude providers"
                    );
                }
                if self.reasoning_effort.is_some() {
                    tracing::warn!(
                        provider = name,
                        "field `reasoning_effort` is only used by OpenAI providers"
                    );
                }
            }
            ProviderKind::Gonka => {
                if self.thinking.is_some() {
                    tracing::warn!(
                        provider = name,
                        "field `thinking` is only used by Claude providers"
                    );
                }
                if self.reasoning_effort.is_some() {
                    tracing::warn!(
                        provider = name,
                        "field `reasoning_effort` is only used by OpenAI providers"
                    );
                }
                if self.thinking_level.is_some() || self.thinking_budget.is_some() {
                    tracing::warn!(
                        provider = name,
                        "fields `thinking_level`/`thinking_budget` are only used by Gemini providers"
                    );
                }
            }
            ProviderKind::Compatible | ProviderKind::Candle => {}
            ProviderKind::Cocoon => {
                if self.base_url.is_some() {
                    tracing::warn!(
                        provider = name,
                        "field `base_url` is ignored for cocoon providers; use `cocoon_client_url` instead"
                    );
                }
            }
        }
    }

    fn validate_gonka_nodes(&self) -> Result<(), crate::error::ConfigError> {
        use crate::error::ConfigError;
        if self.gonka_nodes.is_empty() {
            return Err(ConfigError::Validation(format!(
                "[[llm.providers]] entry '{}' with type=\"gonka\" must set non-empty `gonka_nodes`",
                self.effective_name()
            )));
        }
        for (i, node) in self.gonka_nodes.iter().enumerate() {
            if node.url.is_empty() {
                return Err(ConfigError::Validation(format!(
                    "[[llm.providers]] entry '{}' gonka_nodes[{i}].url must not be empty",
                    self.effective_name()
                )));
            }
            if !node.url.starts_with("http://") && !node.url.starts_with("https://") {
                return Err(ConfigError::Validation(format!(
                    "[[llm.providers]] entry '{}' gonka_nodes[{i}].url must start with http:// or https://",
                    self.effective_name()
                )));
            }
        }
        Ok(())
    }
}

/// Per-session LLM generation override parameters persisted across restarts (#4654).
///
/// Phase 1 captures `reasoning_effort` only. Serialized to JSON and stored in the
/// `channel_preferences` table under `pref_key = "provider_overrides"`.
///
/// `#[serde(default)]` makes deserialization forward-compatible: a blob written by a newer
/// binary with additional fields is accepted, unknown fields are ignored, so the known params
/// still apply. (Issue #4654 originally specified `deny_unknown_fields`; this was intentionally
/// relaxed for forward compatibility — see PR and CHANGELOG.)
///
/// # Examples
///
/// ```
/// use zeph_config::ProviderOverrides;
///
/// let overrides = ProviderOverrides {
///     reasoning_effort: Some("high".to_owned()),
/// };
/// assert!(!overrides.is_empty());
///
/// let empty = ProviderOverrides::default();
/// assert!(empty.is_empty());
/// ```
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderOverrides {
    /// `OpenAI` reasoning effort: `"low"`, `"medium"`, or `"high"`. `None` = provider default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
}

impl ProviderOverrides {
    /// Returns `true` when no override is set.
    ///
    /// Used by the persistence layer to skip writing an empty blob.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_config::ProviderOverrides;
    ///
    /// assert!(ProviderOverrides::default().is_empty());
    /// assert!(!ProviderOverrides { reasoning_effort: Some("low".into()) }.is_empty());
    /// ```
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.reasoning_effort.is_none()
    }
}

/// Validate a pool of `ProviderEntry` items.
///
/// # Errors
///
/// Returns `ConfigError` for fatal validation failures:
/// - Empty pool
/// - Duplicate names
/// - Multiple entries marked `default = true`
/// - Individual entry validation errors
pub fn validate_pool(entries: &[ProviderEntry]) -> Result<(), crate::error::ConfigError> {
    use crate::error::ConfigError;
    use std::collections::HashSet;

    if entries.is_empty() {
        return Err(ConfigError::Validation(
            "at least one LLM provider must be configured in [[llm.providers]]".into(),
        ));
    }

    let default_count = entries.iter().filter(|e| e.default).count();
    if default_count > 1 {
        return Err(ConfigError::Validation(
            "only one [[llm.providers]] entry can be marked `default = true`".into(),
        ));
    }

    let mut seen_names: HashSet<String> = HashSet::new();
    for entry in entries {
        let name = entry.effective_name();
        if !seen_names.insert(name.clone()) {
            return Err(ConfigError::Validation(format!(
                "duplicate provider name \"{name}\" in [[llm.providers]]"
            )));
        }
        entry.validate()?;
    }

    Ok(())
}
