use std::path::Path;

use anyhow::{Context, bail};
use serde::Deserialize;

#[derive(Debug)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub body: String,
}

#[derive(Deserialize)]
struct Frontmatter {
    name: String,
    description: String,
}

/// Load a skill from a SKILL.md file with YAML frontmatter.
///
/// # Errors
///
/// Returns an error if the file cannot be read or the frontmatter is missing/invalid.
pub fn load_skill(path: &Path) -> anyhow::Result<Skill> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;

    let content = content.trim_start();
    if !content.starts_with("---") {
        bail!("missing frontmatter delimiter in {}", path.display());
    }

    let after_open = &content[3..];
    let Some(close) = after_open.find("---") else {
        bail!("unclosed frontmatter in {}", path.display());
    };

    let yaml_str = &after_open[..close];
    let fm: Frontmatter = serde_yml::from_str(yaml_str)
        .with_context(|| format!("invalid frontmatter in {}", path.display()))?;

    let body = after_open[close + 3..].trim().to_string();

    Ok(Skill {
        name: fm.name,
        description: fm.description,
        body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_skill(dir: &Path, content: &str) -> std::path::PathBuf {
        let path = dir.join("SKILL.md");
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn parse_valid_skill() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_skill(
            dir.path(),
            "---\nname: test\ndescription: A test skill.\n---\n# Body\nHello",
        );

        let skill = load_skill(&path).unwrap();
        assert_eq!(skill.name, "test");
        assert_eq!(skill.description, "A test skill.");
        assert_eq!(skill.body, "# Body\nHello");
    }

    #[test]
    fn missing_frontmatter_delimiter() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_skill(dir.path(), "no frontmatter here");

        let err = load_skill(&path).unwrap_err();
        assert!(err.to_string().contains("missing frontmatter"));
    }

    #[test]
    fn unclosed_frontmatter() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_skill(dir.path(), "---\nname: x\n");

        let err = load_skill(&path).unwrap_err();
        assert!(err.to_string().contains("unclosed frontmatter"));
    }

    #[test]
    fn invalid_yaml() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_skill(dir.path(), "---\n: broken\n---\nbody");

        assert!(load_skill(&path).is_err());
    }

    #[test]
    fn missing_required_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_skill(dir.path(), "---\nname: test\n---\nbody");

        assert!(load_skill(&path).is_err());
    }
}
