// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Task orchestration: DAG execution, failure propagation, and persistence.

pub mod aggregator;
pub mod command;
pub mod dag;
pub mod error;
pub mod graph;
pub mod handoff;
pub mod plan_cache;
pub mod planner;
pub mod router;
pub mod scheduler;

pub use aggregator::{Aggregator, LlmAggregator};
pub use command::PlanCommand;
pub use error::OrchestrationError;
pub use graph::{
    FailureStrategy, GraphId, GraphPersistence, GraphStatus, TaskGraph, TaskId, TaskNode,
    TaskResult, TaskStatus,
};
pub use handoff::{
    ArchitectContext, CriterionResult, CriterionStatus, CriticContext, DependencyOutput,
    DependencyStatus, DeveloperContext, GenericContext, HandoffContext, HandoffMetrics,
    HandoffOutput, HandoffRef, HandoffValidationError, HandoffValidator, NoopValidator,
    ReviewerContext, RoleContext, TestDelta, TesterContext, ValidationResult, ValidationRule,
    ValidationSeverity, VerificationResult, VerificationStatus, derive_verification_status,
    validate_context, verify_output,
};
pub use plan_cache::{
    PlanCache, PlanCacheError, PlanTemplate, TemplateTask, normalize_goal, plan_with_cache,
};
pub use planner::{LlmPlanner, Planner};
pub use router::{AgentRouter, RuleBasedRouter};
pub use scheduler::{DagScheduler, SchedulerAction, TaskEvent, TaskOutcome};
