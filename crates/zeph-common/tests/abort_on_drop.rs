// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0
#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Acceptance tests (tests-first) for #7136: `AbortOnDrop<R>` aborts its supervised task on drop
//! and the registry entry disappears.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use zeph_common::task_supervisor::{AbortOnDrop, TaskSupervisor};

struct SetOnDrop(Arc<AtomicBool>);
impl Drop for SetOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

async fn eventually(mut cond: impl FnMut() -> bool) -> bool {
    for _ in 0..100 {
        if cond() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    cond()
}

fn live(sup: &TaskSupervisor, needle: &str) -> usize {
    sup.snapshot()
        .iter()
        .filter(|t| t.name.contains(needle))
        .count()
}

#[tokio::test]
async fn abort_on_drop_drop_aborts_the_parked_task() {
    let sup = TaskSupervisor::new(CancellationToken::new());
    let dropped = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&dropped);
    let guard = AbortOnDrop::new(sup.spawn_oneshot(Arc::from("parked"), move || async move {
        let _probe = SetOnDrop(flag);
        std::future::pending::<()>().await;
    }));
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!dropped.load(Ordering::SeqCst));
    drop(guard);
    assert!(
        eventually(|| dropped.load(Ordering::SeqCst)).await,
        "task future must be dropped after the guard"
    );
}

#[tokio::test]
async fn abort_on_drop_drop_removes_registry_entry() {
    let sup = TaskSupervisor::new(CancellationToken::new());
    let guard =
        AbortOnDrop::new(sup.spawn_oneshot(Arc::from("parked"), std::future::pending::<()>));
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(live(&sup, "parked"), 1);
    drop(guard);
    assert!(
        eventually(|| live(&sup, "parked") == 0).await,
        "registry entry must be reaped after the guard drops"
    );
}

#[tokio::test]
async fn abort_on_drop_holding_the_guard_keeps_the_task_running() {
    let sup = TaskSupervisor::new(CancellationToken::new());
    let _guard = AbortOnDrop::new(sup.spawn_oneshot(Arc::from("kept"), std::future::pending::<()>));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(live(&sup, "kept"), 1);
}

#[tokio::test]
async fn abort_on_drop_debug_is_opaque() {
    let sup = TaskSupervisor::new(CancellationToken::new());
    let guard = AbortOnDrop::new(sup.spawn_oneshot(Arc::from("dbg"), || async {}));
    assert_eq!(format!("{guard:?}"), "AbortOnDrop");
}

#[tokio::test]
async fn scoped_views_with_distinct_scopes_keep_distinct_oneshot_names() {
    let sup = TaskSupervisor::new(CancellationToken::new());
    let a = AbortOnDrop::new(
        sup.scoped("session.1")
            .spawn_oneshot(Arc::from("bridge"), std::future::pending::<()>),
    );
    let b = AbortOnDrop::new(
        sup.scoped("session.2")
            .spawn_oneshot(Arc::from("bridge"), std::future::pending::<()>),
    );
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(live(&sup, "bridge"), 2);
    drop(a);
    assert!(eventually(|| live(&sup, "bridge") == 1).await);
    drop(b);
    assert!(eventually(|| live(&sup, "bridge") == 0).await);
}
