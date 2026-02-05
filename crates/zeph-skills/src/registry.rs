use std::path::Path;

use crate::loader::{Skill, load_skill};

#[derive(Debug)]
pub struct SkillRegistry {
    skills: Vec<Skill>,
}

impl SkillRegistry {
    /// Scan directories for `*/SKILL.md` and load all valid skills.
    ///
    /// Invalid files are logged with `tracing::warn` and skipped.
    pub fn load(paths: &[impl AsRef<Path>]) -> Self {
        let mut skills = Vec::new();

        for base in paths {
            let base = base.as_ref();
            let Ok(entries) = std::fs::read_dir(base) else {
                tracing::warn!("cannot read skill directory: {}", base.display());
                continue;
            };

            for entry in entries.flatten() {
                let skill_path = entry.path().join("SKILL.md");
                if !skill_path.is_file() {
                    continue;
                }
                match load_skill(&skill_path) {
                    Ok(skill) => skills.push(skill),
                    Err(e) => tracing::warn!("skipping {}: {e:#}", skill_path.display()),
                }
            }
        }

        Self { skills }
    }

    #[must_use]
    pub fn all(&self) -> &[Skill] {
        &self.skills
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_from_temp_dir() {
        let dir = tempfile::tempdir().unwrap();

        let skill_dir = dir.path().join("my-skill");
        std::fs::create_dir(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: my-skill\ndescription: test\n---\nbody",
        )
        .unwrap();

        let registry = SkillRegistry::load(&[dir.path().to_path_buf()]);
        assert_eq!(registry.all().len(), 1);
        assert_eq!(registry.all()[0].name, "my-skill");
    }

    #[test]
    fn skips_invalid_skills() {
        let dir = tempfile::tempdir().unwrap();

        let good = dir.path().join("good");
        std::fs::create_dir(&good).unwrap();
        std::fs::write(
            good.join("SKILL.md"),
            "---\nname: good\ndescription: ok\n---\nbody",
        )
        .unwrap();

        let bad = dir.path().join("bad");
        std::fs::create_dir(&bad).unwrap();
        std::fs::write(bad.join("SKILL.md"), "no frontmatter").unwrap();

        let registry = SkillRegistry::load(&[dir.path().to_path_buf()]);
        assert_eq!(registry.all().len(), 1);
        assert_eq!(registry.all()[0].name, "good");
    }

    #[test]
    fn empty_directory() {
        let dir = tempfile::tempdir().unwrap();
        let registry = SkillRegistry::load(&[dir.path().to_path_buf()]);
        assert!(registry.all().is_empty());
    }

    #[test]
    fn missing_directory() {
        let registry = SkillRegistry::load(&[std::path::PathBuf::from("/nonexistent/path")]);
        assert!(registry.all().is_empty());
    }
}
