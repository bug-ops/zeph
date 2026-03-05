// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Task orchestration: DAG execution, failure propagation, and persistence.

pub mod dag;
pub mod error;
pub mod graph;
pub mod planner;
pub mod router;
pub mod scheduler;

pub use crate::config::OrchestrationConfig;
pub use error::OrchestrationError;
pub use graph::{
    FailureStrategy, GraphId, GraphPersistence, GraphStatus, TaskGraph, TaskId, TaskNode,
    TaskResult, TaskStatus,
};
pub use planner::{LlmPlanner, Planner};
pub use router::{AgentRouter, RuleBasedRouter};
pub use scheduler::{DagScheduler, SchedulerAction, TaskEvent, TaskOutcome};
