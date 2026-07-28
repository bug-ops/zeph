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
//!
//! [`SkillTrustGate`] and [`resolve_body`](SkillTrustGate::resolve_body) are also `pub` at the
//! crate root so the binary crate's `zeph skill invoke` CLI preview command can route through
//! the exact same pipeline instead of maintaining its own copy (#6079).

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;
use zeph_common::{SkillTrustLevel, TurnTrustFloor};
use zeph_skills::prompt::{sanitize_skill_text, wrap_quarantined};
use zeph_skills::registry::SkillRegistry;
use zeph_skills::trust::compute_skill_hash;
use zeph_tools::executor::ToolError;

use crate::skill_invoker::SkillTrustSnapshot;

/// Outcome of resolving a skill body through [`SkillTrustGate::resolve_body`].
///
/// Callers match on this to apply their own tool-specific output framing (e.g. `invoke_skill`
/// appends an `<args>` block to `Body`) before truncating for the LLM.
#[derive(Debug)]
pub enum SkillBodyResolution {
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
/// Cloning is cheap — all fields are `Arc`s (`TurnTrustFloor` wraps one internally).
/// Construct one instance per executor from the same `trust_snapshot` `Arc` (and the same
/// `turn_trust_floor`, when available) so `load_skill` and `invoke_skill` see identical
/// trust state within a turn.
#[derive(Clone, Debug)]
pub struct SkillTrustGate {
    registry: Arc<RwLock<SkillRegistry>>,
    trust_snapshot: Arc<RwLock<HashMap<String, SkillTrustSnapshot>>>,
    /// Shared per-turn trust floor (#6701), the same cell `TrustGateExecutor` reads. `None`
    /// in contexts that never wired one (e.g. the `zeph skill invoke` CLI preview, which has
    /// no live turn to degrade) — [`resolve_body`](Self::resolve_body) simply skips the fold
    /// in that case, since there is no subsequent tool dispatch this turn to protect.
    turn_trust_floor: Option<TurnTrustFloor>,
}

impl SkillTrustGate {
    /// Build a gate over `registry` and `trust_snapshot`, with no turn trust floor wired.
    ///
    /// Equivalent to [`with_turn_trust_floor`](Self::with_turn_trust_floor) with `None` —
    /// prefer that constructor when a live agent turn's floor is available so a Quarantined
    /// body read degrades the turn's trust (#6701, RC-3).
    ///
    /// # Examples
    ///
    /// ```
    /// use std::collections::HashMap;
    /// use std::sync::Arc;
    ///
    /// use parking_lot::RwLock;
    /// use zeph_core::SkillTrustGate;
    /// use zeph_skills::registry::SkillRegistry;
    ///
    /// let registry = Arc::new(RwLock::new(SkillRegistry::empty()));
    /// let trust_snapshot = Arc::new(RwLock::new(HashMap::new()));
    /// let _gate = SkillTrustGate::new(registry, trust_snapshot);
    /// ```
    pub fn new(
        registry: Arc<RwLock<SkillRegistry>>,
        trust_snapshot: Arc<RwLock<HashMap<String, SkillTrustSnapshot>>>,
    ) -> Self {
        Self {
            registry,
            trust_snapshot,
            turn_trust_floor: None,
        }
    }

    /// Build a gate over `registry` and `trust_snapshot`, wired to the given turn trust floor.
    ///
    /// `turn_trust_floor` should be the same handle `TrustGateExecutor::trust_floor()` returns
    /// for the live agent turn, so an explicit `invoke_skill`/`load_skill` of a Quarantined
    /// skill folds the turn's trust floor down (#6701, RC-3) instead of leaving the gate's
    /// weakest-link fold blind to bodies read outside the proactive-activation path.
    #[must_use]
    pub fn with_turn_trust_floor(mut self, turn_trust_floor: TurnTrustFloor) -> Self {
        self.turn_trust_floor = Some(turn_trust_floor);
        self
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
    ///
    /// # Errors
    ///
    /// Returns an error only when the `requires_trust_check` integrity re-check's
    /// `spawn_blocking` task panics or is cancelled — a hash mismatch, missing skill directory,
    /// or unreadable `SKILL.md` are reported as `Ok(SkillBodyResolution::Refused(_))`, not `Err`.
    ///
    /// # Examples
    ///
    /// ```
    /// # use std::collections::HashMap;
    /// # use std::sync::Arc;
    /// # use parking_lot::RwLock;
    /// # use zeph_core::{SkillBodyResolution, SkillTrustGate};
    /// # use zeph_skills::registry::SkillRegistry;
    /// # #[tokio::main] async fn main() {
    /// let registry = Arc::new(RwLock::new(SkillRegistry::empty()));
    /// let trust_snapshot = Arc::new(RwLock::new(HashMap::new()));
    /// let gate = SkillTrustGate::new(registry, trust_snapshot);
    ///
    /// match gate.resolve_body("nonexistent").await.unwrap() {
    ///     SkillBodyResolution::NotFound(message) => assert!(message.contains("nonexistent")),
    ///     _ => panic!("expected NotFound for an empty registry"),
    /// }
    /// # }
    /// ```
    pub async fn resolve_body(&self, skill_name: &str) -> Result<SkillBodyResolution, ToolError> {
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
                    // #6701 (RC-3): an explicit invoke_skill/load_skill of a Quarantined skill
                    // is allowed (see specs/005-skills/spec.md § Agent-Invocable Skills), but
                    // reading its body now degrades the turn's trust floor for the remainder
                    // of the turn — closing the gap where invocation previously degraded
                    // nothing. Monotonic: never raises trust, only ever lowers it.
                    if let Some(floor) = &self.turn_trust_floor {
                        floor.fold(SkillTrustLevel::Quarantined);
                    }
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

/// Single source of truth for the `requires_trust_check` arming decision made on promotion to
/// `Trusted`/`Verified` (#6087).
///
/// `force_on` (`--require-check`) always wins; otherwise `force_off` (`--no-require-check`)
/// wins; otherwise falls back to `config_default`
/// (`[skills.trust] require_integrity_check_on_promote`). Used identically by the CLI
/// (`zeph skill trust`, binary crate) and in-session (`/skill trust`,
/// `crate::agent::trust_commands`) promotion handlers — both already gate the call on
/// `matches!(level, SkillTrustLevel::Trusted | SkillTrustLevel::Verified)` before consulting
/// this function; promotion to `Quarantined`/`Blocked` must never call it.
///
/// # Examples
///
/// ```
/// use zeph_core::resolve_require_check;
///
/// assert!(resolve_require_check(true, true, false), "force_on always wins");
/// assert!(!resolve_require_check(false, true, true), "force_off wins over the default");
/// assert!(resolve_require_check(false, false, true), "falls back to the config default");
/// ```
#[must_use]
pub fn resolve_require_check(force_on: bool, force_off: bool, config_default: bool) -> bool {
    if force_on {
        true
    } else if force_off {
        false
    } else {
        config_default
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use zeph_skills::trust::compute_skill_hash;

    use super::*;

    fn make_registry_with_skill(dir: &Path, name: &str, body: &str) -> SkillRegistry {
        let skill_dir = dir.join(name);
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: test skill\n---\n{body}"),
        )
        .unwrap();
        SkillRegistry::load(&[dir.to_path_buf()])
    }

    fn make_gate(
        registry: SkillRegistry,
        snapshots: HashMap<String, SkillTrustSnapshot>,
    ) -> SkillTrustGate {
        SkillTrustGate::new(
            Arc::new(RwLock::new(registry)),
            Arc::new(RwLock::new(snapshots)),
        )
    }

    // Exercises the gate directly (bypassing `SkillLoaderExecutor`/`SkillInvokeExecutor`) to
    // guard the pipeline the CLI's `zeph skill invoke` now shares with the agent tools (#6079).

    #[tokio::test]
    async fn blocked_skill_refused_without_body_read() {
        let dir = tempfile::tempdir().unwrap();
        let body = "secret body that must not leak";
        let registry = make_registry_with_skill(dir.path(), "blocked-skill", body);
        let snapshots = HashMap::from([(
            "blocked-skill".to_owned(),
            SkillTrustSnapshot {
                trust_level: SkillTrustLevel::Blocked,
                requires_trust_check: false,
                blake3_hash: String::new(),
            },
        )]);
        let gate = make_gate(registry, snapshots);
        match gate.resolve_body("blocked-skill").await.unwrap() {
            SkillBodyResolution::Refused(message) => {
                assert!(message.contains("blocked by policy"));
                assert!(!message.contains("secret body"));
            }
            other => panic!("expected Refused, got a different variant: {other:?}"),
        }
    }

    #[tokio::test]
    async fn not_found_sanitizes_skill_name() {
        let dir = tempfile::tempdir().unwrap();
        let registry = SkillRegistry::load(&[dir.path().to_path_buf()]);
        let gate = make_gate(registry, HashMap::new());
        match gate.resolve_body("<|im_start|>nonexistent").await.unwrap() {
            SkillBodyResolution::NotFound(message) => {
                assert!(message.contains("skill not found"));
                assert!(message.contains("[BLOCKED:<|im_start|>]"));
                assert!(
                    !message
                        .replace("[BLOCKED:<|im_start|>]", "")
                        .contains("<|im_start|>")
                );
            }
            other => panic!("expected NotFound, got a different variant: {other:?}"),
        }
    }

    #[tokio::test]
    async fn requires_trust_check_hash_match_returns_body() {
        let dir = tempfile::tempdir().unwrap();
        let body = "trusted content";
        let registry = make_registry_with_skill(dir.path(), "checked-skill", body);
        let hash = compute_skill_hash(&dir.path().join("checked-skill")).unwrap();
        let snapshots = HashMap::from([(
            "checked-skill".to_owned(),
            SkillTrustSnapshot {
                trust_level: SkillTrustLevel::Trusted,
                requires_trust_check: true,
                blake3_hash: hash,
            },
        )]);
        let gate = make_gate(registry, snapshots);
        match gate.resolve_body("checked-skill").await.unwrap() {
            SkillBodyResolution::Body(returned) => assert!(returned.contains(body)),
            other => panic!("expected Body, got a different variant: {other:?}"),
        }
    }

    #[tokio::test]
    async fn requires_trust_check_hash_mismatch_demotes_and_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let body = "content that changed after install";
        let registry = make_registry_with_skill(dir.path(), "tampered-skill", body);
        let snapshots = HashMap::from([(
            "tampered-skill".to_owned(),
            SkillTrustSnapshot {
                trust_level: SkillTrustLevel::Trusted,
                requires_trust_check: true,
                blake3_hash: "0".repeat(64),
            },
        )]);
        let trust_snapshot = Arc::new(RwLock::new(snapshots));
        let gate =
            SkillTrustGate::new(Arc::new(RwLock::new(registry)), Arc::clone(&trust_snapshot));
        match gate.resolve_body("tampered-skill").await.unwrap() {
            SkillBodyResolution::Refused(message) => {
                assert!(message.contains("demoted to Quarantined"));
                assert!(!message.contains(body));
            }
            other => panic!("expected Refused, got a different variant: {other:?}"),
        }
        assert_eq!(
            trust_snapshot
                .read()
                .get("tampered-skill")
                .unwrap()
                .trust_level,
            SkillTrustLevel::Quarantined,
            "in-memory snapshot must reflect the demotion for subsequent calls this turn"
        );
    }

    #[tokio::test]
    async fn missing_snapshot_defaults_to_trusted() {
        let dir = tempfile::tempdir().unwrap();
        let body = "unclassified skill body";
        let registry = make_registry_with_skill(dir.path(), "unknown-skill", body);
        let gate = make_gate(registry, HashMap::new());
        match gate.resolve_body("unknown-skill").await.unwrap() {
            SkillBodyResolution::Body(returned) => {
                assert!(!returned.contains("QUARANTINED"));
                assert!(returned.contains(body));
            }
            other => panic!("expected Body, got a different variant: {other:?}"),
        }
    }

    // ── #6701 (RC-3, D3): resolve_body folds the turn trust floor on Quarantined bodies ──

    #[tokio::test]
    async fn resolve_body_of_quarantined_skill_folds_turn_trust_floor() {
        let dir = tempfile::tempdir().unwrap();
        let body = "quarantined skill body";
        let registry = make_registry_with_skill(dir.path(), "quarantined-skill", body);
        let snapshots = HashMap::from([(
            "quarantined-skill".to_owned(),
            SkillTrustSnapshot {
                trust_level: SkillTrustLevel::Quarantined,
                requires_trust_check: false,
                blake3_hash: String::new(),
            },
        )]);
        let floor = zeph_common::TurnTrustFloor::new(SkillTrustLevel::Trusted);
        let gate = make_gate(registry, snapshots).with_turn_trust_floor(floor.clone());

        assert_eq!(
            floor.get(),
            SkillTrustLevel::Trusted,
            "sanity: starts Trusted"
        );
        match gate.resolve_body("quarantined-skill").await.unwrap() {
            SkillBodyResolution::Body(returned) => assert!(returned.contains("QUARANTINED")),
            other => panic!("expected Body, got a different variant: {other:?}"),
        }
        assert_eq!(
            floor.get(),
            SkillTrustLevel::Quarantined,
            "resolving a Quarantined body must fold the turn's trust floor down"
        );
    }

    #[tokio::test]
    async fn resolve_body_fold_never_raises_an_already_lower_floor() {
        let dir = tempfile::tempdir().unwrap();
        let registry = make_registry_with_skill(dir.path(), "quarantined-skill", "body");
        let snapshots = HashMap::from([(
            "quarantined-skill".to_owned(),
            SkillTrustSnapshot {
                trust_level: SkillTrustLevel::Quarantined,
                requires_trust_check: false,
                blake3_hash: String::new(),
            },
        )]);
        let floor = zeph_common::TurnTrustFloor::new(SkillTrustLevel::Blocked);
        let gate = make_gate(registry, snapshots).with_turn_trust_floor(floor.clone());

        let _ = gate.resolve_body("quarantined-skill").await.unwrap();
        assert_eq!(
            floor.get(),
            SkillTrustLevel::Blocked,
            "fold(Quarantined) must not raise a floor already folded to Blocked"
        );
    }

    #[tokio::test]
    async fn resolve_body_of_trusted_skill_does_not_touch_turn_trust_floor() {
        let dir = tempfile::tempdir().unwrap();
        let body = "trusted skill body";
        let registry = make_registry_with_skill(dir.path(), "trusted-skill", body);
        let snapshots = HashMap::from([(
            "trusted-skill".to_owned(),
            SkillTrustSnapshot {
                trust_level: SkillTrustLevel::Trusted,
                requires_trust_check: false,
                blake3_hash: String::new(),
            },
        )]);
        let floor = zeph_common::TurnTrustFloor::new(SkillTrustLevel::Trusted);
        let gate = make_gate(registry, snapshots).with_turn_trust_floor(floor.clone());

        let _ = gate.resolve_body("trusted-skill").await.unwrap();
        assert_eq!(floor.get(), SkillTrustLevel::Trusted);
    }

    #[tokio::test]
    async fn resolve_body_without_a_wired_floor_never_panics() {
        // No `with_turn_trust_floor` call — must simply skip the fold, not panic.
        let dir = tempfile::tempdir().unwrap();
        let registry = make_registry_with_skill(dir.path(), "quarantined-skill", "body");
        let snapshots = HashMap::from([(
            "quarantined-skill".to_owned(),
            SkillTrustSnapshot {
                trust_level: SkillTrustLevel::Quarantined,
                requires_trust_check: false,
                blake3_hash: String::new(),
            },
        )]);
        let gate = make_gate(registry, snapshots);
        match gate.resolve_body("quarantined-skill").await.unwrap() {
            SkillBodyResolution::Body(returned) => assert!(returned.contains("QUARANTINED")),
            other => panic!("expected Body, got a different variant: {other:?}"),
        }
    }

    // ── #6701 (S5): end-to-end RC-3 — resolve_body then a subsequent tool dispatch ──

    /// Minimal `ToolExecutor` that always allows, so the only thing under test is whether
    /// `TrustGateExecutor` denies `bash` — never whether the inner executor itself would.
    use zeph_tools::executor::ToolExecutor as _;

    #[derive(Debug)]
    struct AlwaysOkExecutor;

    impl zeph_tools::executor::ToolExecutor for AlwaysOkExecutor {
        async fn execute(
            &self,
            _response: &str,
        ) -> Result<Option<zeph_tools::executor::ToolOutput>, ToolError> {
            Ok(None)
        }

        async fn execute_tool_call(
            &self,
            call: &zeph_tools::executor::ToolCall,
        ) -> Result<Option<zeph_tools::executor::ToolOutput>, ToolError> {
            Ok(Some(zeph_tools::executor::ToolOutput {
                tool_name: call.tool_id.clone(),
                summary: "ok".into(),
                blocks_executed: 1,
                filter_stats: None,
                diff: None,
                streamed: false,
                terminal_id: None,
                locations: None,
                raw_response: None,
                claim_source: None,
                ..Default::default()
            }))
        }

        zeph_tools::tool_executor_no_inner_defaults!();
    }

    /// The spec's headline RC-3 invariant, exercised end-to-end rather than by asserting on
    /// `floor.get()` alone: a shared `TurnTrustFloor` wired into BOTH a `SkillTrustGate` (as
    /// `SkillInvokeExecutor`/`SkillLoaderExecutor` would be, in production) and a
    /// `TrustGateExecutor` (as the agent's real tool gate would be) — `resolve_body` on a
    /// Quarantined skill must fold the floor down, and a subsequent `bash` dispatch through the
    /// gate sharing that exact floor must then be denied.
    #[tokio::test]
    async fn resolve_body_of_quarantined_skill_then_bash_dispatch_is_denied() {
        let dir = tempfile::tempdir().unwrap();
        let registry = make_registry_with_skill(dir.path(), "quarantined-skill", "body");
        let snapshots = HashMap::from([(
            "quarantined-skill".to_owned(),
            SkillTrustSnapshot {
                trust_level: SkillTrustLevel::Quarantined,
                requires_trust_check: false,
                blake3_hash: String::new(),
            },
        )]);
        let floor = zeph_common::TurnTrustFloor::new(SkillTrustLevel::Trusted);
        let gate = make_gate(registry, snapshots).with_turn_trust_floor(floor.clone());
        // `from_legacy(&[], &[])` (no denied/confirm commands) resolves to Allow for bash, so
        // the only thing under test is the trust-level gate, not the Supervised-mode
        // confirmation-required default `PermissionPolicy::default()` would apply.
        let trust_gate = zeph_tools::TrustGateExecutor::new(
            AlwaysOkExecutor,
            zeph_tools::PermissionPolicy::from_legacy(&[], &[]),
        )
        .with_trust_floor(floor);

        // Sanity: bash is allowed before any Quarantined body has been read this turn.
        let call = zeph_tools::executor::ToolCall {
            tool_id: "bash".into(),
            params: serde_json::Map::new(),
            caller_id: None,
            context: None,
            tool_call_id: String::new(),
            skill_name: None,
        };
        assert!(
            trust_gate.execute_tool_call(&call).await.is_ok(),
            "bash must be allowed before any Quarantined body is read"
        );

        match gate.resolve_body("quarantined-skill").await.unwrap() {
            SkillBodyResolution::Body(returned) => assert!(returned.contains("QUARANTINED")),
            other => panic!("expected Body, got a different variant: {other:?}"),
        }

        let result = trust_gate.execute_tool_call(&call).await;
        assert!(
            matches!(result, Err(ToolError::Blocked { .. })),
            "a bash call in the same turn, after resolve_body returned a Quarantined body, \
             must be denied — got {result:?}"
        );
    }

    // ── resolve_require_check (#6087) ────────────────────────────────────────

    #[test]
    fn resolve_require_check_defaults_to_config_when_no_flag_forces_it() {
        assert!(resolve_require_check(false, false, true));
        assert!(!resolve_require_check(false, false, false));
    }

    #[test]
    fn resolve_require_check_force_on_wins_over_config_default_false() {
        assert!(resolve_require_check(true, false, false));
    }

    #[test]
    fn resolve_require_check_force_off_wins_over_config_default_true() {
        assert!(!resolve_require_check(false, true, true));
    }

    #[test]
    fn resolve_require_check_force_on_wins_over_force_off() {
        // Both flags present is nonsensical (clap rejects it on the CLI via conflicts_with),
        // but the in-session parser has no such enforcement — force_on must still take
        // precedence so the decision is total and unambiguous either way.
        assert!(resolve_require_check(true, true, false));
        assert!(resolve_require_check(true, true, true));
    }
}
