// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

// Release-profile layout computation for `durable::journal_budget()`'s async state machine
// exceeds rustc's default 128 query-depth limit due to the generic/trait-object nesting in
// `zeph_durable`'s `DurableContext::step`. Same class of issue as zeph-acp (see commit
// 076c7fb3); dev/debug builds are unaffected.
#![recursion_limit = "256"]

//! Multi-model task orchestration: DAG execution, failure propagation, and persistence.
//!
//! `zeph-orchestration` decomposes a user goal into a directed acyclic graph (DAG)
//! of sub-tasks, schedules them for concurrent execution by specialised sub-agents,
//! and synthesises the results into a coherent final response.
//!
//! # Architecture overview
//!
//! ```text
//! User goal
//!    │
//!    ▼
//! [Planner] ──LLM──► TaskGraph (DAG)
//!    │
//!    ▼
//! [DagScheduler] ──tick()──► SchedulerAction
//!    │                           │
//!    │          ┌────────────────┘
//!    │          ▼
//!    │      spawn sub-agent / run inline / cancel / done
//!    │
//!    ▼  (TaskEvent)
//! [DagScheduler] records outcome, applies failure strategy, routes next tasks
//!    │
//!    ▼
//! [Aggregator] ──LLM──► synthesised response
//! ```
//!
//! # Core types
//!
//! - [`TaskGraph`] / [`TaskNode`] — the DAG and its nodes
//! - [`DagScheduler`] — drives execution, emits [`SchedulerAction`]s
//! - [`Planner`] / [`LlmPlanner`] — decomposes a goal into a [`TaskGraph`]
//! - [`Aggregator`] / [`LlmAggregator`] — synthesises completed task outputs
//! - [`AgentRouter`] / [`RuleBasedRouter`] — selects the best agent for a task
//! - [`PlanCache`] — caches and reuses completed plan skeletons
//! - [`PlanVerifier`] — post-task completeness verifier with targeted replan
//!
//! # Example: build a plan and run the scheduler
//!
//! ```rust,ignore
//! use zeph_orchestration::{LlmPlanner, DagScheduler, RuleBasedRouter};
//! use zeph_config::OrchestrationConfig;
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let config = OrchestrationConfig::default();
//! let planner = LlmPlanner::new(my_provider, &config);
//! let (graph, _usage) = planner.plan("build and deploy service", &agents).await?;
//!
//! let scheduler = DagScheduler::new(
//!     graph,
//!     &config,
//!     Box::new(RuleBasedRouter),
//!     agents.clone(),
//! )?;
//! // drive the scheduler loop …
//! # Ok(())
//! # }
//! ```

pub mod admission;
pub mod cascade;
pub mod command;
pub mod dag;
pub mod durable;
pub mod error;
pub mod graph;
pub mod lineage;
pub mod router;
pub mod scheduler;
pub mod topology;

pub mod adaptorch;
pub mod aggregator;
pub mod ensemble;
pub mod plan_cache;
pub mod planner;
pub mod verifier;
pub mod verify_predicate;

pub use admission::AdmissionGate;
pub use cascade::{AbortDecision, CascadeConfig, CascadeDetector, RegionHealth};
pub use command::{HandoffCommand, PlanCommand, TaskRef, has_handoff_fence, parse_handoff_command};
pub use dag::{lookahead_tools, validate_handoff_target};
pub use error::OrchestrationError;
pub use graph::{
    ExecutionMode, FailureStrategy, GraphId, GraphPersistence, GraphStatus, NetworkScope, PlanSlug,
    PredicateOutcome, RecoveryAction, TaskGraph, TaskId, TaskNode, TaskResult, TaskStatus,
    TimeoutPolicy, VerifyPredicate,
};
pub use lineage::{ErrorLineage, LineageEntry, LineageKind, classify_error};
pub use router::{AgentRouter, RuleBasedRouter};
pub use scheduler::{DagScheduler, SchedulerAction, TaskEvent, TaskOutcome, ToolCallSummary};
pub use topology::{
    DispatchStrategy, Topology, TopologyAnalysis, TopologyClassifier, build_rev_adj,
};

pub use adaptorch::{AdaptOrchMetrics, AdvisorVerdict, TaskClass, TopologyAdvisor, TopologyHint};
pub use aggregator::{Aggregator, LlmAggregator};
pub use ensemble::{Ballot, EnsembleAttempt, EnsembleTracker, EnsembleVerifier, MergeOutcome};
pub use plan_cache::{
    PlanCache, PlanCacheError, PlanTemplate, TemplateTask, normalize_goal, plan_with_cache,
};
pub use planner::{LlmPlanner, Planner};
pub use verifier::{Gap, GapSeverity, PlanVerifier, VerificationResult};
pub use verify_predicate::PredicateEvaluator;
