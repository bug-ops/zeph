//! Workspace configuration guards: lint configuration in the root `Cargo.toml` that has no
//! automated coverage from platform-specific CI runners (see #5961), and the Rust toolchain tag
//! of the `docker/Dockerfile*` builder stages (see #7174).

use std::path::PathBuf;

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

// ---- Dockerfile builder tag guard (#7174) ----

/// Returns the tag of a `FROM` line that pulls the `rust` image, `None` for any other line or
/// image. Handles `--flag` tokens, a `docker.io/library/` prefix, `@sha256:` digests and an
/// untagged image (reported as `latest`).
fn rust_from_tag(line: &str) -> Option<String> {
    let mut tokens = line.split_whitespace();
    if !tokens.next()?.eq_ignore_ascii_case("FROM") {
        return None;
    }
    let image = tokens.find(|t| !t.starts_with("--"))?;
    let image = image.strip_prefix("docker.io/library/").unwrap_or(image);
    let image = image.split_once('@').map_or(image, |(name, _digest)| name);
    let (name, tag) = image.split_once(':').unwrap_or((image, "latest"));
    (name == "rust").then(|| tag.to_owned())
}

/// Parses a numeric `X.Y[.Z]` tag (optionally followed by a `-variant`) into `(major, minor)`.
fn tag_major_minor(tag: &str) -> Result<(u32, u32), String> {
    let numeric = tag.split('-').next().unwrap_or_default();
    let parts: Vec<&str> = numeric.split('.').collect();
    if !(2..=3).contains(&parts.len()) {
        return Err(format!("tag {tag:?} is not a numeric X.Y[.Z] version"));
    }
    let nums: Result<Vec<u32>, _> = parts.iter().map(|p| p.parse::<u32>()).collect();
    match nums {
        Ok(n) => Ok((n[0], n[1])),
        Err(_) => Err(format!("tag {tag:?} is not a numeric X.Y[.Z] version")),
    }
}

fn workspace_rust_version() -> (u32, u32) {
    let manifest_path = concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml");
    let manifest = std::fs::read_to_string(manifest_path).expect("read root Cargo.toml");
    let parsed: toml::Table = manifest.parse().expect("parse root Cargo.toml as TOML");
    let msrv = parsed
        .get("workspace")
        .and_then(|w| w.get("package"))
        .and_then(|p| p.get("rust-version"))
        .and_then(|v| v.as_str())
        .expect("workspace.package.rust-version");
    tag_major_minor(msrv).expect("rust-version is numeric")
}

fn dockerfiles() -> Vec<PathBuf> {
    let dir = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/docker"));
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("read docker/")
        .map(|e| e.expect("dir entry").path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("Dockerfile"))
        })
        .collect();
    files.sort();
    files
}

#[test]
fn rust_from_tag_extracts_tag_from_supported_forms() {
    let cases = [
        (
            "FROM rust:1.100.0-slim@sha256:6f14b2b4 AS chef",
            "1.100.0-slim",
        ),
        (
            "FROM --platform=$BUILDPLATFORM rust:1.98-slim AS builder",
            "1.98-slim",
        ),
        ("FROM docker.io/library/rust:1.98.1 AS b", "1.98.1"),
        ("FROM rust:1.100.0-slim@sha256:6f14b2b4", "1.100.0-slim"),
        ("from rust:1.98", "1.98"),
        ("FROM rust", "latest"),
    ];
    for (line, tag) in cases {
        assert_eq!(rust_from_tag(line).as_deref(), Some(tag), "line: {line}");
    }
}

#[test]
fn rust_from_tag_ignores_other_images_and_stage_references() {
    for line in [
        "FROM busybox:1.38-musl@sha256:80e38f9a AS setup",
        "FROM gcr.io/distroless/static-debian12:nonroot@sha256:f5b485ea",
        "FROM chef AS planner",
        "COPY --from=rust:1.98 /a /b",
        "# FROM rust:1.1",
        "RUN echo FROM rust:1.1",
    ] {
        assert_eq!(rust_from_tag(line), None, "line: {line}");
    }
}

#[test]
fn tag_major_minor_compares_numerically_not_lexically() {
    let tag = tag_major_minor("1.100.0-slim").expect("numeric tag");
    assert_eq!(tag, (1, 100));
    assert!(tag >= (1, 98), "1.100 must be >= 1.98");
    assert!(tag_major_minor("1.97.1-slim").expect("numeric tag") < (1, 98));
}

#[test]
fn tag_major_minor_rejects_non_numeric_and_incomplete_tags() {
    for tag in ["latest", "1", "slim", "1.x", "bookworm", ""] {
        assert!(tag_major_minor(tag).is_err(), "tag: {tag:?}");
    }
}

/// Every `rust:` builder tag in `docker/Dockerfile*` must be a numeric version at least the
/// workspace `rust-version`; otherwise the image build fails on `rust-version` enforcement.
#[test]
fn dockerfile_rust_builder_tags_are_at_least_workspace_msrv() {
    let msrv = workspace_rust_version();
    let mut failures = Vec::new();
    for path in dockerfiles() {
        let text = std::fs::read_to_string(&path).expect("read Dockerfile");
        for line in text.lines().filter_map(rust_from_tag) {
            match tag_major_minor(&line) {
                Ok(v) if v >= msrv => {}
                Ok(v) => failures.push(format!(
                    "{}: rust:{line} is {}.{} < workspace rust-version {}.{}",
                    path.display(),
                    v.0,
                    v.1,
                    msrv.0,
                    msrv.1
                )),
                Err(e) => failures.push(format!("{}: {e}", path.display())),
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn dockerfile_dev_pins_at_least_one_rust_builder_image() {
    let text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/docker/Dockerfile.dev"
    ))
    .expect("read docker/Dockerfile.dev");
    assert!(
        text.lines().any(|l| rust_from_tag(l).is_some()),
        "docker/Dockerfile.dev has no rust: FROM line, the tag guard would pass vacuously"
    );
}

/// #7174: the dependency layer is cached with cargo-chef and its failures are not swallowed.
#[test]
fn dockerfile_dev_caches_dependencies_with_cargo_chef_and_fails_loudly() {
    let text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/docker/Dockerfile.dev"
    ))
    .expect("read docker/Dockerfile.dev");
    assert!(text.contains("cargo chef prepare"), "missing planner stage");
    assert!(
        text.contains("cargo chef cook"),
        "missing dependency cook step"
    );
    assert!(
        !text.contains("|| true"),
        "a failed dependency build must fail the image build"
    );
}
