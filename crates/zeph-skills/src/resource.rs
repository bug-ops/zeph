use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Default)]
pub struct SkillResources {
    pub scripts: Vec<PathBuf>,
    pub references: Vec<PathBuf>,
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

/// Load a resource file content with path traversal protection.
///
/// # Errors
///
/// Returns an error if the path escapes the skill directory or the file cannot be read.
#[cfg(test)]
fn load_resource(
    skill_dir: &Path,
    relative_path: &str,
) -> Result<Vec<u8>, crate::error::SkillError> {
    use crate::error::SkillError;
    let canonical_base = skill_dir.canonicalize().map_err(|e| {
        SkillError::Other(format!(
            "failed to canonicalize skill dir {}: {e}",
            skill_dir.display()
        ))
    })?;

    let target = skill_dir.join(relative_path);
    let canonical_target = target.canonicalize().map_err(|e| {
        SkillError::Other(format!(
            "failed to canonicalize resource path {}: {e}",
            target.display()
        ))
    })?;

    if !canonical_target.starts_with(&canonical_base) {
        return Err(SkillError::Invalid(format!(
            "path traversal detected: {} escapes {}",
            relative_path,
            skill_dir.display()
        )));
    }

    std::fs::read(&canonical_target).map_err(|e| {
        SkillError::Other(format!(
            "failed to read resource {}: {e}",
            canonical_target.display()
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discover_empty_skill_dir() {
        let dir = tempfile::tempdir().unwrap();
        let resources = discover_resources(dir.path());
        assert!(resources.scripts.is_empty());
        assert!(resources.references.is_empty());
        assert!(resources.assets.is_empty());
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
        std::fs::write(assets.join("logo.png"), &[0u8; 4]).unwrap();

        let resources = discover_resources(dir.path());
        assert_eq!(resources.scripts.len(), 1);
        assert_eq!(resources.references.len(), 1);
        assert_eq!(resources.assets.len(), 1);
    }

    #[test]
    fn load_resource_valid() {
        let dir = tempfile::tempdir().unwrap();
        let scripts = dir.path().join("scripts");
        std::fs::create_dir(&scripts).unwrap();
        std::fs::write(scripts.join("run.sh"), "echo hello").unwrap();

        let content = load_resource(dir.path(), "scripts/run.sh").unwrap();
        assert_eq!(content, b"echo hello");
    }

    #[test]
    fn load_resource_path_traversal() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("scripts")).unwrap();
        std::fs::write(dir.path().join("scripts/ok.sh"), "ok").unwrap();

        let err = load_resource(dir.path(), "../../../etc/passwd").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("path traversal") || msg.contains("canonicalize"));
    }

    #[test]
    fn load_resource_not_found() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_resource(dir.path(), "nonexistent.txt").is_err());
    }
}
