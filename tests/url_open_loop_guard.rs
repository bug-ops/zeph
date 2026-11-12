// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! INV-LOOP coverage for #7025: the `zeph url-open` re-entrancy guard runs in `main` before
//! the runtime starts, so it is exercised end to end against the built binary.

use std::process::{Command, Output};

fn zeph_bin_path() -> String {
    std::env::var("NEXTEST_BIN_EXE_zeph")
        .or_else(|_| std::env::var("CARGO_BIN_EXE_zeph"))
        .expect("NEXTEST_BIN_EXE_zeph or CARGO_BIN_EXE_zeph must be set by the test runner")
}

fn url_open(uri: &str, depth_marker: Option<&str>) -> Output {
    let mut cmd = Command::new(zeph_bin_path());
    cmd.args(["url-open", uri])
        .env_remove("ZEPH_URL_OPEN_DEPTH");
    if let Some(marker) = depth_marker {
        cmd.env("ZEPH_URL_OPEN_DEPTH", marker);
    }
    cmd.output().expect("failed to run the zeph binary")
}

#[test]
fn reentrant_dispatch_exits_with_loop_message() {
    let out = url_open("zeph://new-session", Some("1"));
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("deep-link dispatch loop detected; exiting"),
        "{stderr}"
    );
}

#[test]
fn fresh_dispatch_passes_the_guard() {
    let out = url_open("not-a-zeph-uri", None);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("dispatch loop detected"),
        "guard must not trip without the marker: {stderr}"
    );
    assert!(stderr.contains("invalid URI"), "{stderr}");
}

#[test]
fn non_one_marker_does_not_trip_the_guard() {
    let out = url_open("not-a-zeph-uri", Some("0"));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("dispatch loop detected"), "{stderr}");
}
