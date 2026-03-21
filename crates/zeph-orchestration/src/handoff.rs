// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! MAST-informed handoff schema for inter-agent context transfer.
//!
//! # MAST Taxonomy Mapping (arxiv 2503.13657, section 2.1)
//!
//! MAST identified three coordination failure modes that account for 36.9% of
//! all multi-agent failures:
//!
//! | Failure Mode         | This module's fix                          |
//! |----------------------|--------------------------------------------|
//! | Ambiguous handoff    | `acceptance_criteria` required (>= 1 item) |
//! | Missing context      | `RoleContext` required fields per role      |
//! | No verification gate | `HandoffOutput.criteria_results` coverage   |
//!
//! Phase 1 ships only types + no-op validation stubs. Wiring into
//! `DagScheduler` happens in Phase 2.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

// ── DependencyOutput ────────────────────────────────────────────────────────

/// Completion status of an upstream dependency task.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DependencyStatus {
    Completed,
    Skipped,
    PartiallyCompleted { reason: String },
}

/// Structured output from a completed upstream dependency.
///
/// Replaces the raw string concatenation in `build_task_prompt()` (Phase 2).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DependencyOutput {
    /// Task ID of the completed dependency.
    pub task_id: String,
    /// Human-readable title of the dependency task.
    pub title: String,
    /// Completion status.
    pub status: DependencyStatus,
    /// Structured summary of what was accomplished.
    pub summary: String,
    /// Key artifacts produced (file paths, issue numbers, etc.).
    pub artifacts: Vec<String>,
    /// Whether the output was truncated due to token budget.
    pub truncated: bool,
}

// ── HandoffRef ──────────────────────────────────────────────────────────────

/// Reference to a prior handoff output — either inline or by ID.
///
/// **Phase 1-2**: Only `Inline` is supported. `ById` is defined for forward
/// compatibility but is rejected at validation time with a hard error.
/// `HandoffStore` (Phase 3) is required for `ById` resolution.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HandoffRef {
    /// Reference by handoff ID; resolved from `HandoffStore` at dispatch.
    /// PHASE 3: unsupported in Phase 1-2, rejected by `validate_context()`.
    ById { handoff_id: String },
    /// Inline content — the only supported variant in Phase 1-2.
    Inline { content: String },
}

// ── RoleContext variants ────────────────────────────────────────────────────

/// Context for an Architect agent: designs specs, defines interfaces.
///
/// Required fields: `spec_files` (>= 1), `scope` (>= 1).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchitectContext {
    /// Spec files the architect must read before designing.
    pub spec_files: Vec<String>,
    /// System constraints from constitution/invariants that apply.
    pub system_constraints: Vec<String>,
    /// Scope boundary: which crates/modules are in scope.
    pub scope: Vec<String>,
}

/// Context for a Developer agent: implements code changes per architect spec.
///
/// Required fields: `spec_ref`, `target_files` (>= 1), `test_requirements` (>= 1).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeveloperContext {
    /// Reference to the architect's spec output.
    pub spec_ref: HandoffRef,
    /// Files that must be modified (from architect output).
    pub target_files: Vec<String>,
    /// Test requirements: what tests must pass after implementation.
    pub test_requirements: Vec<String>,
    /// Feature flags that affect this implementation.
    pub feature_flags: Vec<String>,
}

/// Context for a Tester agent: validates implementation against criteria.
///
/// Required fields: `implementation_ref`, `test_plan` (>= 1).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TesterContext {
    /// Reference to the implementation handoff being tested.
    pub implementation_ref: HandoffRef,
    /// Test plan: specific scenarios to exercise.
    pub test_plan: Vec<String>,
    /// Expected test count delta (before/after).
    pub expected_test_delta: Option<TestDelta>,
    /// Whether live LLM session testing is required (LLM serialization gate).
    pub requires_live_test: bool,
}

/// Context for a Critic agent: reviews design/code for correctness and risks.
///
/// Required fields: `artifact_ref`, `review_dimensions` (>= 1).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CriticContext {
    /// Reference to the artifact being reviewed.
    pub artifact_ref: HandoffRef,
    /// Review dimensions to evaluate (correctness, perf, security, etc.).
    pub review_dimensions: Vec<String>,
    /// Known risks flagged by prior agents.
    pub known_risks: Vec<String>,
}

/// Context for a Reviewer agent: final review gate before merge.
///
/// Required fields: `artifact_refs` (>= 1), `checklist` (>= 1).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewerContext {
    /// References to implementation and all validator outputs.
    pub artifact_refs: Vec<HandoffRef>,
    /// Checklist items that must be verified before approval.
    pub checklist: Vec<String>,
    /// Whether this is a final merge gate (blocks merge on rejection).
    pub is_merge_gate: bool,
}

/// Fallback context for roles without specialized context fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenericContext {
    /// Free-form key-value pairs.
    pub fields: HashMap<String, String>,
}

/// Role-specific context payload for a sub-agent dispatch.
///
/// Each variant enforces the REQUIRED context for that agent role per the MAST
/// taxonomy. Required fields are validated by `validate_context()` (Phase 2).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum RoleContext {
    /// Designs specs, defines interfaces, plans implementation.
    Architect(ArchitectContext),
    /// Implements code changes per architect spec.
    Developer(DeveloperContext),
    /// Validates implementation against acceptance criteria.
    Tester(TesterContext),
    /// Reviews design/code for correctness, completeness, risks.
    Critic(CriticContext),
    /// Final review gate before merge (code quality, style, safety).
    Reviewer(ReviewerContext),
    /// Fallback for roles without specialized context.
    Generic(GenericContext),
}

// ── HandoffContext ──────────────────────────────────────────────────────────

/// Typed handoff context that accompanies every sub-agent dispatch.
///
/// Validated before dispatch (Phase 2); hard violations block execution.
/// Soft violations log a warning but do not block.
///
/// # Key Invariants
///
/// - `acceptance_criteria` MUST have at least 1 entry (MAST: ambiguous handoff)
/// - `role_context` required fields per role MUST be populated (MAST: missing context)
/// - `HandoffRef::ById` is rejected in Phase 1-2 (`HandoffStore` not yet implemented)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandoffContext {
    /// Unique handoff ID for traceability (UUID v4).
    pub handoff_id: String,
    /// ID of the parent handoff that produced this one (forms a chain).
    pub parent_handoff_id: Option<String>,
    /// Orchestration task ID (from `TaskGraph`) when dispatched via `DagScheduler`.
    pub task_id: Option<String>,
    /// The goal/objective in one sentence — what the agent must accomplish.
    pub objective: String,
    /// Concrete acceptance criteria the output must satisfy.
    /// At least one criterion is REQUIRED for all dispatches.
    pub acceptance_criteria: Vec<String>,
    /// Role-specific context payload.
    pub role_context: RoleContext,
    /// Outputs from upstream dependencies (structured, not raw text).
    pub dependency_outputs: Vec<DependencyOutput>,
    /// Hard constraints the agent must not violate.
    pub constraints: Vec<String>,
    /// Maximum allowed output size in characters (prevents runaway output).
    pub max_output_chars: Option<usize>,
}

// ── HandoffOutput ───────────────────────────────────────────────────────────

/// Pass/Fail/Partial/Skipped status for a single acceptance criterion.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CriterionStatus {
    Pass,
    Fail,
    Partial,
    Skipped,
}

/// Result for a single acceptance criterion from `HandoffContext`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CriterionResult {
    /// The acceptance criterion text (must match one from `HandoffContext`).
    pub criterion: String,
    /// Whether this criterion was met.
    pub status: CriterionStatus,
    /// Evidence or explanation for the result.
    pub evidence: String,
}

/// Test count before/after a developer task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestDelta {
    pub before: u32,
    pub after: u32,
}

/// Typed output from a sub-agent after task completion.
///
/// Validated against `HandoffContext.acceptance_criteria` by `verify_output()`
/// before the orchestrator marks the task as `Completed` (Phase 3).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandoffOutput {
    /// Reference back to the `HandoffContext` this output responds to.
    pub handoff_id: String,
    /// One-paragraph summary of what was accomplished.
    pub summary: String,
    /// Per-criterion verification results.
    pub criteria_results: Vec<CriterionResult>,
    /// Artifacts produced (file paths, PR numbers, issue numbers).
    pub artifacts: Vec<String>,
    /// Test count delta (if applicable).
    pub test_delta: Option<TestDelta>,
    /// Identified risks or issues for downstream agents.
    pub risks: Vec<String>,
    /// Suggested next steps (informational, not binding).
    pub next_steps: Vec<String>,
}

// ── Validation types ────────────────────────────────────────────────────────

/// Severity of a validation rule: Hard violations block dispatch, Soft log only.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationSeverity {
    Hard,
    Soft,
}

/// A single validation rule that can be applied to a `HandoffContext`.
///
/// Phase 1: defined for interface completeness; validation logic added in Phase 2.
#[derive(Debug, Clone)]
pub enum ValidationRule {
    /// Validates that `objective` is non-empty and within length limits.
    ObjectiveNonEmpty,
    /// Validates that at least one acceptance criterion is present.
    CriteriaPresent,
    /// Validates that all required role-specific fields are populated.
    RoleContextComplete,
    /// Validates that all `dependency_outputs` task IDs match completed graph nodes.
    DependencyOutputsMatch,
    /// Validates that `HandoffRef::ById` is not used (unsupported in Phase 1-2).
    HandoffRefSupported,
    /// Soft: checks agent capability compatibility with task requirements.
    AgentCapabilityMatch,
    /// Soft: checks that files/crates listed in scope exist on disk.
    ScopeValid,
}

/// Result of evaluating a single validation rule against a `HandoffContext`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationResult {
    /// Identifies which rule produced this result.
    pub rule_id: String,
    /// Whether the rule passed.
    pub passed: bool,
    /// Human-readable message (populated on failure or soft warning).
    pub message: String,
    /// Severity of this rule.
    pub severity: ValidationSeverity,
}

/// A validation error emitted for a single rule violation.
#[derive(Debug, Clone, Serialize)]
pub struct HandoffValidationError {
    pub handoff_id: String,
    pub severity: ValidationSeverity,
    pub rule: String,
    pub message: String,
    pub field_path: String,
}

/// Trait for types that can validate a `HandoffContext`.
///
/// Phase 1: interface only. Implementations added in Phase 2.
pub trait HandoffValidator {
    /// Run all applicable validation rules and return the results.
    ///
    /// # Errors
    ///
    /// Returns `HandoffValidationError` entries for any hard violation.
    fn validate(&self, ctx: &HandoffContext) -> Vec<ValidationResult>;
}

// ── HandoffMetrics ──────────────────────────────────────────────────────────

/// Metrics collected per handoff session for quality tracking.
///
/// All counters start at zero and are incremented by `DagScheduler` (Phase 2).
/// Exposed via `MetricsCollector` watch channel for TUI consumption (Phase 2).
#[derive(Debug, Clone, Default, Serialize)]
pub struct HandoffMetrics {
    /// Total handoffs dispatched in this session.
    pub total_dispatched: u64,
    /// Handoffs that passed pre-dispatch validation without warnings.
    pub clean_dispatches: u64,
    /// Handoffs that had soft validation warnings at dispatch.
    pub warned_dispatches: u64,
    /// Handoffs blocked by hard validation failures.
    pub blocked_dispatches: u64,
    /// Completed handoffs with `VerificationStatus::Verified`.
    pub verified_completions: u64,
    /// Completed handoffs with `VerificationStatus::PartiallyVerified`.
    pub partial_completions: u64,
    /// Completed handoffs with `VerificationStatus::Failed`.
    pub failed_completions: u64,
    /// Completed handoffs where output could not be parsed into `HandoffOutput`.
    pub unverified_completions: u64,
    /// Average criteria coverage ratio across all verified completions (0.0..1.0).
    pub avg_criteria_coverage: f64,
    /// Per-role dispatch counts.
    pub role_counts: HashMap<String, u64>,
    /// Per-rule violation counts for identifying systemic gaps.
    pub rule_violations: HashMap<String, u64>,
}

// ── Verification types (Phase 3) ────────────────────────────────────────────

/// Overall status of post-completion output verification.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationStatus {
    /// All criteria pass, output is well-formed.
    Verified,
    /// Output is well-formed but some criteria are `Partial` or `Skipped`.
    PartiallyVerified,
    /// One or more criteria failed.
    Failed,
    /// Output could not be parsed into `HandoffOutput`.
    Unverified,
}

/// Result of post-completion output verification (Phase 3).
#[derive(Debug, Clone, Serialize)]
pub struct VerificationResult {
    pub handoff_id: String,
    pub status: VerificationStatus,
    /// Fraction of `acceptance_criteria` covered by `criteria_results` (0.0..1.0).
    pub criteria_coverage: f64,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
}

// ── No-op validator (kept for trait object compatibility) ───────────────────

/// No-op validator — use `validate_context()` free function for Phase 2 validation.
pub struct NoopValidator;

impl HandoffValidator for NoopValidator {
    fn validate(&self, _ctx: &HandoffContext) -> Vec<ValidationResult> {
        Vec::new()
    }
}

// ── Phase 2: real validation implementation ─────────────────────────────────

/// Validate a `HandoffContext` against all pre-dispatch rules.
///
/// Returns a list of `ValidationResult` entries. Hard violations must be treated as
/// blocking by the caller (`DagScheduler`). Soft violations are logged as warnings.
///
/// # Rules applied
///
/// - `ObjectiveNonEmpty`: objective must be non-empty and ≤ 1000 chars.
/// - `CriteriaPresent`: at least one acceptance criterion required (MAST invariant).
/// - `RoleContextComplete`: required fields per `RoleContext` variant must be populated.
/// - `HandoffRefSupported`: `HandoffRef::ById` is rejected in Phase 1-2.
#[must_use]
pub fn validate_context(ctx: &HandoffContext) -> Vec<ValidationResult> {
    let mut results = Vec::new();

    // Rule: ObjectiveNonEmpty
    if ctx.objective.trim().is_empty() {
        results.push(ValidationResult {
            rule_id: "ObjectiveNonEmpty".to_string(),
            passed: false,
            message: "objective must not be empty".to_string(),
            severity: ValidationSeverity::Hard,
        });
    } else if ctx.objective.len() > 1000 {
        results.push(ValidationResult {
            rule_id: "ObjectiveNonEmpty".to_string(),
            passed: false,
            message: format!(
                "objective exceeds 1000 chars (got {}); truncate before dispatch",
                ctx.objective.len()
            ),
            severity: ValidationSeverity::Soft,
        });
    } else {
        results.push(ValidationResult {
            rule_id: "ObjectiveNonEmpty".to_string(),
            passed: true,
            message: String::new(),
            severity: ValidationSeverity::Hard,
        });
    }

    // Rule: CriteriaPresent
    if ctx.acceptance_criteria.is_empty() {
        results.push(ValidationResult {
            rule_id: "CriteriaPresent".to_string(),
            passed: false,
            message: "at least one acceptance criterion is required (MAST invariant)".to_string(),
            severity: ValidationSeverity::Hard,
        });
    } else {
        results.push(ValidationResult {
            rule_id: "CriteriaPresent".to_string(),
            passed: true,
            message: String::new(),
            severity: ValidationSeverity::Hard,
        });
    }

    // Rule: HandoffRefSupported — reject ById in Phase 1-2.
    let by_id_error = check_handoff_refs_supported(ctx);
    results.extend(by_id_error);

    // Rule: RoleContextComplete
    let role_errors = validate_role_context(&ctx.role_context);
    results.extend(role_errors);

    results
}

/// Check that no `HandoffRef::ById` variants are used (unsupported in Phase 1-2).
fn check_handoff_refs_supported(ctx: &HandoffContext) -> Vec<ValidationResult> {
    let mut results = Vec::new();

    let has_by_id = match &ctx.role_context {
        RoleContext::Developer(c) => matches!(c.spec_ref, HandoffRef::ById { .. }),
        RoleContext::Tester(c) => matches!(c.implementation_ref, HandoffRef::ById { .. }),
        RoleContext::Critic(c) => matches!(c.artifact_ref, HandoffRef::ById { .. }),
        RoleContext::Reviewer(c) => c
            .artifact_refs
            .iter()
            .any(|r| matches!(r, HandoffRef::ById { .. })),
        RoleContext::Architect(_) | RoleContext::Generic(_) => false,
    };

    if has_by_id {
        results.push(ValidationResult {
            rule_id: "HandoffRefSupported".to_string(),
            passed: false,
            message: "HandoffRef::ById is not supported in Phase 1-2; use Inline".to_string(),
            severity: ValidationSeverity::Hard,
        });
    } else {
        results.push(ValidationResult {
            rule_id: "HandoffRefSupported".to_string(),
            passed: true,
            message: String::new(),
            severity: ValidationSeverity::Hard,
        });
    }

    results
}

/// Validate role-specific required fields per `RoleContext` variant.
fn validate_role_context(role: &RoleContext) -> Vec<ValidationResult> {
    let mut errors: Vec<String> = Vec::new();

    match role {
        RoleContext::Architect(c) => {
            if c.spec_files.is_empty() {
                errors.push("Architect.spec_files must have at least 1 entry".to_string());
            }
            if c.scope.is_empty() {
                errors.push("Architect.scope must have at least 1 entry".to_string());
            }
        }
        RoleContext::Developer(c) => {
            if c.target_files.is_empty() {
                errors.push("Developer.target_files must have at least 1 entry".to_string());
            }
            if c.test_requirements.is_empty() {
                errors.push("Developer.test_requirements must have at least 1 entry".to_string());
            }
            // spec_ref content check for Inline variant
            if matches!(&c.spec_ref, HandoffRef::Inline { content } if content.trim().is_empty()) {
                errors.push("Developer.spec_ref Inline content must not be empty".to_string());
            }
        }
        RoleContext::Tester(c) => {
            if c.test_plan.is_empty() {
                errors.push("Tester.test_plan must have at least 1 entry".to_string());
            }
            if matches!(&c.implementation_ref, HandoffRef::Inline { content } if content.trim().is_empty())
            {
                errors
                    .push("Tester.implementation_ref Inline content must not be empty".to_string());
            }
        }
        RoleContext::Critic(c) => {
            if c.review_dimensions.is_empty() {
                errors.push("Critic.review_dimensions must have at least 1 entry".to_string());
            }
            if matches!(&c.artifact_ref, HandoffRef::Inline { content } if content.trim().is_empty())
            {
                errors.push("Critic.artifact_ref Inline content must not be empty".to_string());
            }
        }
        RoleContext::Reviewer(c) => {
            if c.artifact_refs.is_empty() {
                errors.push("Reviewer.artifact_refs must have at least 1 entry".to_string());
            }
            if c.checklist.is_empty() {
                errors.push("Reviewer.checklist must have at least 1 entry".to_string());
            }
        }
        RoleContext::Generic(_) => {
            // Generic context has no required fields beyond objective/criteria (checked above).
        }
    }

    if errors.is_empty() {
        vec![ValidationResult {
            rule_id: "RoleContextComplete".to_string(),
            passed: true,
            message: String::new(),
            severity: ValidationSeverity::Hard,
        }]
    } else {
        vec![ValidationResult {
            rule_id: "RoleContextComplete".to_string(),
            passed: false,
            message: errors.join("; "),
            severity: ValidationSeverity::Hard,
        }]
    }
}

/// Verify a `HandoffOutput` against the originating `HandoffContext`.
///
/// Checks:
/// - `criteria_results` covers at least one `acceptance_criteria` entry.
/// - No criterion result references a criterion not present in the context.
/// - Overall `VerificationStatus` is derived from per-criterion outcomes.
///
/// Returns a list of `ValidationResult` entries (informational; caller decides action).
#[must_use]
pub fn verify_output(ctx: &HandoffContext, output: &HandoffOutput) -> Vec<ValidationResult> {
    let mut results = Vec::new();

    // Check: at least one criterion result present.
    if output.criteria_results.is_empty() {
        results.push(ValidationResult {
            rule_id: "CriteriaResultsPresent".to_string(),
            passed: false,
            message: "HandoffOutput.criteria_results is empty; no verification possible"
                .to_string(),
            severity: ValidationSeverity::Soft,
        });
        return results;
    }

    // Check: each criterion result must reference a criterion from context.
    let ctx_criteria: std::collections::HashSet<&str> =
        ctx.acceptance_criteria.iter().map(String::as_str).collect();

    for cr in &output.criteria_results {
        if !ctx_criteria.contains(cr.criterion.as_str()) {
            results.push(ValidationResult {
                rule_id: "CriteriaMatch".to_string(),
                passed: false,
                message: format!(
                    "criterion_result references unknown criterion: {:?}",
                    cr.criterion
                ),
                severity: ValidationSeverity::Soft,
            });
        }
    }

    // Coverage: fraction of context criteria covered by results.
    let covered = output
        .criteria_results
        .iter()
        .filter(|cr| ctx_criteria.contains(cr.criterion.as_str()))
        .count();
    let total = ctx.acceptance_criteria.len();

    // Check: all criteria have a result (soft warning if incomplete).
    if covered < total {
        results.push(ValidationResult {
            rule_id: "CriteriaCoverage".to_string(),
            passed: false,
            message: format!("only {covered}/{total} acceptance criteria have results"),
            severity: ValidationSeverity::Soft,
        });
    } else {
        results.push(ValidationResult {
            rule_id: "CriteriaCoverage".to_string(),
            passed: true,
            message: format!("all {total} criteria covered"),
            severity: ValidationSeverity::Soft,
        });
    }

    // Check: no hard failures among criteria results.
    let failed_count = output
        .criteria_results
        .iter()
        .filter(|cr| matches!(cr.status, CriterionStatus::Fail))
        .count();

    if failed_count > 0 {
        results.push(ValidationResult {
            rule_id: "CriteriaOutcome".to_string(),
            passed: false,
            message: format!("{failed_count} criterion(ia) have Fail status"),
            severity: ValidationSeverity::Hard,
        });
    } else {
        results.push(ValidationResult {
            rule_id: "CriteriaOutcome".to_string(),
            passed: true,
            message: String::new(),
            severity: ValidationSeverity::Hard,
        });
    }

    results
}

/// Derive `VerificationStatus` from a `verify_output()` result list.
///
/// Used by `DagScheduler` to decide whether to mark a task `Completed` or flag it.
#[must_use]
pub fn derive_verification_status(
    output: &HandoffOutput,
    results: &[ValidationResult],
) -> VerificationStatus {
    // If no criteria results at all → Unverified.
    if output.criteria_results.is_empty() {
        return VerificationStatus::Unverified;
    }

    // Hard failure among rules → Failed.
    let hard_fail = results
        .iter()
        .any(|r| !r.passed && matches!(r.severity, ValidationSeverity::Hard));
    if hard_fail {
        return VerificationStatus::Failed;
    }

    // All criteria pass → Verified; any Partial/Skipped → PartiallyVerified.
    let all_pass = output
        .criteria_results
        .iter()
        .all(|cr| matches!(cr.status, CriterionStatus::Pass));
    if all_pass {
        VerificationStatus::Verified
    } else {
        VerificationStatus::PartiallyVerified
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn architect_ctx() -> HandoffContext {
        HandoffContext {
            handoff_id: "hoff-001".to_string(),
            parent_handoff_id: None,
            task_id: Some("task-0".to_string()),
            objective: "Design the HandoffContext schema".to_string(),
            acceptance_criteria: vec![
                "Schema covers all 5 agent roles".to_string(),
                "Output saved to spec.md".to_string(),
            ],
            role_context: RoleContext::Architect(ArchitectContext {
                spec_files: vec![".local/specs/constitution.md".to_string()],
                system_constraints: vec!["No new feature flags".to_string()],
                scope: vec!["crates/zeph-orchestration".to_string()],
            }),
            dependency_outputs: Vec::new(),
            constraints: vec!["Specification only, no code".to_string()],
            max_output_chars: Some(50_000),
        }
    }

    fn developer_ctx() -> HandoffContext {
        HandoffContext {
            handoff_id: "hoff-002".to_string(),
            parent_handoff_id: Some("hoff-001".to_string()),
            task_id: Some("task-1".to_string()),
            objective: "Implement HandoffContext types".to_string(),
            acceptance_criteria: vec!["Structs compile".to_string()],
            role_context: RoleContext::Developer(DeveloperContext {
                spec_ref: HandoffRef::Inline {
                    content: "see spec.md".to_string(),
                },
                target_files: vec!["crates/zeph-orchestration/src/handoff.rs".to_string()],
                test_requirements: vec!["cargo nextest run -p zeph-orchestration".to_string()],
                feature_flags: vec!["orchestration".to_string()],
            }),
            dependency_outputs: vec![DependencyOutput {
                task_id: "task-0".to_string(),
                title: "Design schema".to_string(),
                status: DependencyStatus::Completed,
                summary: "Spec created".to_string(),
                artifacts: vec![".local/specs/2023-handoff-hardening/spec.md".to_string()],
                truncated: false,
            }],
            constraints: Vec::new(),
            max_output_chars: None,
        }
    }

    // ── Serialization round-trips ───────────────────────────────────────────

    #[test]
    fn handoff_context_architect_roundtrip_json() {
        let ctx = architect_ctx();
        let json = serde_json::to_string(&ctx).expect("serialize");
        let restored: HandoffContext = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(ctx.handoff_id, restored.handoff_id);
        assert_eq!(ctx.objective, restored.objective);
        assert_eq!(
            ctx.acceptance_criteria.len(),
            restored.acceptance_criteria.len()
        );
    }

    #[test]
    fn handoff_context_developer_roundtrip_json() {
        let ctx = developer_ctx();
        let json = serde_json::to_string(&ctx).expect("serialize");
        let restored: HandoffContext = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(ctx.handoff_id, restored.handoff_id);
        assert_eq!(ctx.parent_handoff_id, restored.parent_handoff_id);
    }

    #[test]
    fn handoff_output_roundtrip_json() {
        let output = HandoffOutput {
            handoff_id: "hoff-001".to_string(),
            summary: "Implemented all types".to_string(),
            criteria_results: vec![CriterionResult {
                criterion: "Structs compile".to_string(),
                status: CriterionStatus::Pass,
                evidence: "cargo build succeeded".to_string(),
            }],
            artifacts: vec!["crates/zeph-orchestration/src/handoff.rs".to_string()],
            test_delta: Some(TestDelta {
                before: 100,
                after: 115,
            }),
            risks: Vec::new(),
            next_steps: vec!["Wire into DagScheduler in Phase 2".to_string()],
        };
        let json = serde_json::to_string(&output).expect("serialize");
        let restored: HandoffOutput = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(output.handoff_id, restored.handoff_id);
        assert_eq!(
            output.criteria_results.len(),
            restored.criteria_results.len()
        );
    }

    #[test]
    fn dependency_output_roundtrip_json() {
        let dep = DependencyOutput {
            task_id: "t0".to_string(),
            title: "build".to_string(),
            status: DependencyStatus::Completed,
            summary: "done".to_string(),
            artifacts: vec!["/tmp/out".to_string()],
            truncated: false,
        };
        let json = serde_json::to_string(&dep).expect("serialize");
        let restored: DependencyOutput = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(dep.task_id, restored.task_id);
        assert!(!restored.truncated);
    }

    #[test]
    fn partially_completed_status_roundtrip() {
        let s = DependencyStatus::PartiallyCompleted {
            reason: "tool failed".to_string(),
        };
        let json = serde_json::to_string(&s).expect("serialize");
        let restored: DependencyStatus = serde_json::from_str(&json).expect("deserialize");
        assert!(matches!(
            restored,
            DependencyStatus::PartiallyCompleted { .. }
        ));
    }

    // ── RoleContext variant instantiation ───────────────────────────────────

    #[test]
    fn all_role_context_variants_instantiate() {
        let variants: Vec<RoleContext> = vec![
            RoleContext::Architect(ArchitectContext {
                spec_files: vec!["spec.md".to_string()],
                system_constraints: Vec::new(),
                scope: vec!["crates/zeph-orchestration".to_string()],
            }),
            RoleContext::Developer(DeveloperContext {
                spec_ref: HandoffRef::Inline {
                    content: "inline".to_string(),
                },
                target_files: vec!["src/lib.rs".to_string()],
                test_requirements: vec!["pass".to_string()],
                feature_flags: Vec::new(),
            }),
            RoleContext::Tester(TesterContext {
                implementation_ref: HandoffRef::Inline {
                    content: "impl".to_string(),
                },
                test_plan: vec!["run nextest".to_string()],
                expected_test_delta: Some(TestDelta {
                    before: 10,
                    after: 15,
                }),
                requires_live_test: false,
            }),
            RoleContext::Critic(CriticContext {
                artifact_ref: HandoffRef::Inline {
                    content: "artifact".to_string(),
                },
                review_dimensions: vec!["correctness".to_string()],
                known_risks: Vec::new(),
            }),
            RoleContext::Reviewer(ReviewerContext {
                artifact_refs: vec![HandoffRef::Inline {
                    content: "pr".to_string(),
                }],
                checklist: vec!["tests pass".to_string()],
                is_merge_gate: true,
            }),
            RoleContext::Generic(GenericContext {
                fields: HashMap::from([("key".to_string(), "value".to_string())]),
            }),
        ];
        // All 6 variants must serialize without panic.
        for v in &variants {
            serde_json::to_string(v).expect("role context variant must serialize");
        }
    }

    #[test]
    fn role_context_serde_tag_is_snake_case() {
        let ctx = RoleContext::Architect(ArchitectContext {
            spec_files: vec!["f.md".to_string()],
            system_constraints: Vec::new(),
            scope: vec!["crate".to_string()],
        });
        let json = serde_json::to_string(&ctx).expect("serialize");
        assert!(
            json.contains("\"role\":\"architect\""),
            "tag must be snake_case: {json}"
        );
    }

    #[test]
    fn handoff_ref_inline_roundtrip() {
        let r = HandoffRef::Inline {
            content: "hello".to_string(),
        };
        let json = serde_json::to_string(&r).expect("serialize");
        let restored: HandoffRef = serde_json::from_str(&json).expect("deserialize");
        assert!(matches!(restored, HandoffRef::Inline { .. }));
    }

    #[test]
    fn handoff_ref_by_id_roundtrip() {
        let r = HandoffRef::ById {
            handoff_id: "hoff-001".to_string(),
        };
        let json = serde_json::to_string(&r).expect("serialize");
        let restored: HandoffRef = serde_json::from_str(&json).expect("deserialize");
        assert!(matches!(restored, HandoffRef::ById { .. }));
    }

    // ── Phase 2: validate_context ───────────────────────────────────────────

    #[test]
    fn validate_context_valid_architect_passes() {
        let ctx = architect_ctx();
        let results = validate_context(&ctx);
        let failures: Vec<&ValidationResult> = results.iter().filter(|r| !r.passed).collect();
        assert!(
            failures.is_empty(),
            "valid architect context must produce no failures: {failures:?}"
        );
    }

    #[test]
    fn validate_context_empty_objective_is_hard_fail() {
        let mut ctx = architect_ctx();
        ctx.objective = String::new();
        let results = validate_context(&ctx);
        let hard_fail = results.iter().find(|r| {
            r.rule_id == "ObjectiveNonEmpty"
                && !r.passed
                && matches!(r.severity, ValidationSeverity::Hard)
        });
        assert!(hard_fail.is_some(), "empty objective must be a hard fail");
    }

    #[test]
    fn validate_context_no_criteria_is_hard_fail() {
        let mut ctx = architect_ctx();
        ctx.acceptance_criteria.clear();
        let results = validate_context(&ctx);
        let hard_fail = results.iter().find(|r| {
            r.rule_id == "CriteriaPresent"
                && !r.passed
                && matches!(r.severity, ValidationSeverity::Hard)
        });
        assert!(hard_fail.is_some(), "missing criteria must be a hard fail");
    }

    #[test]
    fn validate_context_empty_spec_files_is_hard_fail() {
        let mut ctx = architect_ctx();
        if let RoleContext::Architect(ref mut ac) = ctx.role_context {
            ac.spec_files.clear();
        }
        let results = validate_context(&ctx);
        let hard_fail = results
            .iter()
            .find(|r| r.rule_id == "RoleContextComplete" && !r.passed);
        assert!(
            hard_fail.is_some(),
            "empty spec_files must trigger RoleContextComplete fail"
        );
    }

    #[test]
    fn validate_context_developer_empty_target_files_is_hard_fail() {
        let mut ctx = developer_ctx();
        if let RoleContext::Developer(ref mut dc) = ctx.role_context {
            dc.target_files.clear();
        }
        let results = validate_context(&ctx);
        let hard_fail = results
            .iter()
            .find(|r| r.rule_id == "RoleContextComplete" && !r.passed);
        assert!(
            hard_fail.is_some(),
            "empty target_files must trigger RoleContextComplete fail"
        );
    }

    #[test]
    fn validate_context_by_id_ref_is_hard_fail() {
        let mut ctx = developer_ctx();
        if let RoleContext::Developer(ref mut dc) = ctx.role_context {
            dc.spec_ref = HandoffRef::ById {
                handoff_id: "hoff-001".to_string(),
            };
        }
        let results = validate_context(&ctx);
        let hard_fail = results
            .iter()
            .find(|r| r.rule_id == "HandoffRefSupported" && !r.passed);
        assert!(
            hard_fail.is_some(),
            "ById ref must be rejected in Phase 1-2"
        );
    }

    #[test]
    fn validate_context_valid_developer_passes() {
        let ctx = developer_ctx();
        let results = validate_context(&ctx);
        let failures: Vec<&ValidationResult> = results.iter().filter(|r| !r.passed).collect();
        assert!(
            failures.is_empty(),
            "valid developer context must produce no failures: {failures:?}"
        );
    }

    // ── Phase 2: verify_output ──────────────────────────────────────────────

    #[test]
    fn verify_output_all_pass_is_verified() {
        let ctx = architect_ctx();
        let output = HandoffOutput {
            handoff_id: "hoff-001".to_string(),
            summary: "done".to_string(),
            criteria_results: vec![
                CriterionResult {
                    criterion: "Schema covers all 5 agent roles".to_string(),
                    status: CriterionStatus::Pass,
                    evidence: "all variants present".to_string(),
                },
                CriterionResult {
                    criterion: "Output saved to spec.md".to_string(),
                    status: CriterionStatus::Pass,
                    evidence: "file exists".to_string(),
                },
            ],
            artifacts: Vec::new(),
            test_delta: None,
            risks: Vec::new(),
            next_steps: Vec::new(),
        };
        let results = verify_output(&ctx, &output);
        let status = derive_verification_status(&output, &results);
        assert!(
            matches!(status, VerificationStatus::Verified),
            "all-pass output must be Verified, got {status:?}"
        );
    }

    #[test]
    fn verify_output_empty_criteria_results_is_unverified() {
        let ctx = architect_ctx();
        let output = HandoffOutput {
            handoff_id: "hoff-001".to_string(),
            summary: "done".to_string(),
            criteria_results: Vec::new(),
            artifacts: Vec::new(),
            test_delta: None,
            risks: Vec::new(),
            next_steps: Vec::new(),
        };
        let results = verify_output(&ctx, &output);
        let status = derive_verification_status(&output, &results);
        assert!(
            matches!(status, VerificationStatus::Unverified),
            "empty criteria_results must be Unverified"
        );
    }

    #[test]
    fn verify_output_fail_criterion_is_failed() {
        let ctx = architect_ctx();
        let output = HandoffOutput {
            handoff_id: "hoff-001".to_string(),
            summary: "partial".to_string(),
            criteria_results: vec![CriterionResult {
                criterion: "Schema covers all 5 agent roles".to_string(),
                status: CriterionStatus::Fail,
                evidence: "missing Tester variant".to_string(),
            }],
            artifacts: Vec::new(),
            test_delta: None,
            risks: Vec::new(),
            next_steps: Vec::new(),
        };
        let results = verify_output(&ctx, &output);
        let status = derive_verification_status(&output, &results);
        assert!(
            matches!(status, VerificationStatus::Failed),
            "Fail criterion must yield Failed verification status"
        );
    }

    #[test]
    fn verify_output_partial_criterion_is_partially_verified() {
        let ctx = architect_ctx();
        let output = HandoffOutput {
            handoff_id: "hoff-001".to_string(),
            summary: "partial".to_string(),
            criteria_results: vec![
                CriterionResult {
                    criterion: "Schema covers all 5 agent roles".to_string(),
                    status: CriterionStatus::Partial,
                    evidence: "4/5 variants".to_string(),
                },
                CriterionResult {
                    criterion: "Output saved to spec.md".to_string(),
                    status: CriterionStatus::Pass,
                    evidence: "file exists".to_string(),
                },
            ],
            artifacts: Vec::new(),
            test_delta: None,
            risks: Vec::new(),
            next_steps: Vec::new(),
        };
        let results = verify_output(&ctx, &output);
        let status = derive_verification_status(&output, &results);
        assert!(
            matches!(status, VerificationStatus::PartiallyVerified),
            "Partial criterion must yield PartiallyVerified"
        );
    }

    #[test]
    fn validate_context_tester_empty_test_plan_is_hard_fail() {
        let mut ctx = HandoffContext {
            handoff_id: "hoff-tester".to_string(),
            parent_handoff_id: None,
            task_id: Some("task-2".to_string()),
            objective: "Test handoff implementation".to_string(),
            acceptance_criteria: vec!["tests pass".to_string()],
            role_context: RoleContext::Tester(TesterContext {
                implementation_ref: HandoffRef::Inline {
                    content: "impl done".to_string(),
                },
                test_plan: vec!["run nextest".to_string()],
                expected_test_delta: None,
                requires_live_test: false,
            }),
            dependency_outputs: Vec::new(),
            constraints: Vec::new(),
            max_output_chars: None,
        };
        if let RoleContext::Tester(ref mut tc) = ctx.role_context {
            tc.test_plan.clear();
        }
        let results = validate_context(&ctx);
        let hard_fail = results
            .iter()
            .find(|r| r.rule_id == "RoleContextComplete" && !r.passed);
        assert!(
            hard_fail.is_some(),
            "empty test_plan must trigger RoleContextComplete fail"
        );
    }

    #[test]
    fn validate_context_critic_empty_review_dimensions_is_hard_fail() {
        let ctx = HandoffContext {
            handoff_id: "hoff-critic".to_string(),
            parent_handoff_id: None,
            task_id: Some("task-3".to_string()),
            objective: "Review implementation".to_string(),
            acceptance_criteria: vec!["review complete".to_string()],
            role_context: RoleContext::Critic(CriticContext {
                artifact_ref: HandoffRef::Inline {
                    content: "artifact".to_string(),
                },
                review_dimensions: Vec::new(), // empty — must fail
                known_risks: Vec::new(),
            }),
            dependency_outputs: Vec::new(),
            constraints: Vec::new(),
            max_output_chars: None,
        };
        let results = validate_context(&ctx);
        let hard_fail = results
            .iter()
            .find(|r| r.rule_id == "RoleContextComplete" && !r.passed);
        assert!(
            hard_fail.is_some(),
            "empty review_dimensions must trigger RoleContextComplete fail"
        );
    }

    #[test]
    fn validate_context_reviewer_empty_artifact_refs_is_hard_fail() {
        let ctx = HandoffContext {
            handoff_id: "hoff-reviewer".to_string(),
            parent_handoff_id: None,
            task_id: Some("task-4".to_string()),
            objective: "Final review before merge".to_string(),
            acceptance_criteria: vec!["approved".to_string()],
            role_context: RoleContext::Reviewer(ReviewerContext {
                artifact_refs: Vec::new(), // empty — must fail
                checklist: vec!["tests pass".to_string()],
                is_merge_gate: true,
            }),
            dependency_outputs: Vec::new(),
            constraints: Vec::new(),
            max_output_chars: None,
        };
        let results = validate_context(&ctx);
        let hard_fail = results
            .iter()
            .find(|r| r.rule_id == "RoleContextComplete" && !r.passed);
        assert!(
            hard_fail.is_some(),
            "empty artifact_refs must trigger RoleContextComplete fail"
        );
    }

    // ── HandoffMetrics defaults ─────────────────────────────────────────────

    #[test]
    fn handoff_metrics_default_all_zero() {
        let m = HandoffMetrics::default();
        assert_eq!(m.total_dispatched, 0);
        assert_eq!(m.clean_dispatches, 0);
        assert_eq!(m.warned_dispatches, 0);
        assert_eq!(m.blocked_dispatches, 0);
        assert_eq!(m.verified_completions, 0);
        assert_eq!(m.partial_completions, 0);
        assert_eq!(m.failed_completions, 0);
        assert_eq!(m.unverified_completions, 0);
        assert!(m.role_counts.is_empty());
        assert!(m.rule_violations.is_empty());
    }
}
