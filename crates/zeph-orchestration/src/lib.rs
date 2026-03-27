// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Task orchestration: DAG execution, failure propagation, and persistence.

pub mod aggregator;
pub mod command;
pub mod dag;
pub mod error;
pub mod graph;
pub mod plan_cache;
pub mod planner;
pub mod router;
pub mod scheduler;
pub mod topology;
pub mod verifier;

pub use aggregator::{Aggregator, LlmAggregator};
pub use command::PlanCommand;
pub use error::OrchestrationError;
pub use graph::{
    ExecutionMode, FailureStrategy, GraphId, GraphPersistence, GraphStatus, TaskGraph, TaskId,
    TaskNode, TaskResult, TaskStatus,
};
pub use plan_cache::{
    PlanCache, PlanCacheError, PlanTemplate, TemplateTask, normalize_goal, plan_with_cache,
};
pub use planner::{LlmPlanner, Planner};
pub use router::{AgentRouter, RuleBasedRouter};
pub use scheduler::{DagScheduler, SchedulerAction, TaskEvent, TaskOutcome};
pub use topology::{DispatchStrategy, Topology, TopologyAnalysis, TopologyClassifier};
pub use verifier::{Gap, GapSeverity, PlanVerifier, VerificationResult};
