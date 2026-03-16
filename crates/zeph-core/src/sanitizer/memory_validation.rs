// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Memory write validation: structural checks before content reaches the memory store
//! or the graph extractor.
//!
//! Configured under `[security.memory_validation]` in the agent config file.
//! Enabled by default — guards against oversized writes, injection markers, and PII
//! leaking into entity names.

use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use zeph_memory::graph::extractor::ExtractionResult;

// ---------------------------------------------------------------------------
// PII patterns for entity name scanning (subset — email and SSN only)
// ---------------------------------------------------------------------------

/// Email pattern kept in sync with `pii.rs`: domain labels must be purely alphabetic.
static ENTITY_EMAIL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[a-zA-Z0-9._%+\-]{2,}@(?:[a-zA-Z]+\.)+[a-zA-Z]{2,6}")
        .expect("valid ENTITY_EMAIL_RE")
});

/// SSN pattern for entity name scanning.
static ENTITY_SSN_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b\d{3}-\d{2}-\d{4}\b").expect("valid ENTITY_SSN_RE"));

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

/// Validation failure reported by [`MemoryWriteValidator`].
#[derive(Debug, Error)]
pub enum MemoryValidationError {
    #[error("content too large: {size} bytes exceeds max {max}")]
    ContentTooLarge { size: usize, max: usize },

    #[error("entity name too short: '{name}' is below min {min} bytes")]
    EntityNameTooShort { name: String, min: usize },

    #[error("entity name too long: '{name}' exceeds max {max} bytes")]
    EntityNameTooLong { name: String, max: usize },

    #[error("fact text too long: exceeds max {max} bytes")]
    FactTooLong { max: usize },

    #[error("too many entities: {count} exceeds max {max}")]
    TooManyEntities { count: usize, max: usize },

    #[error("too many edges: {count} exceeds max {max}")]
    TooManyEdges { count: usize, max: usize },

    #[error("forbidden pattern detected: {pattern}")]
    ForbiddenPattern { pattern: String },

    #[error("PII detected in entity name: '{entity}'")]
    SuspiciousPiiInEntityName { entity: String },
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

fn default_true() -> bool {
    true
}

fn default_max_content_bytes() -> usize {
    4096
}

fn default_max_entity_name_bytes() -> usize {
    256
}

fn default_min_entity_name_bytes() -> usize {
    3
}

fn default_max_fact_bytes() -> usize {
    1024
}

fn default_max_entities() -> usize {
    50
}

fn default_max_edges() -> usize {
    100
}

/// Configuration for memory write validation, nested under `[security.memory_validation]`.
///
/// Enabled by default with conservative limits. All values correspond to existing
/// capacity constraints already enforced elsewhere; the validator makes them explicit
/// and configurable.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct MemoryWriteValidationConfig {
    /// Master switch. When `false`, validation is a no-op.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Maximum byte length of content passed to `memory_save`.
    #[serde(default = "default_max_content_bytes")]
    pub max_content_bytes: usize,
    /// Minimum byte length of an entity name in graph extraction.
    /// Names shorter than this are rejected as noise (e.g. "go", "cd").
    #[serde(default = "default_min_entity_name_bytes")]
    pub min_entity_name_bytes: usize,
    /// Maximum byte length of a single entity name in graph extraction.
    #[serde(default = "default_max_entity_name_bytes")]
    pub max_entity_name_bytes: usize,
    /// Maximum byte length of an edge fact string in graph extraction.
    #[serde(default = "default_max_fact_bytes")]
    pub max_fact_bytes: usize,
    /// Maximum number of entities allowed per graph extraction result.
    #[serde(default = "default_max_entities")]
    pub max_entities_per_extraction: usize,
    /// Maximum number of edges allowed per graph extraction result.
    #[serde(default = "default_max_edges")]
    pub max_edges_per_extraction: usize,
    /// Forbidden substring patterns. Content containing any of these is rejected.
    /// Default empty — users can add custom patterns (e.g., `"<script"`, `"javascript:"`).
    #[serde(default)]
    pub forbidden_content_patterns: Vec<String>,
}

impl Default for MemoryWriteValidationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_content_bytes: default_max_content_bytes(),
            min_entity_name_bytes: default_min_entity_name_bytes(),
            max_entity_name_bytes: default_max_entity_name_bytes(),
            max_fact_bytes: default_max_fact_bytes(),
            max_entities_per_extraction: default_max_entities(),
            max_edges_per_extraction: default_max_edges(),
            forbidden_content_patterns: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Validator
// ---------------------------------------------------------------------------

/// Validates content before it is written to the memory store or graph extractor.
///
/// Construct once from [`MemoryWriteValidationConfig`] and store on the agent.
/// Cheap to clone.
#[derive(Debug, Clone)]
pub struct MemoryWriteValidator {
    config: MemoryWriteValidationConfig,
}

impl MemoryWriteValidator {
    /// Create a validator from the given configuration.
    #[must_use]
    pub fn new(config: MemoryWriteValidationConfig) -> Self {
        Self { config }
    }

    /// Validate content before it is written via the `memory_save` tool.
    ///
    /// # Errors
    ///
    /// Returns [`MemoryValidationError`] if any validation check fails.
    pub fn validate_memory_save(&self, content: &str) -> Result<(), MemoryValidationError> {
        if !self.config.enabled {
            return Ok(());
        }

        let size = content.len();
        if size > self.config.max_content_bytes {
            return Err(MemoryValidationError::ContentTooLarge {
                size,
                max: self.config.max_content_bytes,
            });
        }

        for pattern in &self.config.forbidden_content_patterns {
            if content.contains(pattern.as_str()) {
                return Err(MemoryValidationError::ForbiddenPattern {
                    pattern: pattern.clone(),
                });
            }
        }

        Ok(())
    }

    /// Validate a graph extraction result before entities and edges are upserted.
    ///
    /// Called inside the spawned extraction task, after `GraphExtractor::extract()` returns.
    ///
    /// # Errors
    ///
    /// Returns [`MemoryValidationError`] if any validation check fails.
    pub fn validate_graph_extraction(
        &self,
        result: &ExtractionResult,
    ) -> Result<(), MemoryValidationError> {
        if !self.config.enabled {
            return Ok(());
        }

        let entity_count = result.entities.len();
        if entity_count > self.config.max_entities_per_extraction {
            return Err(MemoryValidationError::TooManyEntities {
                count: entity_count,
                max: self.config.max_entities_per_extraction,
            });
        }

        let edge_count = result.edges.len();
        if edge_count > self.config.max_edges_per_extraction {
            return Err(MemoryValidationError::TooManyEdges {
                count: edge_count,
                max: self.config.max_edges_per_extraction,
            });
        }

        for entity in &result.entities {
            // Trim before length checks: both min and max apply to the trimmed form
            // to avoid rejecting names with leading/trailing whitespace.
            let name_len = entity.name.trim().len();
            if name_len < self.config.min_entity_name_bytes {
                return Err(MemoryValidationError::EntityNameTooShort {
                    name: entity.name.clone(),
                    min: self.config.min_entity_name_bytes,
                });
            }
            if name_len > self.config.max_entity_name_bytes {
                return Err(MemoryValidationError::EntityNameTooLong {
                    name: entity.name.clone(),
                    max: self.config.max_entity_name_bytes,
                });
            }
            // Guard against PII leaking into entity names (email and SSN).
            if ENTITY_EMAIL_RE.is_match(&entity.name) || ENTITY_SSN_RE.is_match(&entity.name) {
                return Err(MemoryValidationError::SuspiciousPiiInEntityName {
                    entity: entity.name.clone(),
                });
            }
        }

        for edge in &result.edges {
            let fact_len = edge.fact.len();
            if fact_len > self.config.max_fact_bytes {
                return Err(MemoryValidationError::FactTooLong {
                    max: self.config.max_fact_bytes,
                });
            }
        }

        Ok(())
    }

    /// Returns `true` when validation is enabled.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.config.enabled
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use zeph_memory::graph::extractor::{ExtractedEdge, ExtractedEntity};

    use super::*;

    fn validator() -> MemoryWriteValidator {
        MemoryWriteValidator::new(MemoryWriteValidationConfig::default())
    }

    fn validator_disabled() -> MemoryWriteValidator {
        MemoryWriteValidator::new(MemoryWriteValidationConfig {
            enabled: false,
            ..MemoryWriteValidationConfig::default()
        })
    }

    fn entity(name: &str) -> ExtractedEntity {
        ExtractedEntity {
            name: name.to_owned(),
            entity_type: "person".to_owned(),
            summary: None,
        }
    }

    fn edge(fact: &str) -> ExtractedEdge {
        ExtractedEdge {
            source: "A".to_owned(),
            target: "B".to_owned(),
            relation: "knows".to_owned(),
            fact: fact.to_owned(),
            temporal_hint: None,
        }
    }

    fn result_with(entities: Vec<ExtractedEntity>, edges: Vec<ExtractedEdge>) -> ExtractionResult {
        ExtractionResult { entities, edges }
    }

    // --- memory_save validation ---

    #[test]
    fn valid_content_passes() {
        assert!(validator().validate_memory_save("hello world").is_ok());
    }

    #[test]
    fn oversized_content_rejected() {
        let big = "x".repeat(5000);
        let err = validator().validate_memory_save(&big).unwrap_err();
        assert!(matches!(err, MemoryValidationError::ContentTooLarge { .. }));
    }

    #[test]
    fn forbidden_pattern_rejected() {
        let v = MemoryWriteValidator::new(MemoryWriteValidationConfig {
            forbidden_content_patterns: vec!["<script".to_owned()],
            ..MemoryWriteValidationConfig::default()
        });
        let err = v
            .validate_memory_save("text <script>alert(1)</script>")
            .unwrap_err();
        assert!(matches!(
            err,
            MemoryValidationError::ForbiddenPattern { .. }
        ));
    }

    #[test]
    fn disabled_skips_validation() {
        let big = "x".repeat(9999);
        assert!(validator_disabled().validate_memory_save(&big).is_ok());
    }

    // --- graph extraction validation ---

    #[test]
    fn valid_extraction_passes() {
        let r = result_with(vec![entity("Rust"), entity("Alice")], vec![edge("fact")]);
        assert!(validator().validate_graph_extraction(&r).is_ok());
    }

    #[test]
    fn too_many_entities_rejected() {
        let v = MemoryWriteValidator::new(MemoryWriteValidationConfig {
            max_entities_per_extraction: 2,
            ..MemoryWriteValidationConfig::default()
        });
        let r = result_with(vec![entity("Abc"), entity("Def"), entity("Ghi")], vec![]);
        let err = v.validate_graph_extraction(&r).unwrap_err();
        assert!(matches!(err, MemoryValidationError::TooManyEntities { .. }));
    }

    #[test]
    fn too_many_edges_rejected() {
        let v = MemoryWriteValidator::new(MemoryWriteValidationConfig {
            max_edges_per_extraction: 1,
            ..MemoryWriteValidationConfig::default()
        });
        let r = result_with(vec![], vec![edge("a"), edge("b")]);
        let err = v.validate_graph_extraction(&r).unwrap_err();
        assert!(matches!(err, MemoryValidationError::TooManyEdges { .. }));
    }

    #[test]
    fn entity_name_too_long_rejected() {
        let v = MemoryWriteValidator::new(MemoryWriteValidationConfig {
            max_entity_name_bytes: 5,
            ..MemoryWriteValidationConfig::default()
        });
        let r = result_with(vec![entity("TooLongName")], vec![]);
        let err = v.validate_graph_extraction(&r).unwrap_err();
        assert!(matches!(
            err,
            MemoryValidationError::EntityNameTooLong { .. }
        ));
    }

    #[test]
    fn fact_too_long_rejected() {
        let v = MemoryWriteValidator::new(MemoryWriteValidationConfig {
            max_fact_bytes: 10,
            ..MemoryWriteValidationConfig::default()
        });
        let r = result_with(vec![], vec![edge("this fact is longer than ten chars")]);
        let err = v.validate_graph_extraction(&r).unwrap_err();
        assert!(matches!(err, MemoryValidationError::FactTooLong { .. }));
    }

    #[test]
    fn email_in_entity_name_rejected() {
        let r = result_with(vec![entity("user@example.com")], vec![]);
        let err = validator().validate_graph_extraction(&r).unwrap_err();
        assert!(matches!(
            err,
            MemoryValidationError::SuspiciousPiiInEntityName { .. }
        ));
    }

    #[test]
    fn ssn_in_entity_name_rejected() {
        let r = result_with(vec![entity("123-45-6789")], vec![]);
        let err = validator().validate_graph_extraction(&r).unwrap_err();
        assert!(matches!(
            err,
            MemoryValidationError::SuspiciousPiiInEntityName { .. }
        ));
    }

    #[test]
    fn disabled_skips_graph_validation() {
        let v = validator_disabled();
        let big_entities: Vec<_> = (0..200).map(|i| entity(&format!("E{i}"))).collect();
        let r = result_with(big_entities, vec![]);
        assert!(v.validate_graph_extraction(&r).is_ok());
    }

    // --- exact boundary: max_content_bytes ---

    #[test]
    fn content_exactly_at_limit_passes() {
        let v = MemoryWriteValidator::new(MemoryWriteValidationConfig {
            max_content_bytes: 10,
            ..MemoryWriteValidationConfig::default()
        });
        // Exactly 10 bytes — must pass.
        assert!(v.validate_memory_save("1234567890").is_ok());
    }

    #[test]
    fn content_one_byte_over_limit_rejected() {
        let v = MemoryWriteValidator::new(MemoryWriteValidationConfig {
            max_content_bytes: 10,
            ..MemoryWriteValidationConfig::default()
        });
        // 11 bytes — must fail.
        let err = v.validate_memory_save("12345678901").unwrap_err();
        assert!(matches!(err, MemoryValidationError::ContentTooLarge { .. }));
    }

    // --- multiple forbidden patterns: first match blocks ---

    #[test]
    fn multiple_forbidden_patterns_first_match_blocks() {
        let v = MemoryWriteValidator::new(MemoryWriteValidationConfig {
            forbidden_content_patterns: vec!["<script".to_owned(), "javascript:".to_owned()],
            ..MemoryWriteValidationConfig::default()
        });
        let err = v.validate_memory_save("javascript:alert(1)").unwrap_err();
        assert!(matches!(
            err,
            MemoryValidationError::ForbiddenPattern { .. }
        ));
    }

    #[test]
    fn content_without_forbidden_pattern_passes() {
        let v = MemoryWriteValidator::new(MemoryWriteValidationConfig {
            forbidden_content_patterns: vec!["<script".to_owned()],
            ..MemoryWriteValidationConfig::default()
        });
        assert!(v.validate_memory_save("safe content here").is_ok());
    }

    // --- is_enabled ---

    #[test]
    fn is_enabled_true_by_default() {
        assert!(validator().is_enabled());
    }

    #[test]
    fn is_enabled_false_when_disabled() {
        assert!(!validator_disabled().is_enabled());
    }

    // --- empty ExtractionResult passes ---

    #[test]
    fn empty_extraction_passes() {
        let r = result_with(vec![], vec![]);
        assert!(validator().validate_graph_extraction(&r).is_ok());
    }

    // --- exact boundary: entity name ---

    #[test]
    fn entity_name_exactly_at_limit_passes() {
        let v = MemoryWriteValidator::new(MemoryWriteValidationConfig {
            max_entity_name_bytes: 5,
            ..MemoryWriteValidationConfig::default()
        });
        let r = result_with(vec![entity("Alice")], vec![]); // 5 bytes exactly
        assert!(v.validate_graph_extraction(&r).is_ok());
    }

    #[test]
    fn entity_name_one_byte_over_limit_rejected() {
        let v = MemoryWriteValidator::new(MemoryWriteValidationConfig {
            max_entity_name_bytes: 5,
            ..MemoryWriteValidationConfig::default()
        });
        let r = result_with(vec![entity("AliceX")], vec![]); // 6 bytes
        let err = v.validate_graph_extraction(&r).unwrap_err();
        assert!(matches!(
            err,
            MemoryValidationError::EntityNameTooLong { .. }
        ));
    }

    // --- min entity name length (FIX-3) ---

    #[test]
    fn entity_name_below_min_rejected() {
        let r = result_with(vec![entity("go")], vec![]);
        let err = validator().validate_graph_extraction(&r).unwrap_err();
        assert!(matches!(
            err,
            MemoryValidationError::EntityNameTooShort { .. }
        ));
    }

    #[test]
    fn entity_name_at_min_passes() {
        let r = result_with(vec![entity("git")], vec![]);
        assert!(validator().validate_graph_extraction(&r).is_ok());
    }

    // --- exact boundary: entities count ---

    #[test]
    fn entities_exactly_at_limit_passes() {
        let v = MemoryWriteValidator::new(MemoryWriteValidationConfig {
            max_entities_per_extraction: 3,
            ..MemoryWriteValidationConfig::default()
        });
        let r = result_with(vec![entity("Abc"), entity("Def"), entity("Ghi")], vec![]);
        assert!(v.validate_graph_extraction(&r).is_ok());
    }

    // --- error message content ---

    #[test]
    fn content_too_large_error_message() {
        let big = "x".repeat(5000);
        let err = validator().validate_memory_save(&big).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("5000"), "error must include actual size");
        assert!(msg.contains("4096"), "error must include max size");
    }
}
