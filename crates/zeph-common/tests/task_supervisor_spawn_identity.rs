// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0
//! Acceptance tests (tests-first) for #7141: a completion from a replaced same-name task must not
//! unregister the newer entry, and registration must precede any completion (F1).

use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use zeph_common::task_supervisor::{RestartPolicy, TaskDescriptor, TaskStatus, TaskSupervisor};

async fn settle() {
    for _ in 0..10 {
        tokio::time::sleep(Duration::from_millis(30)).await;
        tokio::task::yield_now().await;
    }
}

fn running(sup: &TaskSupervisor, name: &str) -> usize {
    sup.snapshot()
        .iter()
        .filter(|t| &*t.name == name && t.status == TaskStatus::Running)
        .count()
}

#[tokio::test]
async fn spawn_oneshot_same_name_twice_keeps_newer_entry_running() {
    let sup = TaskSupervisor::new(CancellationToken::new());
    let _first = sup.spawn_oneshot(Arc::from("dup-oneshot"), || async {
        tokio::time::sleep(Duration::from_secs(60)).await;
    });
    let _second = sup.spawn_oneshot(Arc::from("dup-oneshot"), || async {
        tokio::time::sleep(Duration::from_secs(60)).await;
    });
    settle().await;
    assert_eq!(running(&sup, "dup-oneshot"), 1);
    assert_eq!(sup.active_count(), 1);
}

#[tokio::test]
async fn spawn_restart_policy_same_name_twice_keeps_newer_entry_running() {
    let sup = TaskSupervisor::new(CancellationToken::new());
    for _ in 0..2 {
        let _ = sup.spawn(TaskDescriptor {
            name: "dup-restart",
            restart: RestartPolicy::Restart {
                max: 3,
                base_delay: Duration::from_millis(10),
            },
            factory: || async { tokio::time::sleep(Duration::from_secs(60)).await },
        });
    }
    settle().await;
    assert_eq!(running(&sup, "dup-restart"), 1);
}

#[tokio::test]
async fn spawn_blocking_same_name_twice_keeps_newer_entry_running() {
    let sup = TaskSupervisor::new(CancellationToken::new());
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let (release2_tx, release2_rx) = std::sync::mpsc::channel::<()>();
    let _first = sup.spawn_blocking(Arc::from("dup-blocking"), move || {
        let _ = release_rx.recv_timeout(Duration::from_secs(10));
    });
    let second = sup.spawn_blocking(Arc::from("dup-blocking"), move || {
        let _ = release2_rx.recv_timeout(Duration::from_secs(10));
    });
    settle().await;
    assert_eq!(running(&sup, "dup-blocking"), 1);
    assert!(!second.is_finished());
    let _ = release_tx.send(());
    let _ = release2_tx.send(());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn spawn_and_spawn_blocking_immediately_finishing_tasks_leave_empty_registry() {
    // Regression-only: probabilistic on the old ordering, deterministic once registration
    // happens-before completion.
    const NAMES: [&str; 8] = [
        "f1-a", "f1-b", "f1-c", "f1-d", "f1-e", "f1-f", "f1-g", "f1-h",
    ];
    let sup = TaskSupervisor::new(CancellationToken::new());
    for _ in 0..50 {
        for name in NAMES {
            let _ = sup.spawn(TaskDescriptor {
                name,
                restart: RestartPolicy::RunOnce,
                factory: || async {},
            });
            let _ = sup.spawn_blocking(Arc::from(format!("{name}-blocking")), || {});
        }
        for _ in 0..200 {
            if sup.active_count() == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(sup.active_count(), 0, "stale entry left in registry");
    }
}
