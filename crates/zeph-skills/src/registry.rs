use std::collections::HashSet;
use std::path::Path;
use std::sync::OnceLock;

use crate::loader::{Skill, SkillMeta, load_skill_body, load_skill_meta};

struct SkillEntry {
    meta: SkillMeta,
    body: OnceLock<String>,
}

#[derive(Default)]
pub struct SkillRegistry {
    entries: Vec<SkillEntry>,
}

impl std::fmt::Debug for SkillRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SkillRegistry")
            .field("count", &self.entries.len())
            .finish()
    }
}

impl SkillRegistry {
    /// Scan directories for `*/SKILL.md` and load metadata only (lazy body).
    ///
    /// Earlier paths have higher priority: if a skill with the same name appears
    /// in multiple paths, only the first one is kept.
    ///
    /// Invalid files are logged with `tracing::warn` and skipped.
    pub fn load(paths: &[impl AsRef<Path>]) -> Self {
        let mut entries = Vec::new();
        let mut seen = HashSet::new();

        for base in paths {
            let base = base.as_ref();
            let Ok(dir_entries) = std::fs::read_dir(base) else {
                tracing::warn!("cannot read skill directory: {}", base.display());
                continue;
            };

            for entry in dir_entries.flatten() {
                let skill_path = entry.path().join("SKILL.md");
                if !skill_path.is_file() {
                    continue;
                }
                match load_skill_meta(&skill_path) {
                    Ok(meta) => {
                        if seen.insert(meta.name.clone()) {
                            entries.push(SkillEntry {
                                meta,
                                body: OnceLock::new(),
                            });
                        } else {
                            tracing::debug!("duplicate skill '{}', skipping", skill_path.display());
                        }
                    }
                    Err(e) => tracing::warn!("skipping {}: {e:#}", skill_path.display()),
                }
            }
        }

        Self { entries }
    }

    /// Reload skills from the given paths, replacing the current set.
    pub fn reload(&mut self, paths: &[impl AsRef<Path>]) {
        *self = Self::load(paths);
    }

    #[must_use]
    pub fn all_meta(&self) -> Vec<&SkillMeta> {
        self.entries.iter().map(|e| &e.meta).collect()
    }

    /// Get the body for a skill by name, loading from disk on first access.
    ///
    /// # Errors
    ///
    /// Returns an error if the body cannot be loaded from disk.
    pub fn get_body(&self, name: &str) -> anyhow::Result<&str> {
        let entry = self
            .entries
            .iter()
            .find(|e| e.meta.name == name)
            .ok_or_else(|| anyhow::anyhow!("skill not found: {name}"))?;

        if let Some(body) = entry.body.get() {
            return Ok(body.as_str());
        }
        let body = load_skill_body(&entry.meta)?;
        let _ = entry.body.set(body);
        Ok(entry.body.get().map_or("", String::as_str))
    }

    /// Get a full Skill (meta + body) by name.
    ///
    /// # Errors
    ///
    /// Returns an error if the skill is not found or body cannot be loaded.
    pub fn get_skill(&self, name: &str) -> anyhow::Result<Skill> {
        let body = self.get_body(name)?.to_owned();
        let entry = self
            .entries
            .iter()
            .find(|e| e.meta.name == name)
            .ok_or_else(|| anyhow::anyhow!("skill not found: {name}"))?;

        Ok(Skill {
            meta: entry.meta.clone(),
            body,
        })
    }

    /// Consume the registry and return all skills with bodies loaded.
    #[must_use]
    pub fn into_skills(self) -> Vec<Skill> {
        self.entries
            .into_iter()
            .filter_map(|entry| {
                let body = match entry.body.into_inner() {
                    Some(b) => b,
                    None => match load_skill_body(&entry.meta) {
                        Ok(b) => b,
                        Err(e) => {
                            tracing::warn!("failed to load body for '{}': {e:#}", entry.meta.name);
                            return None;
                        }
                    },
                };
                Some(Skill {
                    meta: entry.meta,
                    body,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_skill(dir: &Path, name: &str, description: &str, body: &str) {
        let skill_dir = dir.join(name);
        std::fs::create_dir(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {description}\n---\n{body}"),
        )
        .unwrap();
    }

    #[test]
    fn load_from_temp_dir() {
        let dir = tempfile::tempdir().unwrap();
        create_skill(dir.path(), "my-skill", "test", "body");

        let registry = SkillRegistry::load(&[dir.path().to_path_buf()]);
        assert_eq!(registry.all_meta().len(), 1);
        assert_eq!(registry.all_meta()[0].name, "my-skill");
    }

    #[test]
    fn skips_invalid_skills() {
        let dir = tempfile::tempdir().unwrap();
        create_skill(dir.path(), "good", "ok", "body");

        let bad = dir.path().join("bad");
        std::fs::create_dir(&bad).unwrap();
        std::fs::write(bad.join("SKILL.md"), "no frontmatter").unwrap();

        let registry = SkillRegistry::load(&[dir.path().to_path_buf()]);
        assert_eq!(registry.all_meta().len(), 1);
        assert_eq!(registry.all_meta()[0].name, "good");
    }

    #[test]
    fn empty_directory() {
        let dir = tempfile::tempdir().unwrap();
        let registry = SkillRegistry::load(&[dir.path().to_path_buf()]);
        assert!(registry.all_meta().is_empty());
    }

    #[test]
    fn missing_directory() {
        let registry = SkillRegistry::load(&[std::path::PathBuf::from("/nonexistent/path")]);
        assert!(registry.all_meta().is_empty());
    }

    #[test]
    fn priority_first_path_wins() {
        let dir1 = tempfile::tempdir().unwrap();
        let dir2 = tempfile::tempdir().unwrap();
        create_skill(dir1.path(), "dupe", "first", "first body");
        create_skill(dir2.path(), "dupe", "second", "second body");

        let registry = SkillRegistry::load(&[dir1.path().to_path_buf(), dir2.path().to_path_buf()]);
        assert_eq!(registry.all_meta().len(), 1);
        assert_eq!(registry.all_meta()[0].description, "first");
    }

    #[test]
    fn reload_detects_changes() {
        let dir = tempfile::tempdir().unwrap();
        create_skill(dir.path(), "skill-a", "old", "body");

        let mut registry = SkillRegistry::load(&[dir.path().to_path_buf()]);
        assert_eq!(registry.all_meta().len(), 1);

        create_skill(dir.path(), "skill-b", "new", "body");

        registry.reload(&[dir.path().to_path_buf()]);
        assert_eq!(registry.all_meta().len(), 2);
    }

    #[test]
    fn into_skills_consumes_registry() {
        let dir = tempfile::tempdir().unwrap();
        create_skill(dir.path(), "x", "y", "z");

        let registry = SkillRegistry::load(&[dir.path().to_path_buf()]);
        let skills = registry.into_skills();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name(), "x");
        assert_eq!(skills[0].body, "z");
    }

    #[test]
    fn lazy_body_loading() {
        let dir = tempfile::tempdir().unwrap();
        create_skill(dir.path(), "lazy", "desc", "lazy body content");

        let registry = SkillRegistry::load(&[dir.path().to_path_buf()]);
        let body = registry.get_body("lazy").unwrap();
        assert_eq!(body, "lazy body content");
    }

    #[test]
    fn get_skill_returns_full_skill() {
        let dir = tempfile::tempdir().unwrap();
        create_skill(dir.path(), "full", "description", "full body");

        let registry = SkillRegistry::load(&[dir.path().to_path_buf()]);
        let skill = registry.get_skill("full").unwrap();
        assert_eq!(skill.name(), "full");
        assert_eq!(skill.description(), "description");
        assert_eq!(skill.body, "full body");
    }

    #[test]
    fn get_body_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let registry = SkillRegistry::load(&[dir.path().to_path_buf()]);
        assert!(registry.get_body("nonexistent").is_err());
    }
}
