// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Acceptance test for #7162: two agent instances of one ACP session id get distinct supervisor
//! task names.
//!
//! Needs `#[cfg(test)] mod session_scope_tests;` in `src/acp.rs` and
//! `fn acp_session_task_supervisor(shell: &TaskSupervisor, session_id: &str) -> TaskSupervisor`.

use std::sync::Arc;

use tokio_util::sync::CancellationToken;
use zeph_common::task_supervisor::TaskSupervisor;

use super::acp_session_task_supervisor;

#[tokio::test]
async fn acp_session_task_supervisor_scopes_task_names_per_agent_instance() {
    let shell = TaskSupervisor::new(CancellationToken::new());

    let first = acp_session_task_supervisor(&shell, "shared-session");
    let second = acp_session_task_supervisor(&shell, "shared-session");
    let _handles: Vec<_> = [&first, &second]
        .into_iter()
        .map(|supervisor| {
            supervisor.spawn_oneshot(Arc::from("agent.lifecycle.cancel_bridge"), || {
                std::future::pending::<()>()
            })
        })
        .collect();

    let names: Vec<Arc<str>> = shell.snapshot().into_iter().map(|s| s.name).collect();
    let bridges: Vec<&Arc<str>> = names
        .iter()
        .filter(|n| n.contains("cancel_bridge"))
        .collect();
    assert_eq!(
        bridges.len(),
        2,
        "both tasks must stay registered: {names:?}"
    );
    assert_ne!(bridges[0], bridges[1]);
    assert!(bridges.iter().all(|n| n.contains("shared-session")));
}
