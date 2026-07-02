// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Regression coverage for #5394: `zeph --daemon` used to stack-overflow on startup
//! because the whole async runtime ran on the OS main thread, whose stack size is
//! governed by the caller's `ulimit -s` (8 MiB default on macOS/Linux). See
//! `src/main.rs` for the fix (dedicated 32 MiB-stack thread).

/// Kills the wrapped child process (and waits briefly for it to exit) on drop, so the
/// daemon is never left running after a test completes or an assertion panics mid-test.
#[cfg(all(unix, feature = "a2a"))]
struct KillOnDrop(std::process::Child);

#[cfg(all(unix, feature = "a2a"))]
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Reproduces a constrained OS main-thread stack ulimit — the class of environment that
/// crashed every `--daemon` invocation before the fix (originally reported at the macOS/
/// Linux default of `ulimit -s 8192`, 8 MiB) — and asserts the daemon is still alive a few
/// seconds after startup instead of having aborted with SIGSEGV/SIGABRT (exit 134,
/// "thread 'main' has overflowed its stack").
///
/// Uses `ulimit -s 4096` (4 MiB) rather than the originally-reported 8 MiB: empirically,
/// the exact crash threshold shifts with the compiled feature set (`Config`'s frame size
/// depends on which feature-gated sub-configs are compiled in), and 8 MiB does not
/// reliably reproduce the crash under every feature combination this test may be built
/// with. 4 MiB reproduced the pre-fix crash reliably across all feature sets tested,
/// while comfortably fitting under the fix's 32 MiB dedicated-thread stack (8x margin) —
/// and after the fix the OS main thread's own footprint is negligible (it only spawns and
/// joins the worker thread), so the exact ulimit no longer matters.
#[cfg(all(unix, feature = "a2a"))]
#[test]
fn daemon_boots_under_constrained_stack_ulimit() {
    let tmp = tempfile::tempdir().expect("create tempdir");
    let config_path = tmp.path().join("daemon-boot.toml");

    // `Config`'s top-level fields have no struct-wide `#[serde(default)]`, so a
    // partial TOML document fails to parse once the file exists on disk (only a
    // *missing* file falls back to `Config::default()`). Start from a full dump
    // of the defaults and patch only the paths that must stay inside the tempdir.
    let mut doc: toml_edit::DocumentMut = zeph_core::config::Config::dump_defaults()
        .expect("dump default config")
        .parse()
        .expect("parse default config toml");
    doc["memory"]["sqlite_path"] =
        toml_edit::value(tmp.path().join("zeph.db").display().to_string());
    doc["daemon"]["pid_file"] = toml_edit::value(tmp.path().join("zeph.pid").display().to_string());
    doc["skills"]["paths"] = toml_edit::value(toml_edit::Array::from_iter([tmp
        .path()
        .join("skills")
        .display()
        .to_string()]));
    std::fs::write(&config_path, doc.to_string()).expect("write test config");

    let bin = zeph_bin_path();
    let child = std::process::Command::new("bash")
        .arg("-c")
        .arg(format!(
            "ulimit -s 4096 && exec {bin} --config {config} --daemon --bare",
            bin = shell_escape(&bin),
            config = shell_escape(&config_path.display().to_string()),
        ))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn zeph --daemon under bash");
    let mut child = KillOnDrop(child);

    std::thread::sleep(std::time::Duration::from_secs(3));

    match child.0.try_wait().expect("try_wait") {
        None => {} // still running — the crash under investigation never happens
        Some(status) => panic!(
            "daemon exited early with {status:?} instead of staying up \
             (pre-fix symptom: stack overflow, exit 134)"
        ),
    }
}

/// Cheap tripwire against reintroducing `#[tokio::main]` on the OS main thread, which
/// would silently drop the stack-size safety margin added for #5394.
#[test]
fn main_rs_drives_runtime_from_dedicated_stack_thread() {
    let main_rs = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/main.rs"))
        .expect("read src/main.rs");
    assert!(
        main_rs.contains("stack_size("),
        "src/main.rs must spawn the runtime on a thread with an explicit stack_size (see #5394)"
    );
    assert!(
        !main_rs.contains("#[tokio::main]"),
        "src/main.rs must not run the async runtime via #[tokio::main] on the OS main \
         thread — its stack size is bounded by the caller's ulimit -s and can overflow \
         (see #5394); use the dedicated stack_size thread instead"
    );
}

/// Resolves the path to the built `zeph` binary at runtime rather than baking it in via the
/// `CARGO_BIN_EXE_zeph` compile-time macro, which breaks under `cargo nextest`'s
/// archive-and-restore CI flow (the path is captured at build time on a different runner and
/// is no longer valid after `--workspace-remap`). `NEXTEST_BIN_EXE_zeph` is nextest's
/// archive-aware runtime variable; `CARGO_BIN_EXE_zeph` read as a runtime env var (not via
/// `env!`) is the fallback for plain `cargo test`/`cargo nextest run` without an archive.
#[cfg(all(unix, feature = "a2a"))]
fn zeph_bin_path() -> String {
    std::env::var("NEXTEST_BIN_EXE_zeph")
        .or_else(|_| std::env::var("CARGO_BIN_EXE_zeph"))
        .expect(
            "NEXTEST_BIN_EXE_zeph or CARGO_BIN_EXE_zeph must be set by the test runner \
             (cargo test / cargo nextest run / cargo nextest run --archive-file)",
        )
}

/// Minimal single-quote shell escaping for interpolating paths into the `bash -c` string.
#[cfg(all(unix, feature = "a2a"))]
fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}
