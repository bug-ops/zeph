// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Proactive world-knowledge exploration (#3320).
//!
//! [`ProactiveExplorer`] classifies incoming queries against a keyword map of recognisable
//! technology domains and, for domains with no existing SKILL.md, generates one. The skill
//! is written to disk and registered in the [`SkillRegistry`] immediately, but becomes
//! **visible to [`crate::matcher::SkillMatcher`]** only on the next turn — this is an intentional MVP
//! trade-off that avoids an expensive synchronous re-embed on the hot path.
//!
//! # Domain keyword map (MVP)
//!
//! The classifier uses a static keyword → domain table. Any query word that is an exact
//! lowercase match for an entry in the table produces a [`DomainLabel`]. Only the first
//! matching domain is returned; no disambiguation is attempted.
//!
//! # Evaluator gate
//!
//! When constructed with an `Option<Arc<SkillEvaluator>>`, each generated skill is scored
//! before being written to disk. On evaluator rejection the method returns `Ok(())` with
//! a `tracing::info!` log — rejection is a normal outcome, not a fault.

use std::path::PathBuf;
use std::sync::Arc;

use crate::error::SkillError;
use crate::evaluator::{SkillEvaluationRequest, SkillEvaluator, SkillVerdict};
use crate::generator::{SkillGenerationRequest, SkillGenerator};
use crate::registry::SkillRegistry;

/// Keyword → domain mapping used by [`ProactiveExplorer::classify`].
///
/// Each entry is `(keyword, domain_slug)`. Keyword matching is case-insensitive
/// and performed on whitespace-separated tokens in the query.
static DOMAIN_KEYWORDS: &[(&str, &str)] = &[
    ("rust", "rust"),
    ("python", "python"),
    ("docker", "docker"),
    ("git", "git"),
    ("sql", "sql"),
    ("http", "http"),
    ("kubernetes", "kubernetes"),
    ("k8s", "kubernetes"),
    ("typescript", "typescript"),
    ("go", "go"),
    ("golang", "go"),
    ("terraform", "terraform"),
    ("react", "react"),
    ("postgres", "postgres"),
    ("postgresql", "postgres"),
    ("bash", "bash"),
    ("shell", "bash"),
    ("yaml", "yaml"),
    ("json", "json"),
    ("toml", "toml"),
    ("grpc", "grpc"),
    ("redis", "redis"),
    ("kafka", "kafka"),
    ("aws", "aws"),
    ("gcp", "gcp"),
    ("azure", "azure"),
];

/// A canonical domain identifier produced by [`ProactiveExplorer::classify`].
///
/// Wraps a lowercase slug like `"rust"` or `"kubernetes"`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DomainLabel(pub String);

impl DomainLabel {
    /// Return the canonical skill name for this domain: `"world-knowledge-{slug}"`.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_skills::proactive::DomainLabel;
    /// let d = DomainLabel("rust".into());
    /// assert_eq!(d.to_skill_name(), "world-knowledge-rust");
    /// ```
    #[must_use]
    pub fn to_skill_name(&self) -> String {
        format!("world-knowledge-{}", self.0)
    }
}

/// Classifies queries and generates world-knowledge SKILL.md files on demand.
///
/// Constructed by the agent builder when
/// `config.skills.proactive_exploration.enabled = true`. Attach an evaluator via the
/// constructor to apply the quality gate (Feature B, #3319) to generated skills.
///
/// # Examples
///
/// ```rust,no_run
/// use std::path::PathBuf;
/// use std::sync::Arc;
/// use zeph_skills::proactive::ProactiveExplorer;
/// use zeph_skills::generator::SkillGenerator;
///
/// # async fn demo(provider: zeph_llm::any::AnyProvider, registry: &zeph_skills::registry::SkillRegistry) {
/// let generator = SkillGenerator::new(provider, PathBuf::from("/tmp/skills"));
/// let explorer = ProactiveExplorer::new(generator, None, PathBuf::from("/tmp/skills"), 8_000, 30_000, vec![]);
/// if let Some(domain) = explorer.classify("how do I use docker volumes?") {
///     if !explorer.has_knowledge(registry, &domain) {
///         explorer.explore(&domain).await.ok();
///     }
/// }
/// # }
/// ```
pub struct ProactiveExplorer {
    generator: SkillGenerator,
    evaluator: Option<Arc<SkillEvaluator>>,
    output_dir: PathBuf,
    max_chars: usize,
    timeout_ms: u64,
    excluded_domains: Vec<String>,
}

impl ProactiveExplorer {
    /// Create a new explorer.
    ///
    /// - `generator`: drives SKILL.md generation.
    /// - `evaluator`: optional quality gate (Feature B).
    /// - `output_dir`: where generated skills are written.
    /// - `max_chars`: approximate target size hint passed in the generation prompt.
    /// - `timeout_ms`: per-exploration timeout covering the full generate → write path.
    /// - `excluded_domains`: domain slugs to skip (e.g. `["rust"]`).
    #[must_use]
    pub fn new(
        generator: SkillGenerator,
        evaluator: Option<Arc<SkillEvaluator>>,
        output_dir: PathBuf,
        max_chars: usize,
        timeout_ms: u64,
        excluded_domains: Vec<String>,
    ) -> Self {
        Self {
            generator,
            evaluator,
            output_dir,
            max_chars,
            timeout_ms,
            excluded_domains,
        }
    }

    /// Expose the configured timeout so callers can set `tokio::time::timeout` correctly.
    #[must_use]
    pub fn timeout_ms(&self) -> u64 {
        self.timeout_ms
    }

    /// Classify `query` against the keyword map.
    ///
    /// Returns `None` when no keyword in the query matches a known domain.
    /// Returns the first matching [`DomainLabel`] otherwise.
    #[tracing::instrument(name = "core.proactive.classify", skip_all)]
    pub fn classify(&self, query: &str) -> Option<DomainLabel> {
        let lower = query.to_lowercase();
        for token in lower.split_whitespace() {
            // Strip trailing punctuation from tokens.
            let token = token.trim_end_matches(|c: char| !c.is_alphanumeric());
            for &(keyword, domain) in DOMAIN_KEYWORDS {
                if token == keyword {
                    return Some(DomainLabel(domain.to_string()));
                }
            }
        }
        None
    }

    /// Return `true` if the registry already contains a skill for `domain`.
    #[must_use]
    pub fn has_knowledge(&self, registry: &SkillRegistry, domain: &DomainLabel) -> bool {
        let name = domain.to_skill_name();
        registry.all_meta().iter().any(|m| m.name == name)
    }

    /// Return `true` if `domain` is in the configured exclusion list.
    #[must_use]
    pub fn is_excluded(&self, domain: &DomainLabel) -> bool {
        self.excluded_domains.iter().any(|e| e == &domain.0)
    }

    /// Generate and persist a SKILL.md for `domain`.
    ///
    /// Applies the evaluator gate when configured. On evaluator rejection returns
    /// `Ok(())` with an info-level log — rejection is not an error.
    ///
    /// # Errors
    ///
    /// Returns [`SkillError`] if SKILL.md generation or the filesystem write fails.
    #[tracing::instrument(name = "core.proactive.explore", skip_all, fields(domain = %domain.0))]
    pub async fn explore(&self, domain: &DomainLabel) -> Result<(), SkillError> {
        let description = format!(
            "World-knowledge reference skill for {domain}. \
             Provide concise, authoritative quick-reference information about {domain}: \
             key commands, idioms, and best practices. Keep the body under {max_chars} characters.",
            domain = domain.0,
            max_chars = self.max_chars,
        );

        let req = SkillGenerationRequest {
            description: description.clone(),
            category: Some("dev".into()),
            allowed_tools: vec![],
        };

        let skill = self.generator.generate(req).await?;

        // Evaluator gate (S3 fix — see arch spec §2.3).
        if let Some(ref evaluator) = self.evaluator {
            let eval_req = SkillEvaluationRequest {
                name: &skill.name,
                description: &skill.meta.description,
                body: &skill.content,
                original_intent: &description,
            };
            match evaluator.evaluate(&eval_req).await? {
                SkillVerdict::Accept(_) | SkillVerdict::AcceptOnEvalError(_) => {}
                SkillVerdict::Reject { score: _, reason } => {
                    tracing::info!(
                        domain = %domain.0,
                        %reason,
                        "proactive skill rejected by evaluator — skipping write"
                    );
                    return Ok(());
                }
            }
        }

        // Write SKILL.md to disk. Skip if already exists (idempotent).
        let skill_dir = self.output_dir.join(&skill.name);
        if skill_dir.exists() {
            tracing::debug!(
                domain = %domain.0,
                skill = %skill.name,
                "proactive skill already exists, skipping"
            );
            return Ok(());
        }
        tokio::fs::create_dir_all(&skill_dir).await?;
        let skill_path = skill_dir.join("SKILL.md");
        tokio::fs::write(&skill_path, &skill.content).await?;
        tracing::info!(
            domain = %domain.0,
            skill = %skill.name,
            path = %skill_path.display(),
            "proactive skill written to disk"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_rust_query() {
        let generator = SkillGenerator::new(
            zeph_llm::any::AnyProvider::Mock(zeph_llm::mock::MockProvider::default()),
            PathBuf::from("/tmp"),
        );
        let explorer = ProactiveExplorer::new(
            generator,
            None,
            PathBuf::from("/tmp"),
            8_000,
            30_000,
            vec![],
        );

        let label = explorer.classify("how do I use rust async");
        assert_eq!(label, Some(DomainLabel("rust".into())));
    }

    #[test]
    fn classify_returns_none_for_unknown_domain() {
        let generator = SkillGenerator::new(
            zeph_llm::any::AnyProvider::Mock(zeph_llm::mock::MockProvider::default()),
            PathBuf::from("/tmp"),
        );
        let explorer = ProactiveExplorer::new(
            generator,
            None,
            PathBuf::from("/tmp"),
            8_000,
            30_000,
            vec![],
        );

        assert_eq!(explorer.classify("how are you today"), None);
    }

    #[test]
    fn classify_docker_with_punctuation() {
        let generator = SkillGenerator::new(
            zeph_llm::any::AnyProvider::Mock(zeph_llm::mock::MockProvider::default()),
            PathBuf::from("/tmp"),
        );
        let explorer = ProactiveExplorer::new(
            generator,
            None,
            PathBuf::from("/tmp"),
            8_000,
            30_000,
            vec![],
        );

        // Token "docker," with trailing comma — should still match.
        let label = explorer.classify("docker, how do I mount volumes?");
        assert_eq!(label, Some(DomainLabel("docker".into())));
    }

    #[test]
    fn is_excluded_matches_configured_domains() {
        let generator = SkillGenerator::new(
            zeph_llm::any::AnyProvider::Mock(zeph_llm::mock::MockProvider::default()),
            PathBuf::from("/tmp"),
        );
        let explorer = ProactiveExplorer::new(
            generator,
            None,
            PathBuf::from("/tmp"),
            8_000,
            30_000,
            vec!["rust".into(), "go".into()],
        );

        assert!(explorer.is_excluded(&DomainLabel("rust".into())));
        assert!(explorer.is_excluded(&DomainLabel("go".into())));
        assert!(!explorer.is_excluded(&DomainLabel("python".into())));
    }

    #[test]
    fn domain_label_to_skill_name() {
        assert_eq!(
            DomainLabel("rust".into()).to_skill_name(),
            "world-knowledge-rust"
        );
        assert_eq!(
            DomainLabel("kubernetes".into()).to_skill_name(),
            "world-knowledge-kubernetes"
        );
    }

    #[test]
    fn has_knowledge_empty_registry() {
        let registry = SkillRegistry::load(&[] as &[std::path::PathBuf]);
        let generator = SkillGenerator::new(
            zeph_llm::any::AnyProvider::Mock(zeph_llm::mock::MockProvider::default()),
            PathBuf::from("/tmp"),
        );
        let explorer = ProactiveExplorer::new(
            generator,
            None,
            PathBuf::from("/tmp"),
            8_000,
            30_000,
            vec![],
        );

        assert!(!explorer.has_knowledge(&registry, &DomainLabel("rust".into())));
    }
}
