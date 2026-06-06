// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Session, document, and semantic-memory configuration.
//!
//! Per-session history limits, document chunking/ingestion, the semantic-memory
//! toggle surface, and context-strategy selection.

use crate::defaults::default_true;
use crate::providers::ProviderName;
use serde::{Deserialize, Serialize};

use super::default_embed_timeout_secs;

fn default_max_history() -> usize {
    100
}

fn default_title_max_chars() -> usize {
    60
}

fn default_document_collection() -> String {
    "zeph_documents".into()
}

fn default_document_chunk_size() -> usize {
    1000
}

fn default_document_chunk_overlap() -> usize {
    100
}

fn default_document_top_k() -> usize {
    3
}

fn default_temporal_decay_half_life_days() -> u32 {
    30
}

fn default_mmr_lambda() -> f32 {
    0.7
}

fn default_semantic_enabled() -> bool {
    true
}

fn default_recall_limit() -> usize {
    5
}

fn default_vector_weight() -> f64 {
    0.7
}

fn default_keyword_weight() -> f64 {
    0.3
}

fn validate_importance_weight<'de, D>(deserializer: D) -> Result<f64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = <f64 as serde::Deserialize>::deserialize(deserializer)?;
    if value.is_nan() || value.is_infinite() {
        return Err(serde::de::Error::custom(
            "importance_weight must be a finite number",
        ));
    }
    if value < 0.0 {
        return Err(serde::de::Error::custom(
            "importance_weight must be non-negative",
        ));
    }
    if value > 1.0 {
        return Err(serde::de::Error::custom("importance_weight must be <= 1.0"));
    }
    Ok(value)
}

fn default_importance_weight() -> f64 {
    0.15
}

/// Context assembly strategy (#2288).
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ContextStrategy {
    /// Full conversation history trimmed to budget, with memory augmentation.
    /// This is the default and existing behavior.
    #[default]
    FullHistory,
    /// Drop conversation history; assemble context from summaries, semantic recall,
    /// cross-session memory, and session digest only.
    MemoryFirst,
    /// Start as `FullHistory`; switch to `MemoryFirst` when turn count exceeds
    /// `crossover_turn_threshold`.
    Adaptive,
}

/// Session list and auto-title configuration, nested under `[memory.sessions]` in TOML.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct SessionsConfig {
    /// Maximum number of sessions returned by list operations (0 = unlimited).
    #[serde(default = "default_max_history")]
    pub max_history: usize,
    /// Maximum characters for auto-generated session titles.
    #[serde(default = "default_title_max_chars")]
    pub title_max_chars: usize,
}

impl Default for SessionsConfig {
    fn default() -> Self {
        Self {
            max_history: default_max_history(),
            title_max_chars: default_title_max_chars(),
        }
    }
}

/// Configuration for the document ingestion and RAG retrieval pipeline.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DocumentConfig {
    #[serde(default = "default_document_collection")]
    pub collection: String,
    #[serde(default = "default_document_chunk_size")]
    pub chunk_size: usize,
    #[serde(default = "default_document_chunk_overlap")]
    pub chunk_overlap: usize,
    /// Number of document chunks to inject into agent context per turn.
    #[serde(default = "default_document_top_k")]
    pub top_k: usize,
    /// Enable document RAG injection into agent context.
    #[serde(default)]
    pub rag_enabled: bool,
}

impl Default for DocumentConfig {
    fn default() -> Self {
        Self {
            collection: default_document_collection(),
            chunk_size: default_document_chunk_size(),
            chunk_overlap: default_document_chunk_overlap(),
            top_k: default_document_top_k(),
            rag_enabled: false,
        }
    }
}

/// Semantic (vector) memory retrieval configuration, nested under `[memory.semantic]` in TOML.
///
/// Controls how memories are searched and ranked, including temporal decay, MMR diversity
/// re-ranking, and hybrid BM25+vector weighting.
///
/// # Example (TOML)
///
/// ```toml
/// [memory.semantic]
/// enabled = true
/// recall_limit = 5
/// vector_weight = 0.7
/// keyword_weight = 0.3
/// mmr_lambda = 0.7
/// ```
#[derive(Debug, Deserialize, Serialize)]
#[allow(clippy::struct_excessive_bools)] // config struct — boolean flags are idiomatic for TOML-deserialized configuration
pub struct SemanticConfig {
    /// Enable vector-based semantic recall. Default: `true`.
    #[serde(default = "default_semantic_enabled")]
    pub enabled: bool,
    #[serde(default = "default_recall_limit")]
    pub recall_limit: usize,
    #[serde(default = "default_vector_weight")]
    pub vector_weight: f64,
    #[serde(default = "default_keyword_weight")]
    pub keyword_weight: f64,
    #[serde(default = "default_true")]
    pub temporal_decay_enabled: bool,
    #[serde(default = "default_temporal_decay_half_life_days")]
    pub temporal_decay_half_life_days: u32,
    #[serde(default = "default_true")]
    pub mmr_enabled: bool,
    #[serde(default = "default_mmr_lambda")]
    pub mmr_lambda: f32,
    #[serde(default = "default_true")]
    pub importance_enabled: bool,
    #[serde(
        default = "default_importance_weight",
        deserialize_with = "validate_importance_weight"
    )]
    pub importance_weight: f64,
    /// Name of a `[[llm.providers]]` entry to use exclusively for embedding calls during
    /// memory write and backfill operations. A dedicated provider prevents `embed_backfill`
    /// from contending with the guardrail at the API server level (rate limits, Ollama
    /// single-model lock). Falls back to the main agent provider when `None`.
    #[serde(default)]
    pub embedding_provider: Option<ProviderName>,
    /// Timeout in seconds applied to every `embed()` call inside `zeph-memory`.
    ///
    /// Applies to all embedding call sites: admission control, quality gate, recall,
    /// summarization, graph retrieval, consolidation, and tree consolidation.
    /// Set to a higher value when using slow remote embedding providers.
    /// Default: `5`.
    #[serde(default = "default_embed_timeout_secs")]
    pub embed_timeout_secs: u64,
}

impl Default for SemanticConfig {
    fn default() -> Self {
        Self {
            enabled: default_semantic_enabled(),
            recall_limit: default_recall_limit(),
            vector_weight: default_vector_weight(),
            keyword_weight: default_keyword_weight(),
            temporal_decay_enabled: true,
            temporal_decay_half_life_days: default_temporal_decay_half_life_days(),
            mmr_enabled: true,
            mmr_lambda: default_mmr_lambda(),
            importance_enabled: true,
            importance_weight: default_importance_weight(),
            embedding_provider: None,
            embed_timeout_secs: default_embed_timeout_secs(),
        }
    }
}
