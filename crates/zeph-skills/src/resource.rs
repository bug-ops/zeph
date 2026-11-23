// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Skill-local resource file discovery and safe loading.
//!
//! Skills may ship auxiliary files in three well-known subdirectories:
//!
//! | Subdirectory | Contents |
//! |---|---|
//! | `scripts/` | Shell scripts or helper programs |
//! | `references/` | Reference documents injected into skill context |
//! | `assets/` | Static files (images, data) |
//!
//! [`load_skill_resource`] enforces path canonicalization before loading, preventing
//! symlink-based path traversal attacks where a skill might try to read files outside
//! its own directory.

use std::fmt::Write as _;
use std::io::Read as _;
use std::path::{Component, Path, PathBuf};

use crate::error::SkillError;

/// Discovered resource files for a skill, organized by subdirectory.
///
/// All paths are absolute and sorted lexicographically within each category.
/// Entries are only included when the corresponding subdirectory exists.
#[derive(Clone, Debug, Default)]
pub struct SkillResources {
    /// Absolute paths of files in the `scripts/` subdirectory.
    pub scripts: Vec<PathBuf>,
    /// Absolute paths of files in the `references/` subdirectory.
    pub references: Vec<PathBuf>,
    /// Absolute paths of files in the `assets/` subdirectory.
    pub assets: Vec<PathBuf>,
}

/// Discover available resource directories for a skill.
#[must_use]
pub(crate) fn discover_resources(skill_dir: &Path) -> SkillResources {
    let mut resources = SkillResources::default();

    for (subdir, target) in [
        ("scripts", &mut resources.scripts),
        ("references", &mut resources.references),
        ("assets", &mut resources.assets),
    ] {
        let dir = skill_dir.join(subdir);
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_file() {
                    target.push(path);
                }
            }
            target.sort();
        }
    }

    resources
}

/// Upper bound in bytes for a single file read by [`load_skill_resource`] (256 KiB).
pub const MAX_SKILL_RESOURCE_BYTES: u64 = 256 * 1024;

const MAX_RESOURCE_PATH_BYTES: usize = 512;
const RESOURCE_DIRS: [&str; 3] = ["scripts", "references", "assets"];

/// A model-supplied, skill-relative resource path that passed shape validation.
///
/// Constructed only through [`SkillResourcePath::parse`], which rejects absolute paths,
/// parent-directory components, backslashes, NUL bytes, single-component paths and any first
/// component other than `scripts`, `references` or `assets`.
///
/// # Examples
///
/// ```
/// use zeph_skills::resource::SkillResourcePath;
///
/// assert!(SkillResourcePath::parse("references/guide.md").is_ok());
/// assert!(SkillResourcePath::parse("../secrets").is_err());
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillResourcePath(Box<str>);

impl SkillResourcePath {
    /// Parse and validate a skill-relative resource path.
    ///
    /// # Errors
    ///
    /// Returns [`SkillError::Invalid`] when the path has an unsafe or unsupported shape.
    pub fn parse(raw: &str) -> Result<Self, SkillError> {
        let invalid = |why: &str| SkillError::Invalid(format!("resource path rejected: {why}"));
        if raw.is_empty() {
            return Err(invalid("empty"));
        }
        if raw.len() > MAX_RESOURCE_PATH_BYTES {
            return Err(invalid("too long"));
        }
        if raw.contains(['\\', '\0']) {
            return Err(invalid("forbidden character"));
        }
        let mut components = Path::new(raw).components();
        let Some(Component::Normal(first)) = components.next() else {
            return Err(invalid("must start with a subdirectory name"));
        };
        if !RESOURCE_DIRS.iter().any(|dir| first == *dir) {
            return Err(invalid("must be under scripts/, references/ or assets/"));
        }
        let mut rest = 0_usize;
        for component in components {
            if !matches!(component, Component::Normal(_)) {
                return Err(invalid("only plain path segments are allowed"));
            }
            rest += 1;
        }
        if rest == 0 {
            return Err(invalid("a file name is required"));
        }
        Ok(Self(raw.into()))
    }

    /// The validated path as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// List a skill's resource files as skill-relative paths, one `Available <kind>: ...` line per kind.
///
/// Returns `None` when the skill has no resources. Files outside `skill_dir` are skipped, so
/// absolute paths never reach the prompt.
///
/// # Examples
///
/// ```
/// use std::path::Path;
/// use zeph_skills::resource::{SkillResources, format_resource_paths};
///
/// assert!(format_resource_paths(&SkillResources::default(), Path::new("/skills/x")).is_none());
/// ```
#[must_use]
pub fn format_resource_paths(resources: &SkillResources, skill_dir: &Path) -> Option<String> {
    let mut out = String::new();
    for (label, files) in [
        ("references", &resources.references),
        ("scripts", &resources.scripts),
        ("assets", &resources.assets),
    ] {
        let names: Vec<String> = files
            .iter()
            .filter_map(|p| {
                let rel = p.strip_prefix(skill_dir).ok()?;
                let parts: Option<Vec<&str>> =
                    rel.components().map(|c| c.as_os_str().to_str()).collect();
                Some(parts?.join("/"))
            })
            .collect();
        if !names.is_empty() {
            if !out.is_empty() {
                out.push('\n');
            }
            let _ = write!(out, "Available {label}: {}", names.join(", "));
        }
    }
    (!out.is_empty()).then_some(out)
}

/// [`format_resource_paths`] followed by a one-line `load_skill` usage hint.
///
/// Use it where the reader may not have the `load_skill` tool description (sub-agent skill
/// bodies); the main-agent prompt emits the bare paths, since the tool description already
/// documents the `path` argument.
///
/// # Examples
///
/// ```
/// use std::path::Path;
/// use zeph_skills::resource::{SkillResources, format_resource_listing};
///
/// assert!(format_resource_listing(&SkillResources::default(), Path::new("/skills/x")).is_none());
/// ```
#[must_use]
pub fn format_resource_listing(resources: &SkillResources, skill_dir: &Path) -> Option<String> {
    let mut out = format_resource_paths(resources, skill_dir)?;
    out.push_str(
        "\nRead one with load_skill {\"skill_name\": \"<this skill>\", \"path\": \"<path from the list>\"} (UTF-8 text files up to 256 KiB only; binary assets cannot be loaded)",
    );
    Some(out)
}

/// Load a skill resource file as a UTF-8 string with containment and size protection.
///
/// # Errors
///
/// Returns an error if the path escapes the skill directory, the file exceeds
/// [`MAX_SKILL_RESOURCE_BYTES`], is not UTF-8, or cannot be read.
pub fn load_skill_resource(
    skill_dir: &Path,
    path: &SkillResourcePath,
) -> Result<String, SkillError> {
    let canonical_base = skill_dir.canonicalize().map_err(|e| {
        SkillError::Other(format!(
            "failed to canonicalize skill dir {}: {e}",
            skill_dir.display()
        ))
    })?;
    let canonical_target = skill_dir.join(path.as_str()).canonicalize().map_err(|e| {
        SkillError::Other(format!("failed to resolve resource {}: {e}", path.as_str()))
    })?;
    if !canonical_target.starts_with(&canonical_base) {
        return Err(SkillError::Invalid(format!(
            "path traversal detected: {} escapes the skill directory",
            path.as_str()
        )));
    }

    // A FIFO or device swapped in after discovery would block `open`/`read` indefinitely.
    if !std::fs::metadata(&canonical_target)?.is_file() {
        return Err(SkillError::Invalid(format!(
            "resource {} is not a regular file",
            path.as_str()
        )));
    }
    let file = std::fs::File::open(&canonical_target)?;
    if !file.metadata()?.is_file() {
        return Err(SkillError::Invalid(format!(
            "resource {} is not a regular file",
            path.as_str()
        )));
    }
    let mut bytes = Vec::new();
    file.take(MAX_SKILL_RESOURCE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len())? > MAX_SKILL_RESOURCE_BYTES {
        return Err(SkillError::Invalid(format!(
            "resource {} exceeds {MAX_SKILL_RESOURCE_BYTES} bytes",
            path.as_str()
        )));
    }
    String::from_utf8(bytes)
        .map_err(|_| SkillError::Invalid(format!("resource {} is not valid UTF-8", path.as_str())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discover_empty_skill_dir() {
        let dir = tempfile::tempdir().unwrap();
        let resources = discover_resources(dir.path());
        assert_eq!(resources.scripts, [] as [std::path::PathBuf; 0]);
        assert_eq!(resources.references, [] as [std::path::PathBuf; 0]);
        assert_eq!(resources.assets, [] as [std::path::PathBuf; 0]);
    }

    #[test]
    fn discover_with_resources() {
        let dir = tempfile::tempdir().unwrap();
        let scripts = dir.path().join("scripts");
        std::fs::create_dir(&scripts).unwrap();
        std::fs::write(scripts.join("run.sh"), "#!/bin/bash").unwrap();

        let refs = dir.path().join("references");
        std::fs::create_dir(&refs).unwrap();
        std::fs::write(refs.join("doc.md"), "# Doc").unwrap();

        let assets = dir.path().join("assets");
        std::fs::create_dir(&assets).unwrap();
        std::fs::write(assets.join("logo.png"), [0u8; 4]).unwrap();

        let resources = discover_resources(dir.path());
        assert_eq!(resources.scripts.len(), 1);
        assert_eq!(resources.references.len(), 1);
        assert_eq!(resources.assets.len(), 1);
    }

    #[test]
    fn load_skill_resource_valid() {
        let dir = tempfile::tempdir().unwrap();
        let scripts = dir.path().join("scripts");
        std::fs::create_dir(&scripts).unwrap();
        std::fs::write(scripts.join("run.sh"), "echo hello").unwrap();

        let path = SkillResourcePath::parse("scripts/run.sh").unwrap();
        assert_eq!(
            load_skill_resource(dir.path(), &path).unwrap(),
            "echo hello"
        );
    }

    #[test]
    fn load_skill_resource_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let path = SkillResourcePath::parse("scripts/nonexistent.txt").unwrap();
        assert!(load_skill_resource(dir.path(), &path).is_err());
    }
}
