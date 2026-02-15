use std::path::{Path, PathBuf};

use crate::error::SkillError;

#[derive(Clone, Debug)]
pub struct SkillMeta {
    pub name: String,
    pub description: String,
    pub compatibility: Option<String>,
    pub license: Option<String>,
    pub metadata: Vec<(String, String)>,
    pub allowed_tools: Vec<String>,
    pub skill_dir: PathBuf,
}

#[derive(Clone, Debug)]
pub struct Skill {
    pub meta: SkillMeta,
    pub body: String,
}

impl Skill {
    #[must_use]
    pub fn name(&self) -> &str {
        &self.meta.name
    }

    #[must_use]
    pub fn description(&self) -> &str {
        &self.meta.description
    }
}

fn validate_skill_name(name: &str, dir_name: &str) -> Result<(), SkillError> {
    if name.is_empty() || name.len() > 64 {
        return Err(SkillError::Invalid(format!(
            "skill name must be 1-64 characters, got {}",
            name.len()
        )));
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err(SkillError::Invalid(format!(
            "skill name must contain only lowercase letters, digits, and hyphens: {name}"
        )));
    }
    if name.starts_with('-') || name.ends_with('-') {
        return Err(SkillError::Invalid(format!(
            "skill name must not start or end with hyphen: {name}"
        )));
    }
    if name.contains("--") {
        return Err(SkillError::Invalid(format!(
            "skill name must not contain consecutive hyphens: {name}"
        )));
    }
    if name != dir_name {
        return Err(SkillError::Invalid(format!(
            "skill name '{name}' does not match directory name '{dir_name}'"
        )));
    }
    Ok(())
}

struct RawFrontmatter {
    name: Option<String>,
    description: Option<String>,
    compatibility: Option<String>,
    license: Option<String>,
    metadata: Vec<(String, String)>,
    allowed_tools: Vec<String>,
}

fn parse_frontmatter(yaml_str: &str) -> RawFrontmatter {
    let mut name = None;
    let mut description = None;
    let mut compatibility = None;
    let mut license = None;
    let mut metadata = Vec::new();
    let mut allowed_tools = Vec::new();

    for line in yaml_str.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some((key, value)) = line.split_once(':') {
            let key = key.trim();
            let value = value.trim().to_string();
            match key {
                "name" => name = Some(value),
                "description" => description = Some(value),
                "compatibility" => compatibility = Some(value),
                "license" => license = Some(value),
                "allowed-tools" => {
                    allowed_tools = value
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect();
                }
                _ => {
                    if !value.is_empty() {
                        metadata.push((key.to_string(), value));
                    }
                }
            }
        }
    }

    RawFrontmatter {
        name,
        description,
        compatibility,
        license,
        metadata,
        allowed_tools,
    }
}

fn split_frontmatter(content: &str) -> Result<(&str, &str), SkillError> {
    let content = content.trim_start();
    if !content.starts_with("---") {
        return Err(SkillError::Invalid("missing frontmatter delimiter".into()));
    }
    let after_open = &content[3..];
    let Some(close) = after_open.find("---") else {
        return Err(SkillError::Invalid("unclosed frontmatter".into()));
    };
    let yaml_str = &after_open[..close];
    let body = after_open[close + 3..].trim();
    Ok((yaml_str, body))
}

/// Verify that `path` resolves to a location inside `base_dir` after canonicalization.
///
/// Prevents symlink-based path traversal by ensuring the canonical path
/// starts with the canonical base directory prefix.
///
/// # Errors
///
/// Returns `SkillError::Invalid` if the path escapes `base_dir`.
pub fn validate_path_within(path: &Path, base_dir: &Path) -> Result<PathBuf, SkillError> {
    let canonical_base = base_dir.canonicalize().map_err(|e| {
        SkillError::Other(format!(
            "failed to canonicalize base dir {}: {e}",
            base_dir.display()
        ))
    })?;
    let canonical_path = path.canonicalize().map_err(|e| {
        SkillError::Other(format!(
            "failed to canonicalize path {}: {e}",
            path.display()
        ))
    })?;
    if !canonical_path.starts_with(&canonical_base) {
        return Err(SkillError::Invalid(format!(
            "path {} escapes skills directory {}",
            canonical_path.display(),
            canonical_base.display()
        )));
    }
    Ok(canonical_path)
}

/// Load only frontmatter metadata from a SKILL.md file.
///
/// # Errors
///
/// Returns an error if the file cannot be read or the frontmatter is missing/invalid.
pub fn load_skill_meta(path: &Path) -> Result<SkillMeta, SkillError> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| SkillError::Other(format!("failed to read {}: {e}", path.display())))?;

    let (yaml_str, _body) = split_frontmatter(&content)
        .map_err(|e| SkillError::Other(format!("in {}: {e}", path.display())))?;

    let raw = parse_frontmatter(yaml_str);

    let name = raw.name.filter(|s| !s.is_empty()).ok_or_else(|| {
        SkillError::Invalid(format!(
            "missing 'name' in frontmatter of {}",
            path.display()
        ))
    })?;
    let description = raw.description.filter(|s| !s.is_empty()).ok_or_else(|| {
        SkillError::Invalid(format!(
            "missing 'description' in frontmatter of {}",
            path.display()
        ))
    })?;

    let skill_dir = path.parent().map(Path::to_path_buf).unwrap_or_default();

    let dir_name = skill_dir.file_name().and_then(|n| n.to_str()).unwrap_or("");

    validate_skill_name(&name, dir_name)
        .map_err(|e| SkillError::Other(format!("in {}: {e}", path.display())))?;

    Ok(SkillMeta {
        name,
        description,
        compatibility: raw.compatibility,
        license: raw.license,
        metadata: raw.metadata,
        allowed_tools: raw.allowed_tools,
        skill_dir,
    })
}

/// Load the body content for a skill given its metadata.
///
/// # Errors
///
/// Returns an error if the file cannot be read or parsed.
pub fn load_skill_body(meta: &SkillMeta) -> Result<String, SkillError> {
    let path = meta.skill_dir.join("SKILL.md");
    let content = std::fs::read_to_string(&path)
        .map_err(|e| SkillError::Other(format!("failed to read {}: {e}", path.display())))?;

    let (_yaml_str, body) = split_frontmatter(&content)
        .map_err(|e| SkillError::Other(format!("in {}: {e}", path.display())))?;

    Ok(body.to_string())
}

/// Load a skill from a SKILL.md file with YAML frontmatter.
///
/// # Errors
///
/// Returns an error if the file cannot be read or the frontmatter is missing/invalid.
pub fn load_skill(path: &Path) -> Result<Skill, SkillError> {
    let meta = load_skill_meta(path)?;
    let body = load_skill_body(&meta)?;
    Ok(Skill { meta, body })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_skill(dir: &Path, name: &str, content: &str) -> std::path::PathBuf {
        let skill_dir = dir.join(name);
        std::fs::create_dir_all(&skill_dir).unwrap();
        let path = skill_dir.join("SKILL.md");
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn parse_valid_skill() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_skill(
            dir.path(),
            "test",
            "---\nname: test\ndescription: A test skill.\n---\n# Body\nHello",
        );

        let skill = load_skill(&path).unwrap();
        assert_eq!(skill.name(), "test");
        assert_eq!(skill.description(), "A test skill.");
        assert_eq!(skill.body, "# Body\nHello");
    }

    #[test]
    fn missing_frontmatter_delimiter() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("bad");
        std::fs::create_dir_all(&skill_dir).unwrap();
        let path = skill_dir.join("SKILL.md");
        std::fs::write(&path, "no frontmatter here").unwrap();

        let err = load_skill(&path).unwrap_err();
        assert!(format!("{err:#}").contains("missing frontmatter"));
    }

    #[test]
    fn unclosed_frontmatter() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("test");
        std::fs::create_dir_all(&skill_dir).unwrap();
        let path = skill_dir.join("SKILL.md");
        std::fs::write(&path, "---\nname: test\n").unwrap();

        let err = load_skill(&path).unwrap_err();
        assert!(format!("{err:#}").contains("unclosed frontmatter"));
    }

    #[test]
    fn invalid_yaml() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("broken");
        std::fs::create_dir_all(&skill_dir).unwrap();
        let path = skill_dir.join("SKILL.md");
        std::fs::write(&path, "---\n: broken\n---\nbody").unwrap();

        assert!(load_skill(&path).is_err());
    }

    #[test]
    fn missing_required_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_skill(dir.path(), "test", "---\nname: test\n---\nbody");

        assert!(load_skill(&path).is_err());
    }

    #[test]
    fn load_skill_meta_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_skill(
            dir.path(),
            "my-skill",
            "---\nname: my-skill\ndescription: desc\n---\nbig body here",
        );

        let meta = load_skill_meta(&path).unwrap();
        assert_eq!(meta.name, "my-skill");
        assert_eq!(meta.description, "desc");
        assert_eq!(meta.skill_dir, path.parent().unwrap());
    }

    #[test]
    fn load_body_from_meta() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_skill(
            dir.path(),
            "my-skill",
            "---\nname: my-skill\ndescription: desc\n---\nthe body content",
        );

        let meta = load_skill_meta(&path).unwrap();
        let body = load_skill_body(&meta).unwrap();
        assert_eq!(body, "the body content");
    }

    #[test]
    fn extended_frontmatter() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_skill(
            dir.path(),
            "my-skill",
            "---\nname: my-skill\ndescription: desc\ncompatibility: linux\nlicense: MIT\nallowed-tools: bash, python\ncustom-key: custom-value\n---\nbody",
        );

        let meta = load_skill_meta(&path).unwrap();
        assert_eq!(meta.compatibility.as_deref(), Some("linux"));
        assert_eq!(meta.license.as_deref(), Some("MIT"));
        assert_eq!(meta.allowed_tools, vec!["bash", "python"]);
        assert_eq!(
            meta.metadata,
            vec![("custom-key".into(), "custom-value".into())]
        );
    }

    #[test]
    fn name_validation_rejects_uppercase() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("Bad");
        std::fs::create_dir_all(&skill_dir).unwrap();
        let path = skill_dir.join("SKILL.md");
        std::fs::write(&path, "---\nname: Bad\ndescription: d\n---\nb").unwrap();

        assert!(load_skill_meta(&path).is_err());
    }

    #[test]
    fn name_validation_rejects_leading_hyphen() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("-bad");
        std::fs::create_dir_all(&skill_dir).unwrap();
        let path = skill_dir.join("SKILL.md");
        std::fs::write(&path, "---\nname: -bad\ndescription: d\n---\nb").unwrap();

        assert!(load_skill_meta(&path).is_err());
    }

    #[test]
    fn name_validation_rejects_consecutive_hyphens() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("a--b");
        std::fs::create_dir_all(&skill_dir).unwrap();
        let path = skill_dir.join("SKILL.md");
        std::fs::write(&path, "---\nname: a--b\ndescription: d\n---\nb").unwrap();

        assert!(load_skill_meta(&path).is_err());
    }

    #[test]
    fn name_validation_rejects_dir_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("actual-dir");
        std::fs::create_dir_all(&skill_dir).unwrap();
        let path = skill_dir.join("SKILL.md");
        std::fs::write(&path, "---\nname: wrong-name\ndescription: d\n---\nb").unwrap();

        assert!(load_skill_meta(&path).is_err());
    }

    #[test]
    #[cfg(unix)]
    fn validate_path_within_rejects_symlink_escape() {
        let base = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();

        let outside_file = outside.path().join("secret.txt");
        std::fs::write(&outside_file, "secret").unwrap();

        let link_path = base.path().join("evil-link");
        std::os::unix::fs::symlink(&outside_file, &link_path).unwrap();
        let err = validate_path_within(&link_path, base.path()).unwrap_err();
        assert!(
            format!("{err:#}").contains("escapes skills directory"),
            "expected path traversal error, got: {err:#}"
        );
    }

    #[test]
    fn validate_path_within_accepts_legitimate_path() {
        let base = tempfile::tempdir().unwrap();
        let inner = base.path().join("skill-dir");
        std::fs::create_dir_all(&inner).unwrap();
        let file = inner.join("SKILL.md");
        std::fs::write(&file, "content").unwrap();

        let result = validate_path_within(&file, base.path());
        assert!(result.is_ok());
    }

    #[test]
    fn name_validation_too_long() {
        let name = "a".repeat(65);
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join(&name);
        std::fs::create_dir_all(&skill_dir).unwrap();
        let path = skill_dir.join("SKILL.md");
        std::fs::write(&path, format!("---\nname: {name}\ndescription: d\n---\nb")).unwrap();

        assert!(load_skill_meta(&path).is_err());
    }
}
