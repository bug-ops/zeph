// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Shared trust-gating pipeline for skill-body tool executors.
//!
//! [`SkillTrustGate`] is the single implementation of the trust pipeline documented in
//! [`crate::skill_invoker`]: refuse `Blocked` skills before any body read, run the optional
//! per-invocation blake3 integrity re-check, sanitize non-Trusted bodies, and wrap Quarantined
//! bodies. Both `load_skill` ([`crate::skill_loader::SkillLoaderExecutor`]) and `invoke_skill`
//! ([`crate::skill_invoker::SkillInvokeExecutor`]) hold their own `SkillTrustGate` built from
//! the *same* `trust_snapshot` `Arc` (see `agent_setup::build_skill_executors` in the binary
//! crate), so the two tools cannot drift apart and observe identical trust state within a turn
//! (#6049, #6050).

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;
use zeph_common::SkillTrustLevel;
use zeph_skills::prompt::{sanitize_skill_text, wrap_quarantined};
use zeph_skills::registry::SkillRegistry;
use zeph_skills::trust::compute_skill_hash;
use zeph_tools::executor::ToolError;

use crate::skill_invoker::SkillTrustSnapshot;

/// Outcome of resolving a skill body through [`SkillTrustGate::resolve_body`].
///
/// Callers match on this to apply their own tool-specific output framing (e.g. `invoke_skill`
/// appends an `<args>` block to `Body`) before truncating for the LLM.
pub(crate) enum SkillBodyResolution {
    /// Refused by policy (blocked) or a failed integrity check — ready-to-return tool summary.
    Refused(String),
    /// `skill_name` has no entry in the registry — ready-to-return tool summary.
    NotFound(String),
    /// Body resolved and gated (sanitized for non-Trusted, wrapped for Quarantined). Not yet
    /// truncated — callers append any additional framing first, then call
    /// [`truncate_tool_output`](zeph_tools::executor::truncate_tool_output).
    Body(String),
}

/// Shared registry + trust-snapshot pair backing both skill-body tool executors.
///
/// Cloning is cheap — both fields are `Arc`s. Construct one instance per executor from the same
/// `trust_snapshot` `Arc` so `load_skill` and `invoke_skill` see identical trust state within a
/// turn.
#[derive(Clone, Debug)]
pub(crate) struct SkillTrustGate {
    registry: Arc<RwLock<SkillRegistry>>,
    trust_snapshot: Arc<RwLock<HashMap<String, SkillTrustSnapshot>>>,
}

impl SkillTrustGate {
    pub(crate) fn new(
        registry: Arc<RwLock<SkillRegistry>>,
        trust_snapshot: Arc<RwLock<HashMap<String, SkillTrustSnapshot>>>,
    ) -> Self {
        Self {
            registry,
            trust_snapshot,
        }
    }

    /// Resolve the trust snapshot entry for a skill.
    ///
    /// Returns `None` when no row exists — [`resolve_body`](Self::resolve_body) treats absence
    /// as `SkillTrustLevel::MISSING_ENTRY_FALLBACK` (Trusted), not Quarantined.
    fn resolve_snapshot(&self, skill_name: &str) -> Option<SkillTrustSnapshot> {
        self.trust_snapshot.read().get(skill_name).cloned()
    }

    /// Run the per-invocation blake3 integrity check.
    ///
    /// Returns `Some(message)` when the invocation must be aborted (hash mismatch, empty stored
    /// hash, missing skill dir, or IO error). Returns `None` when the check passes and dispatch
    /// should proceed.
    async fn check_integrity(
        &self,
        skill_name: &str,
        skill_name_safe: &str,
        entry: &SkillTrustSnapshot,
    ) -> Result<Option<String>, ToolError> {
        if entry.blake3_hash.is_empty() {
            tracing::warn!(
                skill = %skill_name,
                "requires_trust_check is set but no stored hash found, aborting invocation"
            );
            return Ok(Some(format!(
                "skill integrity check failed: {skill_name_safe} \
                 — requires_trust_check is set but no stored hash found"
            )));
        }
        let stored_hash = entry.blake3_hash.clone();
        let skill_dir = {
            let guard = self.registry.read();
            guard.skill_dir(skill_name)
        };
        let Some(dir) = skill_dir else {
            tracing::warn!(
                skill = %skill_name,
                "requires_trust_check: skill_dir not found, aborting invocation"
            );
            return Ok(Some(format!(
                "skill integrity check failed: {skill_name_safe} — skill directory not found"
            )));
        };
        let current_hash = tokio::task::spawn_blocking(move || compute_skill_hash(&dir))
            .await
            .map_err(|e| ToolError::InvalidParams {
                message: format!("spawn_blocking join error: {e}"),
            })?;
        match current_hash {
            Ok(hash) if hash != stored_hash => {
                tracing::warn!(
                    skill = %skill_name,
                    "hash mismatch on per-invocation check, demoting to Quarantined"
                );
                self.trust_snapshot
                    .write()
                    .entry(skill_name.to_owned())
                    .and_modify(|e| e.trust_level = SkillTrustLevel::Quarantined);
                // TODO: persist demotion to trust store (#4293 follow-up)
                Ok(Some(format!(
                    "skill integrity check failed: {skill_name_safe} — demoted to Quarantined"
                )))
            }
            Err(e) => {
                tracing::warn!(
                    skill = %skill_name,
                    err = %e,
                    "failed to re-hash skill, aborting invocation"
                );
                Ok(Some(format!(
                    "skill integrity check failed: {skill_name_safe} — cannot read SKILL.md"
                )))
            }
            Ok(_) => Ok(None), // hash matches, proceed
        }
    }

    /// Resolve `skill_name` through the trust pipeline shared by `load_skill` and
    /// `invoke_skill`: refuse `Blocked` before any body read, re-check integrity when
    /// `requires_trust_check` is set, then sanitize/wrap the body per trust level.
    ///
    /// A missing trust-snapshot row resolves to `SkillTrustLevel::MISSING_ENTRY_FALLBACK`
    /// (Trusted) — "never classified", not "known untrusted". `skill_name` is sanitized before
    /// it appears in any returned message, including the not-found path.
    pub(crate) async fn resolve_body(
        &self,
        skill_name: &str,
    ) -> Result<SkillBodyResolution, ToolError> {
        let snapshot = self.resolve_snapshot(skill_name);
        let trust = snapshot
            .as_ref()
            .map_or(SkillTrustLevel::MISSING_ENTRY_FALLBACK, |s| s.trust_level);
        // Sanitize skill_name before it appears in any tool output: it originates from the LLM
        // and could carry injection markers (e.g. `<|im_start|>`).
        let skill_name_safe = sanitize_skill_text(skill_name);

        // Blocked skills are refused before any body read — executor defense layer.
        if trust == SkillTrustLevel::Blocked {
            return Ok(SkillBodyResolution::Refused(format!(
                "skill is blocked by policy: {skill_name_safe}"
            )));
        }

        // Per-invocation integrity check: re-hash SKILL.md when requires_trust_check is set.
        if let Some(entry) = snapshot.as_ref().filter(|s| s.requires_trust_check)
            && let Some(message) = self
                .check_integrity(skill_name, &skill_name_safe, entry)
                .await?
        {
            return Ok(SkillBodyResolution::Refused(message));
        }

        // Clone body out of the read guard before any further await — never hold lock across await.
        let body = {
            let guard = self.registry.read();
            guard.body(skill_name).map(str::to_owned)
        };

        match body {
            Ok(raw_body) => {
                // Apply the same pipeline as `format_skills_prompt`: sanitize for non-Trusted,
                // additionally wrap for Quarantined.
                let sanitized = if trust == SkillTrustLevel::Trusted {
                    raw_body
                } else {
                    sanitize_skill_text(&raw_body)
                };
                let wrapped = if trust == SkillTrustLevel::Quarantined {
                    wrap_quarantined(&skill_name_safe, &sanitized)
                } else {
                    sanitized
                };
                Ok(SkillBodyResolution::Body(wrapped))
            }
            Err(_) => Ok(SkillBodyResolution::NotFound(format!(
                "skill not found: {skill_name_safe}"
            ))),
        }
    }
}
