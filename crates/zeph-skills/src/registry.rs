use std::collections::HashSet;
use std::path::Path;

use crate::loader::{Skill, load_skill};

#[derive(Debug)]
pub struct SkillRegistry {
    skills: Vec<Skill>,
}

impl SkillRegistry {
    /// Scan directories for `*/SKILL.md` and load all valid skills.
    ///
    /// Earlier paths have higher priority: if a skill with the same name appears
    /// in multiple paths, only the first one is kept.
    ///
    /// Invalid files are logged with `tracing::warn` and skipped.
    pub fn load(paths: &[impl AsRef<Path>]) -> Self {
        let mut skills = Vec::new();
        let mut seen = HashSet::new();

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
                    Ok(skill) => {
                        if seen.insert(skill.name.clone()) {
                            skills.push(skill);
                        } else {
                            tracing::debug!("duplicate skill '{}', skipping", skill.name);
                        }
                    }
                    Err(e) => tracing::warn!("skipping {}: {e:#}", skill_path.display()),
                }
            }
        }

        Self { skills }
    }

    /// Reload skills from the given paths, replacing the current set.
    pub fn reload(&mut self, paths: &[impl AsRef<Path>]) {
        self.skills = Self::load(paths).skills;
    }

    #[must_use]
    pub fn all(&self) -> &[Skill] {
        &self.skills
    }

    /// Consume the registry and return the owned skills vector.
    #[must_use]
    pub fn into_skills(self) -> Vec<Skill> {
        self.skills
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

    #[test]
    fn priority_first_path_wins() {
        let dir1 = tempfile::tempdir().unwrap();
        let dir2 = tempfile::tempdir().unwrap();

        let s1 = dir1.path().join("dupe");
        std::fs::create_dir(&s1).unwrap();
        std::fs::write(
            s1.join("SKILL.md"),
            "---\nname: dupe\ndescription: first\n---\nfirst body",
        )
        .unwrap();

        let s2 = dir2.path().join("dupe");
        std::fs::create_dir(&s2).unwrap();
        std::fs::write(
            s2.join("SKILL.md"),
            "---\nname: dupe\ndescription: second\n---\nsecond body",
        )
        .unwrap();

        let registry = SkillRegistry::load(&[dir1.path().to_path_buf(), dir2.path().to_path_buf()]);
        assert_eq!(registry.all().len(), 1);
        assert_eq!(registry.all()[0].description, "first");
    }

    #[test]
    fn reload_detects_changes() {
        let dir = tempfile::tempdir().unwrap();

        let s1 = dir.path().join("skill-a");
        std::fs::create_dir(&s1).unwrap();
        std::fs::write(
            s1.join("SKILL.md"),
            "---\nname: a\ndescription: old\n---\nbody",
        )
        .unwrap();

        let mut registry = SkillRegistry::load(&[dir.path().to_path_buf()]);
        assert_eq!(registry.all().len(), 1);

        let s2 = dir.path().join("skill-b");
        std::fs::create_dir(&s2).unwrap();
        std::fs::write(
            s2.join("SKILL.md"),
            "---\nname: b\ndescription: new\n---\nbody",
        )
        .unwrap();

        registry.reload(&[dir.path().to_path_buf()]);
        assert_eq!(registry.all().len(), 2);
    }

    #[test]
    fn into_skills_consumes_registry() {
        let dir = tempfile::tempdir().unwrap();
        let s = dir.path().join("skill");
        std::fs::create_dir(&s).unwrap();
        std::fs::write(s.join("SKILL.md"), "---\nname: x\ndescription: y\n---\nz").unwrap();

        let registry = SkillRegistry::load(&[dir.path().to_path_buf()]);
        let skills = registry.into_skills();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "x");
    }
}
