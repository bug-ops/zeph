// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0
//! A special file swapped in for a resource must be refused, not block the reader (#7159).

#![cfg(unix)]

use std::sync::mpsc;
use std::time::Duration;

use zeph_skills::resource::{SkillResourcePath, load_skill_resource};

#[test]
fn fifo_resource_is_rejected_without_blocking() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir(dir.path().join("references")).expect("mkdir");
    let fifo = dir.path().join("references/pipe.md");
    let status = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .expect("mkfifo");
    assert!(status.success());

    let base = dir.path().to_path_buf();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let path = SkillResourcePath::parse("references/pipe.md").expect("path");
        let _ = tx.send(load_skill_resource(&base, &path).is_err());
    });
    let rejected = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("load_skill_resource blocked on a FIFO");
    assert!(rejected);
}
