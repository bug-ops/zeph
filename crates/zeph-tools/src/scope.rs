// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `ScopedToolExecutor`: config-driven capability scoping wrapper.
//!
//! Wraps any `ToolExecutor` and filters both `tool_definitions()` (LLM tool list) and
//! `execute_tool_call()` (dispatch path) to an operator-configured allow-list of
//! fully-qualified tool ids.
//!
//! # Wiring order
//!
//! ```text
//! ScopedToolExecutor          ← outermost (this crate)
//!   → PolicyGateExecutor
//!       → TrustGateExecutor
//!           → CompositeExecutor
//!               → ToolFilter, AuditedExecutor, ...
//! ```
//!
//! `ScopedToolExecutor` is placed outside `PolicyGateExecutor` so an out-of-scope call
//! short-circuits before policy evaluation.
//!
//! # Tool-id namespacing
//!
//! All tool ids MUST carry a namespace prefix before scope resolution:
//!
//! | Source | Prefix |
//! |---|---|
//! | Built-in executors | `builtin:` |
//! | Skill-defined tools | `skill:<name>/` |
//! | MCP tools | `mcp:<server_id>/` |
//! | ACP / A2A proxied tools | `acp:<peer>/` / `a2a:<peer>/` |
//!
//! An un-namespaced tool id returned by an executor at registration is a
//! `ScopeError::UnqualifiedId`.
//!
//! # Pattern strictness
//!
//! - `builtin:` / `skill:` globs: strict — zero-match is `ScopeError::DeadPattern`.
//! - `mcp:` / `acp:` / `a2a:` globs: provisional — zero-match is
//!   `ScopeWarning::ProvisionalDeadPattern` (re-resolved on dynamic registration).
//! - A glob matching the **entire** registry without an explicit `general` opt-in is
//!   `ScopeError::AccidentallyFull`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use arc_swap::ArcSwap;
use globset::{Glob, GlobSet, GlobSetBuilder};
use tracing::warn;

use crate::audit::{AuditEntry, AuditLogger, AuditResult, chrono_now};
use crate::executor::{ToolCall, ToolError, ToolExecutor, ToolOutput};
use crate::registry::ToolDef;
use zeph_config::{CapabilityScopesConfig, PatternStrictness};

// ── Errors & warnings ─────────────────────────────────────────────────────────

/// Fatal startup error emitted when a scope configuration is invalid.
#[derive(Debug, thiserror::Error)]
pub enum ScopeError {
    /// A glob pattern in a strict namespace matched zero registered tool ids.
    #[error("scope '{scope}': pattern '{pattern}' matched zero registered tools (dead pattern)")]
    DeadPattern { scope: String, pattern: String },

    /// A glob pattern expanded to the entire tool registry without an explicit opt-in.
    #[error(
        "scope '{scope}': pattern '{pattern}' matches the entire registry; use default_scope=\"general\" to opt in"
    )]
    AccidentallyFull { scope: String, pattern: String },

    /// An executor registered a tool id without a namespace prefix.
    #[error("tool id '{id}' has no namespace prefix (expected '<namespace>:<id>')")]
    UnqualifiedId { id: String },

    /// A glob pattern could not be compiled.
    #[error("scope '{scope}': invalid glob pattern '{pattern}': {source}")]
    InvalidPattern {
        scope: String,
        pattern: String,
        #[source]
        source: globset::Error,
    },
}

/// Non-fatal warning emitted for provisional-namespace zero-match patterns.
#[derive(Debug)]
pub struct ScopeWarning {
    /// The scope name containing the unresolved pattern.
    pub scope: String,
    /// The glob pattern that matched zero ids at build time.
    pub pattern: String,
}

// ── ToolScope ─────────────────────────────────────────────────────────────────

/// Materialised tool scope: a pre-compiled allow-list of fully-qualified tool ids.
///
/// At agent build time, glob patterns are resolved against the registered tool set
/// and stored as a `HashSet<String>`. Runtime admission is an O(1) lookup.
#[derive(Debug, Clone)]
pub struct ToolScope {
    /// Identifier of this scope (task-type name).
    pub task_type: Option<String>,
    /// Expanded, materialised set of fully-qualified tool ids.
    admitted: HashSet<String>,
    /// `true` for the `general` default-scope only; admits every id without lookup.
    is_full: bool,
    /// Original patterns, kept for re-resolution when new tools are registered dynamically.
    patterns: Vec<String>,
}

impl ToolScope {
    /// The identity scope: admits every tool id. Used for the `general` default scope.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_tools::scope::ToolScope;
    ///
    /// let scope = ToolScope::full();
    /// assert!(scope.admits("builtin:shell"));
    /// assert!(scope.admits("mcp:any_server/any_tool"));
    /// ```
    #[must_use]
    pub fn full() -> Self {
        Self {
            task_type: None,
            admitted: HashSet::new(),
            is_full: true,
            patterns: vec!["*".to_owned()],
        }
    }

    /// Compile a scope from glob patterns against the materialised registry.
    ///
    /// # Errors
    ///
    /// Returns `ScopeError::DeadPattern` when a strict-namespace glob matches zero ids,
    /// `ScopeError::AccidentallyFull` when a pattern expands to the entire registry without
    /// an explicit `general` opt-in, or `ScopeError::InvalidPattern` on invalid glob syntax.
    pub fn try_compile<S: std::hash::BuildHasher>(
        task_type: impl Into<String>,
        patterns: &[String],
        registry_ids: &HashSet<String, S>,
        strictness: PatternStrictness,
        is_general_scope: bool,
    ) -> Result<(Self, Vec<ScopeWarning>), ScopeError> {
        let task_type_str = task_type.into();
        let mut admitted = HashSet::new();
        let mut warnings = Vec::new();

        for pattern in patterns {
            // Validate that glob compiles.
            let glob = Glob::new(pattern).map_err(|e| ScopeError::InvalidPattern {
                scope: task_type_str.clone(),
                pattern: pattern.clone(),
                source: e,
            })?;

            let mut builder = GlobSetBuilder::new();
            builder.add(glob);
            let glob_set: GlobSet = builder.build().map_err(|e| ScopeError::InvalidPattern {
                scope: task_type_str.clone(),
                pattern: pattern.clone(),
                source: e,
            })?;

            let matched: HashSet<String> = registry_ids
                .iter()
                .filter(|id| glob_set.is_match(id.as_str()))
                .cloned()
                .collect();

            // Check for accidentally-full expansion (unless this is the general scope).
            if !is_general_scope && matched.len() == registry_ids.len() && !registry_ids.is_empty()
            {
                return Err(ScopeError::AccidentallyFull {
                    scope: task_type_str,
                    pattern: pattern.clone(),
                });
            }

            if matched.is_empty() {
                let is_strict = is_strict_pattern(pattern, strictness);
                if is_strict {
                    return Err(ScopeError::DeadPattern {
                        scope: task_type_str,
                        pattern: pattern.clone(),
                    });
                }
                warnings.push(ScopeWarning {
                    scope: task_type_str.clone(),
                    pattern: pattern.clone(),
                });
            }

            admitted.extend(matched);
        }

        Ok((
            Self {
                task_type: Some(task_type_str),
                admitted,
                is_full: false,
                patterns: patterns.to_vec(),
            },
            warnings,
        ))
    }

    /// Returns `true` when the given fully-qualified tool id is admitted by this scope.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_tools::scope::ToolScope;
    ///
    /// let scope = ToolScope::full();
    /// assert!(scope.admits("builtin:shell"));
    /// ```
    #[must_use]
    pub fn admits(&self, qualified_tool_id: &str) -> bool {
        self.is_full || self.admitted.contains(qualified_tool_id)
    }

    /// Returns the list of admitted tool ids (excluding `full` scopes).
    ///
    /// Useful for `/scope list` output and the `scope_at_definition` audit field.
    #[must_use]
    pub fn admitted_ids(&self) -> Vec<&str> {
        self.admitted.iter().map(String::as_str).collect()
    }

    /// The raw glob patterns this scope was compiled from (for re-resolution).
    #[must_use]
    pub fn patterns(&self) -> &[String] {
        &self.patterns
    }

    /// Re-resolve the scope against a new registry (called on dynamic tool registration).
    ///
    /// Returns a new `ToolScope` with the updated admit set; warnings are logged but not
    /// returned (non-fatal for provisional namespaces).
    #[must_use]
    pub fn re_resolve<S: std::hash::BuildHasher>(&self, registry_ids: &HashSet<String, S>) -> Self {
        let task_type_str = self
            .task_type
            .clone()
            .unwrap_or_else(|| "<unknown>".to_owned());
        let mut admitted = HashSet::new();
        for pattern in &self.patterns {
            let Ok(glob) = Glob::new(pattern) else {
                warn!(scope = %task_type_str, pattern, "re-resolve: invalid glob, skipping");
                continue;
            };
            let mut builder = GlobSetBuilder::new();
            builder.add(glob);
            let Ok(glob_set) = builder.build() else {
                continue;
            };
            let matched: HashSet<String> = registry_ids
                .iter()
                .filter(|id| glob_set.is_match(id.as_str()))
                .cloned()
                .collect();
            admitted.extend(matched);
        }
        Self {
            task_type: self.task_type.clone(),
            admitted,
            is_full: false,
            patterns: self.patterns.clone(),
        }
    }
}

/// Returns `true` when the pattern targets a strict namespace (`builtin:` or `skill:`).
fn is_strict_pattern(pattern: &str, strictness: PatternStrictness) -> bool {
    match strictness {
        PatternStrictness::Strict => true,
        PatternStrictness::Permissive => false,
        PatternStrictness::ProvisionalForDynamicNamespaces => {
            // Strict for builtin: and skill:; provisional for mcp:, acp:, a2a:
            pattern.starts_with("builtin:") || pattern.starts_with("skill:")
        }
    }
}

// ── ScopedToolExecutor ────────────────────────────────────────────────────────

/// Wraps any `ToolExecutor` and enforces a capability scope on both tool listing and dispatch.
///
/// # Type parameter
///
/// `E` is the inner executor (e.g., `PolicyGateExecutor<TrustGateExecutor<CompositeExecutor>>`).
///
/// # Examples
///
/// ```rust,no_run
/// use std::collections::HashSet;
/// use zeph_tools::scope::{ScopedToolExecutor, ToolScope};
/// use zeph_tools::{ToolExecutor, ToolCall};
/// use zeph_common::ToolName;
///
/// // Build a full (no-op) scope — identity, admits everything.
/// let scope = ToolScope::full();
///
/// // Wrap some inner executor (omitted for brevity).
/// struct MockExecutor;
/// impl ToolExecutor for MockExecutor {
///     async fn execute(&self, _: &str) -> Result<Option<zeph_tools::ToolOutput>, zeph_tools::ToolError> { Ok(None) }
/// }
/// let executor = ScopedToolExecutor::new(MockExecutor, scope);
/// ```
pub struct ScopedToolExecutor<E: ToolExecutor> {
    inner: E,
    /// Atomically swappable active scope. Swapped via `set_scope()`.
    scope: ArcSwap<ToolScope>,
    /// Named scope map for task-type lookup.
    scopes: HashMap<String, Arc<ToolScope>>,
    /// Name of the scope currently surfaced to the LLM (captured at `tool_definitions()` time).
    scope_at_definition: parking_lot::Mutex<Option<String>>,
    /// Optional shared queue — `OutOfScope` signal codes pushed here; drained by `begin_turn()`.
    signal_queue: Option<crate::policy_gate::RiskSignalQueue>,
    /// Optional audit logger — `out_of_scope` entries emitted on every rejection.
    audit: Option<Arc<AuditLogger>>,
}

impl<E: ToolExecutor> ScopedToolExecutor<E> {
    /// Create a new `ScopedToolExecutor` with the given initial scope.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// use zeph_tools::scope::{ScopedToolExecutor, ToolScope};
    ///
    /// struct Noop;
    /// impl zeph_tools::ToolExecutor for Noop {
    ///     async fn execute(&self, _: &str) -> Result<Option<zeph_tools::ToolOutput>, zeph_tools::ToolError> { Ok(None) }
    /// }
    /// let executor = ScopedToolExecutor::new(Noop, ToolScope::full());
    /// ```
    #[must_use]
    pub fn new(inner: E, initial_scope: ToolScope) -> Self {
        Self {
            inner,
            scope: ArcSwap::from_pointee(initial_scope),
            scopes: HashMap::new(),
            scope_at_definition: parking_lot::Mutex::new(None),
            signal_queue: None,
            audit: None,
        }
    }

    /// Attach an audit logger so every `OutOfScope` rejection writes an audit entry.
    #[must_use]
    pub fn with_audit(mut self, audit: Arc<AuditLogger>) -> Self {
        self.audit = Some(audit);
        self
    }

    /// Attach a shared signal queue so `OutOfScope` rejections are recorded in the sentinel.
    #[must_use]
    pub fn with_signal_queue(mut self, queue: crate::policy_gate::RiskSignalQueue) -> Self {
        self.signal_queue = Some(queue);
        self
    }

    /// Register a named scope for use with `set_scope_for_task`.
    pub fn register_scope(&mut self, name: impl Into<String>, scope: ToolScope) {
        self.scopes.insert(name.into(), Arc::new(scope));
    }

    /// Switch the active scope by task-type name. Returns `false` when the name is not found.
    pub fn set_scope_for_task(&self, task_type: &str) -> bool {
        if let Some(scope) = self.scopes.get(task_type) {
            self.scope.store(Arc::clone(scope));
            true
        } else {
            false
        }
    }

    /// Replace the active scope with the given one directly.
    pub fn set_scope(&self, scope: ToolScope) {
        self.scope.store(Arc::new(scope));
    }

    /// Return the list of tool ids admitted by the scope for `task_type`.
    ///
    /// Returns `None` when `task_type` is not registered.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// use zeph_tools::scope::{ScopedToolExecutor, ToolScope};
    ///
    /// struct Noop;
    /// impl zeph_tools::ToolExecutor for Noop {
    ///     async fn execute(&self, _: &str) -> Result<Option<zeph_tools::ToolOutput>, zeph_tools::ToolError> { Ok(None) }
    /// }
    /// let mut executor = ScopedToolExecutor::new(Noop, ToolScope::full());
    /// // scope_for_task returns None for unregistered task types
    /// assert!(executor.scope_for_task("unknown").is_none());
    /// ```
    #[must_use]
    pub fn scope_for_task(&self, task_type: &str) -> Option<Vec<String>> {
        self.scopes.get(task_type).map(|s| {
            if s.is_full {
                vec!["*".to_owned()]
            } else {
                s.admitted_ids().iter().map(|s| (*s).to_owned()).collect()
            }
        })
    }

    /// Name of the active scope at the last `tool_definitions()` call (for audit).
    #[must_use]
    pub fn scope_at_definition_name(&self) -> Option<String> {
        self.scope_at_definition.lock().clone()
    }

    /// Name of the currently active scope (for audit at dispatch time).
    #[must_use]
    pub fn active_scope_name(&self) -> Option<String> {
        self.scope.load().task_type.clone()
    }
}

impl<E: ToolExecutor> ToolExecutor for ScopedToolExecutor<E> {
    // CRIT-03 carve-out: legacy fenced-block dispatch path is not scoped (mirrors PolicyGate).
    async fn execute(&self, response: &str) -> Result<Option<ToolOutput>, ToolError> {
        self.inner.execute(response).await
    }

    async fn execute_confirmed(&self, response: &str) -> Result<Option<ToolOutput>, ToolError> {
        self.inner.execute_confirmed(response).await
    }

    /// Return the filtered tool definitions visible to the LLM under the active scope.
    ///
    /// Captures the active scope name into `scope_at_definition` for audit use.
    fn tool_definitions(&self) -> Vec<ToolDef> {
        let scope = self.scope.load();
        self.scope_at_definition.lock().clone_from(&scope.task_type);
        self.inner
            .tool_definitions()
            .into_iter()
            .filter(|d| {
                let id = d.id.as_ref();
                scope.admits(id)
            })
            .collect()
    }

    /// Execute a structured tool call, rejecting out-of-scope ids before any side-effect.
    ///
    /// Returns `ToolError::OutOfScope` when the tool id is not in the active scope.
    /// The audit log entry at the call site must carry `error_category = "out_of_scope"`.
    async fn execute_tool_call(&self, call: &ToolCall) -> Result<Option<ToolOutput>, ToolError> {
        let scope = self.scope.load();
        let tool_id = call.tool_id.as_str();

        // Reject un-namespaced tool ids (NEVER clause from spec 050).
        if !tool_id.contains(':') {
            return Err(ToolError::OutOfScope {
                tool_id: tool_id.to_owned(),
                task_type: scope.task_type.clone(),
            });
        }

        if !scope.admits(tool_id) {
            let scope_name = scope.task_type.clone();
            let scope_def = self.scope_at_definition.lock().clone();
            tracing::debug!(
                tool_id,
                scope = ?scope_name,
                "ScopedToolExecutor: out-of-scope rejection"
            );
            // Signal code 3 = OutOfScope (matches RiskSignal::OutOfScope in zeph-core).
            if let Some(ref q) = self.signal_queue {
                q.lock().push(3);
            }
            // F4: emit audit entry with error_category = "out_of_scope".
            if let Some(ref audit) = self.audit {
                let entry = AuditEntry {
                    timestamp: chrono_now(),
                    tool: call.tool_id.clone(),
                    command: String::new(),
                    result: AuditResult::Blocked {
                        reason: "out_of_scope".to_owned(),
                    },
                    duration_ms: 0,
                    error_category: Some("out_of_scope".to_owned()),
                    error_domain: Some("security".to_owned()),
                    error_phase: None,
                    claim_source: None,
                    mcp_server_id: None,
                    injection_flagged: false,
                    embedding_anomalous: false,
                    cross_boundary_mcp_to_acp: false,
                    adversarial_policy_decision: None,
                    exit_code: None,
                    truncated: false,
                    caller_id: call.caller_id.clone(),
                    policy_match: None,
                    correlation_id: None,
                    vigil_risk: None,
                    execution_env: None,
                    resolved_cwd: None,
                    scope_at_definition: scope_def,
                    scope_at_dispatch: scope_name,
                };
                audit.log(&entry).await;
            }
            return Err(ToolError::OutOfScope {
                tool_id: tool_id.to_owned(),
                task_type: scope.task_type.clone(),
            });
        }

        self.inner.execute_tool_call(call).await
    }

    async fn execute_tool_call_confirmed(
        &self,
        call: &ToolCall,
    ) -> Result<Option<ToolOutput>, ToolError> {
        let scope = self.scope.load();
        let tool_id = call.tool_id.as_str();
        if !tool_id.contains(':') || !scope.admits(tool_id) {
            let scope_name = scope.task_type.clone();
            let scope_def = self.scope_at_definition.lock().clone();
            if let Some(ref q) = self.signal_queue {
                q.lock().push(3);
            }
            if let Some(ref audit) = self.audit {
                let entry = AuditEntry {
                    timestamp: chrono_now(),
                    tool: call.tool_id.clone(),
                    command: String::new(),
                    result: AuditResult::Blocked {
                        reason: "out_of_scope".to_owned(),
                    },
                    duration_ms: 0,
                    error_category: Some("out_of_scope".to_owned()),
                    error_domain: Some("security".to_owned()),
                    error_phase: None,
                    claim_source: None,
                    mcp_server_id: None,
                    injection_flagged: false,
                    embedding_anomalous: false,
                    cross_boundary_mcp_to_acp: false,
                    adversarial_policy_decision: None,
                    exit_code: None,
                    truncated: false,
                    caller_id: call.caller_id.clone(),
                    policy_match: None,
                    correlation_id: None,
                    vigil_risk: None,
                    execution_env: None,
                    resolved_cwd: None,
                    scope_at_definition: scope_def,
                    scope_at_dispatch: scope_name,
                };
                audit.log(&entry).await;
            }
            return Err(ToolError::OutOfScope {
                tool_id: tool_id.to_owned(),
                task_type: scope.task_type.clone(),
            });
        }
        self.inner.execute_tool_call_confirmed(call).await
    }

    fn set_skill_env(&self, env: Option<std::collections::HashMap<String, String>>) {
        self.inner.set_skill_env(env);
    }

    fn set_effective_trust(&self, level: crate::SkillTrustLevel) {
        self.inner.set_effective_trust(level);
    }

    fn is_tool_retryable(&self, tool_id: &str) -> bool {
        self.inner.is_tool_retryable(tool_id)
    }

    fn is_tool_speculatable(&self, tool_id: &str) -> bool {
        self.inner.is_tool_speculatable(tool_id)
    }
}

// ── Config-driven builder ──────────────────────────────────────────────────────

/// Build a `ScopedToolExecutor` from a `CapabilityScopesConfig` and a registered tool set.
///
/// Returns a fatal `ScopeError` when any strict-namespace pattern matches zero tools.
/// Emits `ScopeWarning` entries for provisional-namespace zero-match patterns.
///
/// # Errors
///
/// Returns `ScopeError` when scope configuration is invalid (dead patterns, accidental-full).
///
/// # Examples
///
/// ```rust,no_run
/// use std::collections::HashSet;
/// use zeph_config::CapabilityScopesConfig;
/// use zeph_tools::scope::build_scoped_executor;
///
/// struct Noop;
/// impl zeph_tools::ToolExecutor for Noop {
///     async fn execute(&self, _: &str) -> Result<Option<zeph_tools::ToolOutput>, zeph_tools::ToolError> { Ok(None) }
/// }
///
/// let cfg = CapabilityScopesConfig::default();
/// let registry: HashSet<String> = HashSet::new();
/// let executor = build_scoped_executor(Noop, &cfg, &registry).expect("build failed");
/// ```
pub fn build_scoped_executor<E: ToolExecutor, S: std::hash::BuildHasher>(
    inner: E,
    cfg: &CapabilityScopesConfig,
    registry_ids: &HashSet<String, S>,
) -> Result<ScopedToolExecutor<E>, ScopeError> {
    // Verify no un-namespaced ids in registry.
    for id in registry_ids {
        if !id.contains(':') {
            return Err(ScopeError::UnqualifiedId { id: id.clone() });
        }
    }

    let default_scope_name = &cfg.default_scope;
    let strictness = cfg.pattern_strictness;

    // The default initial scope is full (no-op) unless a named default_scope is configured.
    let initial_scope = ToolScope::full();
    let mut executor = ScopedToolExecutor::new(inner, initial_scope);

    for (task_type, scope_cfg) in &cfg.scopes {
        let is_general = task_type == default_scope_name;
        let (scope, warnings) = ToolScope::try_compile(
            task_type.clone(),
            &scope_cfg.patterns,
            registry_ids,
            strictness,
            is_general,
        )?;
        for w in &warnings {
            warn!(
                scope = %w.scope,
                pattern = %w.pattern,
                "capability scope: provisional zero-match pattern (will re-resolve on dynamic registration)"
            );
        }
        executor.register_scope(task_type.clone(), scope);
    }

    // If a default_scope is configured and registered, activate it.
    if cfg.scopes.contains_key(default_scope_name.as_str()) {
        executor.set_scope_for_task(default_scope_name);
    }

    Ok(executor)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::ToolCall;
    use crate::registry::{InvocationHint, ToolDef};
    use zeph_common::ToolName;
    use zeph_config::{CapabilityScopesConfig, PatternStrictness, ScopeConfig};

    fn make_registry(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|s| (*s).to_owned()).collect()
    }

    struct NullExecutor {
        defs: Vec<ToolDef>,
    }

    impl ToolExecutor for NullExecutor {
        async fn execute(&self, _: &str) -> Result<Option<ToolOutput>, ToolError> {
            Ok(None)
        }

        fn tool_definitions(&self) -> Vec<ToolDef> {
            self.defs.clone()
        }

        async fn execute_tool_call(
            &self,
            call: &ToolCall,
        ) -> Result<Option<ToolOutput>, ToolError> {
            Ok(Some(ToolOutput {
                tool_name: call.tool_id.clone(),
                summary: "ok".to_owned(),
                blocks_executed: 1,
                filter_stats: None,
                diff: None,
                streamed: false,
                terminal_id: None,
                locations: None,
                raw_response: None,
                claim_source: None,
            }))
        }
    }

    fn null_def(id: &str) -> ToolDef {
        ToolDef {
            id: id.to_owned().into(),
            description: "test tool".into(),
            schema: schemars::schema_for!(String),
            invocation: InvocationHint::ToolCall,
            output_schema: None,
        }
    }

    fn make_call(tool_id: &str) -> ToolCall {
        ToolCall {
            tool_id: ToolName::new(tool_id),
            params: serde_json::Map::new(),
            caller_id: None,
            context: None,

            tool_call_id: String::new(),
        }
    }

    #[test]
    fn full_scope_admits_everything() {
        let scope = ToolScope::full();
        assert!(scope.admits("builtin:shell"));
        assert!(scope.admits("mcp:server/tool"));
        assert!(scope.admits("builtin:read"));
    }

    #[test]
    fn compiled_scope_admits_only_matched() {
        let registry = make_registry(&["builtin:shell", "builtin:read", "builtin:write"]);
        let patterns = vec!["builtin:read".to_owned()];
        let (scope, warnings) = ToolScope::try_compile(
            "narrow",
            &patterns,
            &registry,
            PatternStrictness::Strict,
            false,
        )
        .unwrap();
        assert!(warnings.is_empty());
        assert!(scope.admits("builtin:read"));
        assert!(!scope.admits("builtin:shell"));
        assert!(!scope.admits("builtin:write"));
    }

    #[test]
    fn dead_pattern_strict_returns_error() {
        let registry = make_registry(&["builtin:shell"]);
        let patterns = vec!["builtin:nonexistent".to_owned()];
        let result = ToolScope::try_compile(
            "test",
            &patterns,
            &registry,
            PatternStrictness::Strict,
            false,
        );
        assert!(
            matches!(result, Err(ScopeError::DeadPattern { .. })),
            "expected DeadPattern, got {result:?}"
        );
    }

    #[test]
    fn dead_pattern_provisional_returns_warning() {
        let registry = make_registry(&["builtin:shell"]);
        let patterns = vec!["mcp:server/nonexistent".to_owned()];
        let result = ToolScope::try_compile(
            "test",
            &patterns,
            &registry,
            PatternStrictness::ProvisionalForDynamicNamespaces,
            false,
        );
        assert!(result.is_ok());
        let (_, warnings) = result.unwrap();
        assert_eq!(warnings.len(), 1);
    }

    #[test]
    fn accidentally_full_pattern_returns_error() {
        let registry = make_registry(&["builtin:shell", "builtin:read"]);
        let patterns = vec!["*".to_owned()];
        let result = ToolScope::try_compile(
            "test",
            &patterns,
            &registry,
            PatternStrictness::Strict,
            false, // not general scope
        );
        assert!(
            matches!(result, Err(ScopeError::AccidentallyFull { .. })),
            "expected AccidentallyFull for non-general scope with '*'"
        );
    }

    #[test]
    fn general_scope_allows_wildcard() {
        let registry = make_registry(&["builtin:shell", "builtin:read"]);
        let patterns = vec!["*".to_owned()];
        let result = ToolScope::try_compile(
            "general",
            &patterns,
            &registry,
            PatternStrictness::Strict,
            true, // is_general_scope = true
        );
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn executor_rejects_out_of_scope_call() {
        let registry = make_registry(&["builtin:shell", "builtin:read"]);
        let (scope, _) = ToolScope::try_compile(
            "narrow",
            &["builtin:read".to_owned()],
            &registry,
            PatternStrictness::Strict,
            false,
        )
        .unwrap();
        let inner = NullExecutor {
            defs: vec![null_def("builtin:shell"), null_def("builtin:read")],
        };
        let executor = ScopedToolExecutor::new(inner, scope);
        let call = make_call("builtin:shell");
        let result = executor.execute_tool_call(&call).await;
        assert!(matches!(result, Err(ToolError::OutOfScope { .. })));
    }

    #[tokio::test]
    async fn executor_allows_in_scope_call() {
        let registry = make_registry(&["builtin:shell", "builtin:read"]);
        let (scope, _) = ToolScope::try_compile(
            "narrow",
            &["builtin:read".to_owned()],
            &registry,
            PatternStrictness::Strict,
            false,
        )
        .unwrap();
        let inner = NullExecutor {
            defs: vec![null_def("builtin:shell"), null_def("builtin:read")],
        };
        let executor = ScopedToolExecutor::new(inner, scope);
        let call = make_call("builtin:read");
        let result = executor.execute_tool_call(&call).await;
        assert!(result.is_ok());
    }

    #[test]
    fn tool_definitions_filtered_by_scope() {
        let registry = make_registry(&["builtin:shell", "builtin:read"]);
        let (scope, _) = ToolScope::try_compile(
            "narrow",
            &["builtin:read".to_owned()],
            &registry,
            PatternStrictness::Strict,
            false,
        )
        .unwrap();
        let inner = NullExecutor {
            defs: vec![null_def("builtin:shell"), null_def("builtin:read")],
        };
        let executor = ScopedToolExecutor::new(inner, scope);
        let defs = executor.tool_definitions();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].id.as_ref(), "builtin:read");
    }

    #[tokio::test]
    async fn unnamespaced_tool_id_rejected() {
        let scope = ToolScope::full();
        let inner = NullExecutor {
            defs: vec![null_def("builtin:shell")],
        };
        let executor = ScopedToolExecutor::new(inner, scope);
        let call = make_call("shell"); // no namespace
        let result = executor.execute_tool_call(&call).await;
        assert!(
            matches!(result, Err(ToolError::OutOfScope { .. })),
            "un-namespaced id must be rejected"
        );
    }

    #[test]
    fn build_scoped_executor_rejects_unqualified_registry_id() {
        let cfg = CapabilityScopesConfig::default();
        let registry = make_registry(&["shell"]); // no namespace
        let inner = NullExecutor { defs: vec![] };
        let result = build_scoped_executor(inner, &cfg, &registry);
        assert!(
            matches!(result, Err(ScopeError::UnqualifiedId { .. })),
            "unqualified registry id must be rejected"
        );
    }

    #[test]
    fn scope_for_task_returns_ids() {
        let registry = make_registry(&["builtin:shell", "builtin:read"]);
        let (scope, _) = ToolScope::try_compile(
            "narrow",
            &["builtin:read".to_owned()],
            &registry,
            PatternStrictness::Strict,
            false,
        )
        .unwrap();
        let inner = NullExecutor { defs: vec![] };
        let mut executor = ScopedToolExecutor::new(inner, ToolScope::full());
        executor.register_scope("narrow", scope);
        let ids = executor.scope_for_task("narrow");
        assert!(ids.is_some());
        let ids = ids.unwrap();
        assert!(ids.contains(&"builtin:read".to_owned()));
        assert!(!ids.contains(&"builtin:shell".to_owned()));
    }

    #[test]
    fn scope_for_task_returns_none_for_unknown() {
        let inner = NullExecutor { defs: vec![] };
        let executor = ScopedToolExecutor::new(inner, ToolScope::full());
        assert!(executor.scope_for_task("does_not_exist").is_none());
    }

    #[test]
    fn re_resolve_updates_admitted_set() {
        // Initial registry: two tools so builtin:* does not accidentally cover everything.
        // Use a specific pattern to keep the test simple.
        let registry = make_registry(&["builtin:read", "mcp:server/tool"]);
        let (scope, _) = ToolScope::try_compile(
            "narrow",
            &["builtin:read".to_owned()],
            &registry,
            PatternStrictness::Strict,
            false,
        )
        .unwrap();
        assert!(scope.admits("builtin:read"));
        assert!(!scope.admits("builtin:write"));

        // After re-resolve with a new registry entry the pattern still only matches "builtin:read".
        let mut new_registry = registry.clone();
        new_registry.insert("builtin:write".to_owned());
        let updated = scope.re_resolve(&new_registry);
        assert!(updated.admits("builtin:read"));
        // "builtin:write" is not in the original pattern, so it remains excluded.
        assert!(!updated.admits("builtin:write"));
    }

    #[test]
    fn build_from_config_with_scopes() {
        let mut scopes = std::collections::HashMap::new();
        scopes.insert(
            "general".to_owned(),
            ScopeConfig {
                patterns: vec!["*".to_owned()],
            },
        );
        scopes.insert(
            "narrow".to_owned(),
            ScopeConfig {
                patterns: vec!["builtin:read".to_owned()],
            },
        );
        let cfg = CapabilityScopesConfig {
            default_scope: "general".to_owned(),
            strict: false,
            pattern_strictness: PatternStrictness::Strict,
            scopes,
        };
        let registry = make_registry(&["builtin:shell", "builtin:read"]);
        let inner = NullExecutor { defs: vec![] };
        let executor = build_scoped_executor(inner, &cfg, &registry).unwrap();
        // narrow scope should be registered
        let narrow_ids = executor.scope_for_task("narrow");
        assert!(narrow_ids.is_some());
        let ids = narrow_ids.unwrap();
        assert!(ids.contains(&"builtin:read".to_owned()));
    }
}
