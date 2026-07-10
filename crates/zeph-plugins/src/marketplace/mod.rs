// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Pluggable skill/plugin discovery-and-fetch registry client (spec-045, #5869).
//!
//! This module defines the trait boundary a registry backend must implement to plug into
//! `zeph skill search`/`zeph skill get`/`zeph plugin search`/`zeph plugin get` (FR-005,
//! NFR-003). It intentionally stops at "search" and "fetch a package to a local directory" —
//! everything downstream (frontmatter validation, injection scanning, trust upsert) reuses the
//! existing [`zeph_skills::manager::SkillManager`] and [`crate::manager::PluginManager`]
//! install pipelines unchanged (NFR-002).
//!
//! # Compile-time gate
//!
//! The entire module body is gated by the `registry` Cargo feature (see `Cargo.toml`). This is
//! a *thin* feature: `reqwest` is already an unconditional dependency of this crate, and the
//! only additive surface it enables is `reqwest`'s own `query` Cargo feature (a thin wrapper
//! around `serde_urlencoded`, not a new crate) — the feature's purpose is to satisfy the
//! project convention that every new optional network capability gets a dedicated feature
//! flag, not to gate a heavyweight dependency. The CLI argument variants and
//! [`zeph_config::RegistryConfig`] parsing are **not** gated by this feature and always
//! compile, so `--help` and `--migrate-config` keep working in a build without it.
//!
//! # Backends
//!
//! - [`skills_sh::SkillsShClient`] — the only shipped backend, targeting the public
//!   [skills.sh](https://www.skills.sh) registry.
//! - `mock::MockRegistryClient` (test-only, not linkable here — compiled under
//!   `#[cfg(any(test, feature = "mock"))]`, see that module) — proves the trait boundary is
//!   real by providing a second, independent implementation (SC-004). The `mock` feature (not
//!   just `#[cfg(test)]`) exists so downstream crates like the `zeph` binary can depend on it
//!   from their own `dev-dependencies`, mirroring `zeph-vault`'s `MockVaultProvider` pattern.

use std::path::{Component, Path};
use std::pin::Pin;

pub mod skills_sh;

#[cfg(any(test, feature = "mock"))]
pub mod mock;

/// A single search result returned by a registry backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryEntry {
    /// Backend-assigned identifier, opaque to the caller. Pass verbatim to
    /// [`RegistryClient::fetch`].
    pub registry_id: String,
    /// Human-readable name.
    pub name: String,
    /// Short description.
    pub description: String,
    /// Free-form tags/categories, when the backend provides them.
    pub tags: Vec<String>,
    /// Author or publisher, when the backend provides it.
    pub author: Option<String>,
    /// Security audit status string, when the backend provides one (e.g. `"pass"`, `"warn"`,
    /// `"fail"`). `None` when the backend has no audit concept or the search response did not
    /// include it.
    pub security_audit_status: Option<String>,
}

/// A fetched package, extracted to a local temporary directory.
///
/// Ownership of `extracted_dir` is returned to the caller; the directory (and everything in
/// it) is deleted when this value is dropped. Callers that want to keep it must install the
/// contents elsewhere (e.g. via [`zeph_skills::manager::SkillManager::install_from_path`])
/// before dropping this value.
pub struct PackageArchive {
    /// The `registry_id` this package was fetched for.
    pub registry_id: String,
    /// Temporary directory owning the package's materialized files. Do not pass this path
    /// directly to `SkillManager::install_from_path`/`PluginManager::add` — use the
    /// `install_dir` field instead (see its docs for why).
    pub extracted_dir: tempfile::TempDir,
    /// `true` when the package contains a `plugin.toml` (a Zeph plugin package), `false` when
    /// it is a bare `SKILL.md` package.
    pub has_plugin_manifest: bool,
    /// The directory to pass to `SkillManager::install_from_path` (bare skill package) or
    /// `PluginManager::add` (plugin bundle).
    ///
    /// For a **bare skill package** this is a subdirectory of `extracted_dir` named after the
    /// skill's frontmatter `name` — `zeph_skills::loader::load_skill_meta`'s
    /// `validate_skill_name` requires the source directory's own basename to equal the
    /// declared skill name, and `extracted_dir`'s basename is an OS-assigned random string that
    /// will never satisfy that by chance. Without this indirection, `SkillManager::install_from_path`
    /// would fail with "skill name '<x>' does not match directory name '<random-tmp-name>'" for
    /// every real registry fetch — found via this crate's own test suite, not a live session
    /// (fix accompanying review issue #4's testability work).
    ///
    /// For a **plugin bundle** this is `extracted_dir.path()` itself — `PluginManager::add`
    /// does not require its source directory to be named any particular way.
    pub install_dir: std::path::PathBuf,
}

impl std::fmt::Debug for PackageArchive {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PackageArchive")
            .field("registry_id", &self.registry_id)
            .field("extracted_dir", &self.extracted_dir.path())
            .field("has_plugin_manifest", &self.has_plugin_manifest)
            .field("install_dir", &self.install_dir)
            .finish()
    }
}

/// Errors that can occur during a registry search or fetch.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    /// The HTTP request itself failed (connection, DNS, TLS).
    #[error("registry request failed: {0}")]
    Request(String),

    /// The request timed out.
    #[error("registry request timed out")]
    Timeout,

    /// The backend returned a non-success HTTP status not otherwise classified below.
    #[error("registry returned HTTP {status}: {body}")]
    Backend { status: u16, body: String },

    /// The backend requires authentication and no credential was configured, or the configured
    /// credential was rejected.
    #[error(
        "registry requires authentication; set skills.registry.auth_vault_key in config.toml \
         and store the token with `zeph vault set <KEY> <token>`"
    )]
    AuthRequired,

    /// The requested `registry_id` does not exist in the backend.
    #[error("package {0:?} not found in registry")]
    NotFound(String),

    /// The backend's response body could not be parsed into the expected shape.
    #[error("registry response could not be parsed: {0}")]
    InvalidResponse(String),

    /// A response (search page or package detail) exceeded the accepted size limit, or a
    /// package contained more files or total bytes than the accepted cap. Rejected before the
    /// body is read into memory / before a file is written, to prevent OOM or disk/inode
    /// exhaustion from a malicious, compromised, or MITM'd registry (review fix #2).
    #[error("registry response exceeded the size limit: {0}")]
    TooLarge(String),

    /// A package file path in the response was unsafe to materialize (absolute or contained a
    /// `..` component).
    #[error("registry package contains an unsafe file path: {0}")]
    UnsafePath(String),

    /// `skills.registry.backend_url` (or a value derived from it) uses a scheme other than
    /// `http`/`https` (review fix #6 — SSRF hardening).
    #[error("unsafe registry backend URL: {0}")]
    UnsafeBackendUrl(String),

    /// A user-supplied `registry_id` contained characters (`?`, `#`, whitespace) that could
    /// alter the request URL's query string or fragment when interpolated into the path
    /// (review fix #8).
    #[error("invalid registry id {0:?}: must not contain '?', '#', or whitespace")]
    InvalidRegistryId(String),

    /// A filesystem operation failed while materializing the fetched package.
    #[error("filesystem error: {0}")]
    Io(#[from] std::io::Error),
}

/// Pluggable registry backend for skill/plugin discovery and fetch (FR-005, NFR-003).
///
/// Implement this trait to integrate a private or self-hosted registry. The crate ships
/// [`skills_sh::SkillsShClient`] out of the box.
///
/// Uses boxed futures (rather than native `async fn` in trait) because callers need to select
/// a backend at runtime behind `dyn RegistryClient` based on `[skills.registry] backend_kind` —
/// this mirrors the `zeph_vault::VaultProvider` pattern already used elsewhere in this
/// workspace for the same reason (not an intra-doc link: `zeph-vault` is not a dependency of
/// this crate; async trait objects are not natively object-safe, which is why both traits use
/// boxed futures instead of native `async fn` in trait).
///
/// # Implementing
///
/// ```
/// use std::pin::Pin;
/// use std::future::Future;
/// use zeph_plugins::marketplace::{RegistryClient, RegistryEntry, RegistryError, PackageArchive};
///
/// struct EmptyRegistry;
///
/// impl RegistryClient for EmptyRegistry {
///     fn search(
///         &self,
///         _query: &str,
///     ) -> Pin<Box<dyn Future<Output = Result<Vec<RegistryEntry>, RegistryError>> + Send + '_>> {
///         Box::pin(async move { Ok(Vec::new()) })
///     }
///
///     fn fetch(
///         &self,
///         registry_id: &str,
///     ) -> Pin<Box<dyn Future<Output = Result<PackageArchive, RegistryError>> + Send + '_>> {
///         let id = registry_id.to_owned();
///         Box::pin(async move { Err(RegistryError::NotFound(id)) })
///     }
/// }
/// ```
pub trait RegistryClient: Send + Sync {
    /// Search the registry by free-text `query`.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError`] on network failure, timeout, or an unparseable response.
    fn search(
        &self,
        query: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<RegistryEntry>, RegistryError>> + Send + '_>>;

    /// Fetch the package identified by `registry_id` and materialize it into a local temporary
    /// directory.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError::NotFound`] when the id does not exist, and other
    /// [`RegistryError`] variants on network, auth, or parsing failure.
    fn fetch(
        &self,
        registry_id: &str,
    ) -> Pin<Box<dyn Future<Output = Result<PackageArchive, RegistryError>> + Send + '_>>;
}

/// Maximum number of files a single registry-fetched package may contain.
///
/// Defense in depth independent of `skills_sh`'s own response-size pre-check (not an intra-doc
/// link: that constant is private to its module): `write_files_safe` is a shared primitive any
/// future [`RegistryClient`] backend could call, not necessarily one that already capped its
/// HTTP response size the same way (review fix #2).
const MAX_PACKAGE_FILES: usize = 500;

/// Maximum total byte size across all files in a single registry-fetched package.
const MAX_PACKAGE_TOTAL_BYTES: u64 = 16 * 1024 * 1024;

/// Write `files` (relative path, content) pairs into `dest`, creating parent directories as
/// needed.
///
/// Rejects any path that is absolute or contains a `..` component — the same tar-slip class of
/// protection [`crate::manager::security`]'s `extract_archive_safe` applies to tar entries,
/// applied here to registry-supplied JSON file lists instead (skills.sh serves package
/// contents inline in JSON rather than as a tar archive; see `skills_sh` module docs). Also
/// rejects a package with more than [`MAX_PACKAGE_FILES`] entries or more than
/// [`MAX_PACKAGE_TOTAL_BYTES`] combined content bytes, before any file is written (review fix
/// #2 — prevents disk/inode exhaustion from a malicious or compromised registry).
///
/// # Errors
///
/// Returns [`RegistryError::TooLarge`] when the file-count or total-bytes cap is exceeded,
/// [`RegistryError::UnsafePath`] for a rejected entry, or [`RegistryError::Io`] if a file
/// cannot be written.
pub(crate) fn write_files_safe(
    dest: &Path,
    files: &[(String, String)],
) -> Result<(), RegistryError> {
    if files.len() > MAX_PACKAGE_FILES {
        return Err(RegistryError::TooLarge(format!(
            "package contains {} files (max {MAX_PACKAGE_FILES})",
            files.len()
        )));
    }
    let total_bytes: u64 = files.iter().map(|(_, content)| content.len() as u64).sum();
    if total_bytes > MAX_PACKAGE_TOTAL_BYTES {
        return Err(RegistryError::TooLarge(format!(
            "package content totals {total_bytes} bytes (max {MAX_PACKAGE_TOTAL_BYTES})"
        )));
    }
    for (rel_path, content) in files {
        let candidate = Path::new(rel_path);
        if candidate.is_absolute()
            || candidate
                .components()
                .any(|c| matches!(c, Component::ParentDir))
        {
            return Err(RegistryError::UnsafePath(rel_path.clone()));
        }
        let target = dest.join(candidate);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&target, content)?;
    }
    Ok(())
}

/// Reject a registry-supplied skill `name` before it is ever joined onto a filesystem path.
///
/// # Security
///
/// `name` originates from an untrusted registry response's `SKILL.md` frontmatter, parsed via
/// [`zeph_skills::loader::load_skill_meta_from_str`] — the *string*-based parser, chosen
/// specifically because it has no directory context and therefore does **not** call
/// `zeph_skills::loader`'s private `validate_skill_name` (that check only runs on the
/// file-path-based `load_skill_meta`, once a directory already exists to compare against).
/// This function is the substitute gate that must run before `name` is used anywhere near a
/// path, most importantly `tmp_root.join(name)` in [`materialize_package`]. `PathBuf::join`
/// has two exploitable behaviors with an unvalidated `name`: a relative-traversal segment like
/// `"../../etc"` walks the join outside `tmp_root`, and an **absolute** path like
/// `"/etc/cron.d"` *replaces* the base path outright — `tmp_root.join("/etc/cron.d")` yields
/// exactly `/etc/cron.d`, not a path under `tmp_root`. Either one, unguarded, turns a
/// malicious/MITM'd registry response into an arbitrary-file-write primitive once
/// `write_files_safe` writes the package's files under the resulting `install_dir`.
///
/// # Errors
///
/// Returns [`RegistryError::InvalidResponse`] when `name` is empty, is `"."`, contains a path
/// separator (`/` or `\`), or contains a `..` component.
fn validate_registry_skill_name(name: &str) -> Result<(), RegistryError> {
    if name.is_empty() || name == "." || name.contains(['/', '\\']) || name.contains("..") {
        return Err(RegistryError::InvalidResponse(format!(
            "package SKILL.md declares an unsafe name {name:?}"
        )));
    }
    Ok(())
}

/// Materialize `files` under `tmp_root` and determine the directory to pass to
/// `SkillManager::install_from_path`/`PluginManager::add` — see [`PackageArchive`]'s
/// `install_dir` field docs for why a bare skill package needs a different directory than a
/// plugin bundle.
///
/// Classification is based on the presence of a `"plugin.toml"` entry in `files` (checked
/// before any write, not via a post-write filesystem scan — equally correct here since `files`
/// is already the full authoritative entry list, and avoids a redundant filesystem read).
///
/// # Errors
///
/// Returns [`RegistryError::InvalidResponse`] when a bare skill package (no `plugin.toml`) has
/// no `SKILL.md` entry, its frontmatter cannot be parsed, or its declared `name` fails
/// [`validate_registry_skill_name`] (see that function's docs for why this check is mandatory
/// and cannot be delegated to `SkillManager::install_from_path`'s later validation). Failing
/// fast with a clear message here is preferable to letting the caller's later
/// `SkillManager::install_from_path` call fail with a confusing directory-name-mismatch error.
/// Otherwise propagates [`write_files_safe`]'s errors.
pub(crate) fn materialize_package(
    tmp_root: &Path,
    files: &[(String, String)],
) -> Result<(bool, std::path::PathBuf), RegistryError> {
    let has_plugin_manifest = files.iter().any(|(path, _)| path == "plugin.toml");
    if has_plugin_manifest {
        write_files_safe(tmp_root, files)?;
        return Ok((true, tmp_root.to_path_buf()));
    }

    let skill_md_content = files
        .iter()
        .find(|(path, _)| path == "SKILL.md")
        .map(|(_, content)| content.as_str())
        .ok_or_else(|| {
            RegistryError::InvalidResponse(
                "package has neither plugin.toml nor SKILL.md at its root".to_owned(),
            )
        })?;
    let (meta, _body) = zeph_skills::loader::load_skill_meta_from_str(skill_md_content)
        .map_err(|e| RegistryError::InvalidResponse(format!("invalid SKILL.md: {e}")))?;
    validate_registry_skill_name(&meta.name)?;

    let install_dir = tmp_root.join(&meta.name);
    write_files_safe(&install_dir, files)?;
    Ok((false, install_dir))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_files_safe_rejects_absolute_path() {
        let dir = tempfile::tempdir().unwrap();
        let files = vec![("/etc/passwd".to_owned(), "evil".to_owned())];
        let err = write_files_safe(dir.path(), &files).unwrap_err();
        assert!(matches!(err, RegistryError::UnsafePath(_)));
    }

    #[test]
    fn write_files_safe_rejects_parent_dir_traversal() {
        let dir = tempfile::tempdir().unwrap();
        let files = vec![("../../etc/passwd".to_owned(), "evil".to_owned())];
        let err = write_files_safe(dir.path(), &files).unwrap_err();
        assert!(matches!(err, RegistryError::UnsafePath(_)));
    }

    #[test]
    fn write_files_safe_writes_nested_files() {
        let dir = tempfile::tempdir().unwrap();
        let files = vec![
            ("SKILL.md".to_owned(), "---\nname: x\n---\nbody".to_owned()),
            ("scripts/run.sh".to_owned(), "#!/bin/sh\necho hi".to_owned()),
        ];
        write_files_safe(dir.path(), &files).unwrap();
        assert!(dir.path().join("SKILL.md").is_file());
        assert!(dir.path().join("scripts/run.sh").is_file());
    }

    #[test]
    fn materialize_package_installs_skill_into_name_matching_subdir() {
        let dir = tempfile::tempdir().unwrap();
        let files = vec![(
            "SKILL.md".to_owned(),
            "---\nname: pdf-tools\ndescription: a test skill\n---\nbody".to_owned(),
        )];
        let (has_plugin_manifest, install_dir) = materialize_package(dir.path(), &files).unwrap();
        assert!(!has_plugin_manifest);
        assert_eq!(install_dir, dir.path().join("pdf-tools"));
        assert!(install_dir.join("SKILL.md").is_file());
    }

    #[test]
    fn materialize_package_installs_plugin_bundle_at_root() {
        let dir = tempfile::tempdir().unwrap();
        let files = vec![
            (
                "plugin.toml".to_owned(),
                "[plugin]\nname = \"x\"\nversion = \"0.1.0\"".to_owned(),
            ),
            (
                "skills/y/SKILL.md".to_owned(),
                "---\nname: y\ndescription: a test skill\n---\nbody".to_owned(),
            ),
        ];
        let (has_plugin_manifest, install_dir) = materialize_package(dir.path(), &files).unwrap();
        assert!(has_plugin_manifest);
        assert_eq!(install_dir, dir.path());
        assert!(install_dir.join("plugin.toml").is_file());
    }

    #[test]
    fn materialize_package_rejects_package_with_neither_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let files = vec![("README.md".to_owned(), "hello".to_owned())];
        let err = materialize_package(dir.path(), &files).unwrap_err();
        assert!(matches!(err, RegistryError::InvalidResponse(_)));
    }

    #[test]
    fn materialize_package_rejects_unparseable_skill_md() {
        let dir = tempfile::tempdir().unwrap();
        let files = vec![("SKILL.md".to_owned(), "not frontmatter at all".to_owned())];
        let err = materialize_package(dir.path(), &files).unwrap_err();
        assert!(matches!(err, RegistryError::InvalidResponse(_)));
    }

    // ── Arbitrary-file-write via unvalidated SKILL.md `name` (security fix) ──

    #[test]
    fn validate_registry_skill_name_rejects_parent_dir_traversal() {
        let err = validate_registry_skill_name("../../../../etc").unwrap_err();
        assert!(matches!(err, RegistryError::InvalidResponse(_)));
    }

    #[test]
    fn validate_registry_skill_name_rejects_absolute_path() {
        let err = validate_registry_skill_name("/etc/cron.d").unwrap_err();
        assert!(matches!(err, RegistryError::InvalidResponse(_)));
    }

    #[test]
    fn validate_registry_skill_name_rejects_backslash_traversal() {
        let err = validate_registry_skill_name("..\\..\\Windows").unwrap_err();
        assert!(matches!(err, RegistryError::InvalidResponse(_)));
    }

    #[test]
    fn validate_registry_skill_name_rejects_empty_and_dot() {
        assert!(validate_registry_skill_name("").is_err());
        assert!(validate_registry_skill_name(".").is_err());
    }

    #[test]
    fn validate_registry_skill_name_accepts_normal_name() {
        validate_registry_skill_name("pdf-tools").unwrap();
    }

    #[test]
    fn materialize_package_rejects_traversal_name_with_zero_filesystem_writes() {
        let dir = tempfile::tempdir().unwrap();
        let files = vec![(
            "SKILL.md".to_owned(),
            "---\nname: ../../../../tmp/zeph-marketplace-poc\ndescription: evil\n---\nbody"
                .to_owned(),
        )];
        let err = materialize_package(dir.path(), &files).unwrap_err();
        assert!(matches!(err, RegistryError::InvalidResponse(_)));
        // Prove nothing escaped tmp_root: the tempdir must still be completely empty — not
        // just that install_dir construction "failed" in isolation, but that no write ever
        // reached the filesystem for this malicious package.
        assert!(
            dir.path().read_dir().unwrap().next().is_none(),
            "tmp_root must be empty after a rejected malicious name — no partial write"
        );
        assert!(
            !std::path::Path::new("/tmp/zeph-marketplace-poc").exists(),
            "traversal must not have escaped to /tmp"
        );
    }

    #[test]
    fn materialize_package_rejects_absolute_path_name_with_zero_filesystem_writes() {
        let dir = tempfile::tempdir().unwrap();
        let poc_path = "/tmp/zeph-marketplace-poc-absolute";
        let files = vec![(
            "SKILL.md".to_owned(),
            format!("---\nname: {poc_path}\ndescription: evil\n---\nbody"),
        )];
        let err = materialize_package(dir.path(), &files).unwrap_err();
        assert!(matches!(err, RegistryError::InvalidResponse(_)));
        assert!(
            dir.path().read_dir().unwrap().next().is_none(),
            "tmp_root must be empty after a rejected malicious name — no partial write"
        );
        assert!(
            !std::path::Path::new(poc_path).exists(),
            "absolute-path name must not have replaced the base path and written to {poc_path}"
        );
    }

    #[test]
    fn write_files_safe_rejects_too_many_files() {
        let dir = tempfile::tempdir().unwrap();
        let files: Vec<(String, String)> = (0..=MAX_PACKAGE_FILES)
            .map(|i| (format!("f{i}.txt"), "x".to_owned()))
            .collect();
        let err = write_files_safe(dir.path(), &files).unwrap_err();
        assert!(matches!(err, RegistryError::TooLarge(_)));
    }

    #[test]
    fn write_files_safe_rejects_excessive_total_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let huge = "x".repeat(usize::try_from(MAX_PACKAGE_TOTAL_BYTES).unwrap() + 1);
        let files = vec![("big.txt".to_owned(), huge)];
        let err = write_files_safe(dir.path(), &files).unwrap_err();
        assert!(matches!(err, RegistryError::TooLarge(_)));
    }

    #[test]
    fn write_files_safe_accepts_package_at_the_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let files: Vec<(String, String)> = (0..MAX_PACKAGE_FILES)
            .map(|i| (format!("f{i}.txt"), "x".to_owned()))
            .collect();
        write_files_safe(dir.path(), &files).unwrap();
        assert!(dir.path().join("f0.txt").is_file());
    }
}
