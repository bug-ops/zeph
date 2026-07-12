//! Guards workspace-level lint configuration in the root `Cargo.toml` that has
//! no automated coverage from platform-specific CI runners (see #5961).

/// `workspace.lints.rust.linker_messages` must stay `"allow"`.
///
/// `ci.yml` (the workflow gating every push/PR to `main`) is Linux-only, and
/// `ci-non-linux.yml` — the workflow that builds on macOS/arm64 and would
/// otherwise catch a regression here — only runs on manual `workflow_dispatch`.
/// Without this test, an accidental removal of the `allow` would stay green on
/// every PR and only surface as a build failure on the next manual macOS run
/// or tagged release build. This test inspects the TOML content directly, so
/// it runs on any OS, including Linux CI.
#[test]
fn workspace_lints_keep_linker_messages_allowed() {
    let manifest_path = concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml");
    let manifest = std::fs::read_to_string(manifest_path).expect("read root Cargo.toml");
    let parsed: toml::Table = manifest.parse().expect("parse root Cargo.toml as TOML");

    let linker_messages = parsed
        .get("workspace")
        .and_then(|w| w.get("lints"))
        .and_then(|l| l.get("rust"))
        .and_then(|r| r.get("linker_messages"))
        .and_then(|v| v.as_str());

    assert_eq!(
        linker_messages,
        Some("allow"),
        "workspace.lints.rust.linker_messages must stay \"allow\" (see #5961, #5895): \
         it suppresses Apple ld's arm64-only __eh_frame section warning that \
         build.warnings = \"deny\" (.cargo/config.toml) would otherwise escalate to a hard \
         build failure on macOS/arm64. ci.yml (the PR/push gate) is Linux-only and cannot \
         detect this in the linker itself — this test is the only automated per-PR guard. \
         Do not remove this assertion without re-reading #5961."
    );
}
