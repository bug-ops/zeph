// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Regression test for the quadratic scan in `strip_code_fence` on a long run of backticks
//! that never forms a closing fence line.

use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use zeph_common::llm_response::strip_code_fence;

const ONE_MIB: usize = 1024 * 1024;
const GENEROUS_BOUND: Duration = Duration::from_secs(5);

#[test]
fn strip_code_fence_single_line_backtick_run_completes_in_linear_time() {
    let input = "`".repeat(ONE_MIB);
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let len = strip_code_fence(&input).len();
        let _ = tx.send(len);
    });

    let len = rx
        .recv_timeout(GENEROUS_BOUND)
        .expect("strip_code_fence must finish a 1 MiB backtick run well within the bound");
    assert!(len <= ONE_MIB);
}
