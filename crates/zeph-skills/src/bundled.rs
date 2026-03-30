// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Compile-time embedded bundled skills and startup provisioning.
//!
//! Bundled skills are embedded into the binary at compile time via [`include_dir!`].
//! On every startup, [`provision_bundled_skills`] ensures the managed skills directory
//! contains up-to-date copies of all bundled skills.
//!
//! # Provenance tracking
//!
//! Each provisioned skill gets a `.bundled` marker file alongside `SKILL.md`.
//! The marker contains the skill version that was provisioned. If the version in
//! the marker differs from the embedded version, the skill is re-provisioned.
//! Skills without a `.bundled` marker are treated as user-owned and never touched.
//!
//! # Binary rollback
//!
//! Rolling back to an older binary will downgrade bundled skills to the older
//! embedded versions. This is intentional — the binary and its bundled skills
//! are a single release artifact.

use std::fs;
use std::path::Path;

use include_dir::{Dir, include_dir};
use tracing::{debug, info, warn};

static BUNDLED_SKILLS_DIR: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/skills");

/// Summary of a single provisioning run.
#[derive(Debug, Default)]
pub struct ProvisionReport {
    /// Skills newly written to the managed dir (were absent).
    pub installed: Vec<String>,
    /// Skills re-written because the embedded version differed from the marker.
    pub updated: Vec<String>,
    /// Skills skipped because no `.bundled` marker exists (user-owned).
    pub skipped: Vec<String>,
    /// Skills that could not be provisioned — (name, error message).
    pub failed: Vec<(String, String)>,
}

/// Provision bundled skills to `managed_dir`.
///
/// Iterates over all embedded top-level skill directories (those containing a
/// `SKILL.md` file). For each skill:
/// - If the skill directory is absent → install it.
/// - If the skill directory is present and has a `.bundled` marker whose version
///   differs from the embedded version → update it.
/// - If the skill directory is present but has no `.bundled` marker → skip (user-owned).
///
/// All per-skill errors are non-fatal: they are collected in `report.failed` and
/// provisioning continues for the remaining skills.
///
/// # Errors
///
/// Returns an error only if `managed_dir` cannot be created.
pub fn provision_bundled_skills(managed_dir: &Path) -> Result<ProvisionReport, std::io::Error> {
    fs::create_dir_all(managed_dir)?;

    let mut report = ProvisionReport::default();

    for entry in BUNDLED_SKILLS_DIR.entries() {
        let include_dir::DirEntry::Dir(skill_dir) = entry else {
            continue; // skip top-level files (e.g. README.md)
        };

        let skill_name = skill_dir.path().to_string_lossy().into_owned();
        // In include_dir 0.7, get_file() takes a path relative to the embedded
        // root, not relative to the Dir itself.
        let skill_md_path = format!("{skill_name}/SKILL.md");

        // Filter: only process entries that contain a SKILL.md file.
        if BUNDLED_SKILLS_DIR.get_file(&skill_md_path).is_none() {
            debug!(skill = %skill_name, "skipping embedded entry without SKILL.md");
            continue;
        }

        let embedded_version = extract_embedded_version(skill_dir);
        let target_dir = managed_dir.join(&skill_name);
        let marker_path = target_dir.join(".bundled");

        if !target_dir.exists() {
            // Skill is absent — install it.
            match write_skill(skill_dir, &target_dir, &marker_path, &embedded_version) {
                Ok(()) => {
                    info!(skill = %skill_name, version = %embedded_version, "installed bundled skill");
                    report.installed.push(skill_name);
                }
                Err(e) => {
                    warn!(skill = %skill_name, error = %e, "failed to install bundled skill");
                    report.failed.push((skill_name, e.to_string()));
                }
            }
            continue;
        }

        // Skill dir exists — check marker.
        match read_marker_version(&marker_path) {
            MarkerState::NoMarker => {
                // Check if this is a legacy bundled skill (provisioned before the
                // .bundled marker system): compare on-disk SKILL.md to embedded.
                if is_legacy_bundled(&target_dir, &skill_name) {
                    match write_skill(skill_dir, &target_dir, &marker_path, &embedded_version) {
                        Ok(()) => {
                            info!(
                                skill = %skill_name,
                                to = %embedded_version,
                                "migrated legacy bundled skill (added .bundled marker)"
                            );
                            report.updated.push(skill_name);
                        }
                        Err(e) => {
                            warn!(skill = %skill_name, error = %e, "failed to migrate legacy bundled skill");
                            report.failed.push((skill_name, e.to_string()));
                        }
                    }
                } else {
                    // User-owned skill — never overwrite.
                    debug!(skill = %skill_name, "skipping user-owned skill (no .bundled marker)");
                    report.skipped.push(skill_name);
                }
            }
            MarkerState::CorruptMarker => {
                warn!(
                    skill = %skill_name,
                    "corrupt .bundled marker — treating skill as user-owned, skipping"
                );
                report.skipped.push(skill_name);
            }
            MarkerState::Version(marker_version) => {
                // Use != so both upgrades and rollbacks re-provision.
                if marker_version != embedded_version {
                    match write_skill(skill_dir, &target_dir, &marker_path, &embedded_version) {
                        Ok(()) => {
                            info!(
                                skill = %skill_name,
                                from = %marker_version,
                                to = %embedded_version,
                                "updated bundled skill"
                            );
                            report.updated.push(skill_name);
                        }
                        Err(e) => {
                            warn!(skill = %skill_name, error = %e, "failed to update bundled skill");
                            report.failed.push((skill_name, e.to_string()));
                        }
                    }
                }
                // else: already current, nothing to do.
            }
        }
    }

    if report.installed.is_empty() && report.updated.is_empty() && report.failed.is_empty() {
        debug!(
            skipped = report.skipped.len(),
            "all bundled skills are up to date"
        );
    }

    Ok(report)
}

// --- helpers -----------------------------------------------------------------

/// Write all files from an embedded skill dir to `target_dir` atomically.
///
/// All files (including the `.bundled` marker) are first written to a sibling
/// temp directory, then the temp directory is renamed into place in a single
/// `fs::rename` call. Because the rename is atomic on the same filesystem,
/// a process killed mid-write leaves no partial `target_dir` — the absent
/// directory is re-provisioned on the next startup.
fn write_skill(
    skill_dir: &include_dir::Dir<'_>,
    target_dir: &Path,
    marker_path: &Path,
    version: &str,
) -> Result<(), std::io::Error> {
    // Write to a sibling temp dir, then atomically rename.
    let parent = target_dir.parent().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "target_dir has no parent")
    })?;
    let tmp_name = format!(
        ".zeph-provision-tmp-{}",
        target_dir
            .file_name()
            .map_or("skill", |n| n.to_str().unwrap_or("skill"))
    );
    let tmp_dir = parent.join(&tmp_name);

    // Clean up any leftover temp dir from a previous interrupted run.
    if tmp_dir.exists() {
        fs::remove_dir_all(&tmp_dir)?;
    }
    fs::create_dir_all(&tmp_dir)?;

    // Write all embedded files into the temp dir.
    write_dir_contents(skill_dir, &tmp_dir)?;

    // Write the .bundled marker inside the temp dir (atomic move covers it).
    let tmp_marker = tmp_dir.join(".bundled");
    fs::write(&tmp_marker, version)?;

    // Atomically replace the target dir.
    if target_dir.exists() {
        fs::remove_dir_all(target_dir)?;
    }
    fs::rename(&tmp_dir, target_dir)?;

    // Sanity: marker_path should now exist at target_dir/.bundled.
    debug_assert_eq!(marker_path, &target_dir.join(".bundled"));

    Ok(())
}

/// Recursively write all files from an [`include_dir::Dir`] into `dest`.
fn write_dir_contents(dir: &include_dir::Dir<'_>, dest: &Path) -> Result<(), std::io::Error> {
    for file in dir.files() {
        let rel = file.path().file_name().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "file has no name")
        })?;
        fs::write(dest.join(rel), file.contents())?;
    }
    for subdir in dir.dirs() {
        let rel = subdir.path().file_name().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "subdir has no name")
        })?;
        let sub_dest = dest.join(rel);
        fs::create_dir_all(&sub_dest)?;
        write_dir_contents(subdir, &sub_dest)?;
    }
    Ok(())
}

enum MarkerState {
    /// `.bundled` file does not exist.
    NoMarker,
    /// `.bundled` file exists and contains the provisioned version string.
    Version(String),
    /// `.bundled` file exists but could not be read.
    CorruptMarker,
}

fn read_marker_version(marker_path: &Path) -> MarkerState {
    if !marker_path.exists() {
        return MarkerState::NoMarker;
    }
    match fs::read_to_string(marker_path) {
        Ok(content) => {
            let v = content.trim().to_owned();
            if v.is_empty() {
                MarkerState::CorruptMarker
            } else {
                MarkerState::Version(v)
            }
        }
        Err(_) => MarkerState::CorruptMarker,
    }
}

/// Extract the `version` field from the embedded SKILL.md frontmatter.
/// Falls back to `"1.0"` if the field is absent or cannot be parsed.
fn extract_embedded_version(skill_dir: &include_dir::Dir<'_>) -> String {
    // In include_dir 0.7, get_file() takes a path relative to the embedded root.
    let skill_md_path = format!("{}/SKILL.md", skill_dir.path().display());
    let Some(skill_file) = BUNDLED_SKILLS_DIR.get_file(&skill_md_path) else {
        return "1.0".to_owned();
    };
    let Ok(content) = std::str::from_utf8(skill_file.contents()) else {
        return "1.0".to_owned();
    };
    parse_frontmatter_version(content).unwrap_or_else(|| "1.0".to_owned())
}

/// Check whether an on-disk skill dir (without a `.bundled` marker) matches the
/// embedded version — indicating it was provisioned before the marker system.
///
/// Returns `true` only when the on-disk `SKILL.md` content (trimmed) equals the
/// embedded `SKILL.md` content (trimmed). A mismatch means the user modified the
/// file, so we treat the skill as user-owned.
fn is_legacy_bundled(target_dir: &Path, skill_name: &str) -> bool {
    let embedded_path = format!("{skill_name}/SKILL.md");
    let Some(embedded_file) = BUNDLED_SKILLS_DIR.get_file(&embedded_path) else {
        return false;
    };
    let Ok(embedded_content) = std::str::from_utf8(embedded_file.contents()) else {
        return false;
    };
    match fs::read_to_string(target_dir.join("SKILL.md")) {
        Ok(on_disk) => on_disk.trim() == embedded_content.trim(),
        Err(_) => false,
    }
}

/// Parse the `version:` key from the `metadata:` block in SKILL.md frontmatter.
///
/// Frontmatter is delimited by `---` lines. Within `metadata:`, lines of the
/// form `  version: <value>` are matched.
fn parse_frontmatter_version(content: &str) -> Option<String> {
    let mut in_frontmatter = false;
    let mut in_metadata = false;

    for line in content.lines() {
        if !in_frontmatter {
            if line.trim() == "---" {
                in_frontmatter = true;
            }
            continue;
        }
        if line.trim() == "---" {
            break; // end of frontmatter
        }
        if line.trim_start().starts_with("metadata:") {
            in_metadata = true;
            continue;
        }
        if in_metadata {
            // A non-indented line ends the metadata block.
            if !line.starts_with(' ') && !line.starts_with('\t') {
                in_metadata = false;
                continue;
            }
            let trimmed = line.trim();
            if let Some(rest) = trimmed.strip_prefix("version:") {
                let v = rest.trim().trim_matches('"').trim_matches('\'').to_owned();
                if !v.is_empty() {
                    return Some(v);
                }
            }
        }
    }
    None
}

// --- tests -------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_skill_md(version: &str) -> String {
        format!(
            "---\nname: test-skill\ndescription: A test skill\nmetadata:\n  version: {version}\n---\n\nSkill body.\n"
        )
    }

    #[test]
    fn parse_version_from_frontmatter() {
        let content = make_skill_md("2.3");
        assert_eq!(parse_frontmatter_version(&content), Some("2.3".to_owned()));
    }

    #[test]
    fn parse_version_missing_returns_none() {
        let content = "---\nname: test-skill\ndescription: desc\n---\n\nbody\n";
        assert_eq!(parse_frontmatter_version(content), None);
    }

    #[test]
    fn marker_no_file_returns_no_marker() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join(".bundled");
        assert!(matches!(read_marker_version(&path), MarkerState::NoMarker));
    }

    #[test]
    fn marker_empty_file_returns_corrupt() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join(".bundled");
        fs::write(&path, "").unwrap();
        assert!(matches!(
            read_marker_version(&path),
            MarkerState::CorruptMarker
        ));
    }

    #[test]
    fn marker_with_version_returns_version() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join(".bundled");
        fs::write(&path, "1.5\n").unwrap();
        assert!(matches!(
            read_marker_version(&path),
            MarkerState::Version(v) if v == "1.5"
        ));
    }

    /// `is_legacy_bundled` returns true when on-disk SKILL.md matches embedded content (trimmed).
    #[test]
    fn is_legacy_bundled_matches_identical_content() {
        let tmp = TempDir::new().unwrap();
        let skill_dir = tmp.path().join("test-skill");
        fs::create_dir_all(&skill_dir).unwrap();

        // Write the same content that would be in an embedded skill.
        let skill_md_content = make_skill_md("1.0");
        fs::write(skill_dir.join("SKILL.md"), &skill_md_content).unwrap();

        // We can't directly call is_legacy_bundled with a real include_dir::Dir,
        // so we test the provision path: provision to empty dir first (installs),
        // then remove the .bundled marker and re-provision — the skill must be
        // migrated (moved to updated), not skipped.
        let managed = TempDir::new().unwrap();
        let report1 = provision_bundled_skills(managed.path()).expect("first provision");
        assert!(report1.failed.is_empty());

        // Remove all .bundled markers to simulate pre-marker state.
        for name in &report1.installed {
            let marker = managed.path().join(name).join(".bundled");
            if marker.exists() {
                fs::remove_file(&marker).unwrap();
            }
        }

        // Re-provision: skills whose SKILL.md matches embedded → migrate (updated).
        // Skills whose SKILL.md was modified → skip.
        let report2 = provision_bundled_skills(managed.path()).expect("second provision");
        assert!(
            report2.failed.is_empty(),
            "no failures on re-provision: {:?}",
            report2.failed
        );
        // All skills without markers should be migrated (updated), none skipped.
        assert!(
            report2.installed.is_empty(),
            "no new installs expected on re-provision"
        );
        assert!(
            report2.skipped.is_empty(),
            "no skills should be skipped when content matches embedded"
        );
        assert!(
            !report2.updated.is_empty(),
            "all skills without marker must be migrated to updated"
        );

        // After migration, each skill must have a .bundled marker.
        for name in &report2.updated {
            let marker = managed.path().join(name).join(".bundled");
            assert!(
                marker.exists(),
                "{name}: .bundled marker missing after migration"
            );
        }
    }

    /// `is_legacy_bundled` returns false when on-disk SKILL.md differs from embedded.
    #[test]
    fn is_legacy_bundled_skips_modified_skill() {
        let managed = TempDir::new().unwrap();
        let report1 = provision_bundled_skills(managed.path()).expect("first provision");
        assert!(report1.failed.is_empty());
        assert!(!report1.installed.is_empty());

        // Remove .bundled markers AND modify SKILL.md to simulate user edits.
        for name in &report1.installed {
            let skill_dir = managed.path().join(name);
            let marker = skill_dir.join(".bundled");
            if marker.exists() {
                fs::remove_file(&marker).unwrap();
            }
            let skill_md = skill_dir.join("SKILL.md");
            if skill_md.exists() {
                let mut content = fs::read_to_string(&skill_md).unwrap();
                content.push_str("\n# user modification\n");
                fs::write(&skill_md, content).unwrap();
            }
        }

        let report2 = provision_bundled_skills(managed.path()).expect("second provision");
        assert!(
            report2.failed.is_empty(),
            "no failures: {:?}",
            report2.failed
        );
        // All modified skills must be skipped (treated as user-owned).
        assert!(
            report2.updated.is_empty(),
            "modified skills must not be updated"
        );
        assert!(
            report2.installed.is_empty(),
            "no re-installs expected when dir exists"
        );
        assert!(
            !report2.skipped.is_empty(),
            "modified skills must be skipped"
        );
    }

    /// Provision to an empty managed dir: all bundled skills are installed and
    /// each gets a `.bundled` marker file containing the skill version.
    #[test]
    fn provision_to_empty_dir_installs_all_skills() {
        let tmp = TempDir::new().unwrap();
        let managed = tmp.path();

        let report = provision_bundled_skills(managed).expect("provision should succeed");

        // Every bundled skill must be installed (none were pre-existing).
        assert!(
            report.failed.is_empty(),
            "unexpected failures: {:?}",
            report.failed
        );
        assert!(report.skipped.is_empty(), "no skills should be skipped");
        assert!(report.updated.is_empty(), "no skills should be updated");
        assert!(
            !report.installed.is_empty(),
            "at least one skill must be installed"
        );

        // Each installed skill must have a SKILL.md and a .bundled marker.
        for name in &report.installed {
            let skill_dir = managed.join(name);
            assert!(
                skill_dir.join("SKILL.md").exists(),
                "{name}: SKILL.md missing"
            );
            let marker = skill_dir.join(".bundled");
            assert!(marker.exists(), "{name}: .bundled marker missing");
            let version = fs::read_to_string(&marker).unwrap();
            assert!(
                !version.trim().is_empty(),
                "{name}: .bundled marker is empty"
            );
        }
    }
}
