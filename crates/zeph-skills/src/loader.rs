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
    pub requires_secrets: Vec<String>,
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
    requires_secrets: Vec<String>,
    /// Whether `requires-secrets` (deprecated) was used instead of `x-requires-secrets`.
    deprecated_requires_secrets: bool,
}

fn parse_frontmatter(yaml_str: &str) -> RawFrontmatter {
    let mut name = None;
    let mut description = None;
    let mut compatibility = None;
    let mut license = None;
    let mut metadata = Vec::new();
    let mut allowed_tools = Vec::new();
    let mut requires_secrets = Vec::new();
    let mut deprecated_requires_secrets = false;
    let mut in_metadata = false;

    for line in yaml_str.lines() {
        if in_metadata {
            if line.starts_with("  ") || line.starts_with('\t') {
                let trimmed = line.trim();
                if let Some((k, v)) = trimmed.split_once(':') {
                    let v = v.trim();
                    if !v.is_empty() {
                        metadata.push((k.trim().to_string(), v.to_string()));
                    }
                }
                continue;
            }
            in_metadata = false;
        }

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
                "compatibility" => {
                    if !value.is_empty() {
                        compatibility = Some(value);
                    }
                }
                "license" => {
                    if !value.is_empty() {
                        license = Some(value);
                    }
                }
                "allowed-tools" => {
                    allowed_tools = value.split_whitespace().map(ToString::to_string).collect();
                }
                "x-requires-secrets" => {
                    requires_secrets = value
                        .split(',')
                        .map(|s| s.trim().to_lowercase().replace('-', "_"))
                        .filter(|s| !s.is_empty())
                        .collect();
                }
                "requires-secrets" => {
                    deprecated_requires_secrets = true;
                    // Only apply if x-requires-secrets was not already parsed.
                    // The canonical x-requires-secrets always wins over the deprecated form.
                    if requires_secrets.is_empty() {
                        requires_secrets = value
                            .split(',')
                            .map(|s| s.trim().to_lowercase().replace('-', "_"))
                            .filter(|s| !s.is_empty())
                            .collect();
                    }
                }
                "metadata" if value.is_empty() => {
                    in_metadata = true;
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
        requires_secrets,
        deprecated_requires_secrets,
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

    if description.len() > 1024 {
        return Err(SkillError::Invalid(format!(
            "description exceeds 1024 characters ({}) in {}",
            description.len(),
            path.display()
        )));
    }

    if let Some(ref c) = raw.compatibility
        && c.len() > 500
    {
        return Err(SkillError::Invalid(format!(
            "compatibility exceeds 500 characters ({}) in {}",
            c.len(),
            path.display()
        )));
    }

    let skill_dir = path.parent().map(Path::to_path_buf).unwrap_or_default();

    let dir_name = skill_dir.file_name().and_then(|n| n.to_str()).unwrap_or("");

    validate_skill_name(&name, dir_name)
        .map_err(|e| SkillError::Other(format!("in {}: {e}", path.display())))?;

    if raw.deprecated_requires_secrets {
        tracing::warn!(
            "'requires-secrets' is deprecated, use 'x-requires-secrets' in {}",
            path.display()
        );
    }

    Ok(SkillMeta {
        name,
        description,
        compatibility: raw.compatibility,
        license: raw.license,
        metadata: raw.metadata,
        allowed_tools: raw.allowed_tools,
        requires_secrets: raw.requires_secrets,
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

    if body.len() > 20_000 {
        tracing::warn!(
            skill = %meta.name,
            bytes = body.len(),
            "skill body exceeds 20000 bytes; consider trimming to stay within ~5000 token budget"
        );
    }

    Ok(body.to_string())
}

/// Parse Markdown link targets from skill body and warn about broken or out-of-bounds references.
///
/// Checks links whose targets start with `references/`, `scripts/`, or `assets/`.
/// Missing files or paths escaping `skill_dir` are returned as warning strings.
/// This does not block skill loading.
#[must_use]
pub fn validate_skill_references(body: &str, skill_dir: &Path) -> Vec<String> {
    let mut warnings = Vec::new();
    // Match ](references/...), ](scripts/...), ](assets/...)
    let mut rest = body;
    while let Some(open) = rest.find("](") {
        rest = &rest[open + 2..];
        let Some(close) = rest.find(')') else {
            break;
        };
        let target = &rest[..close];
        rest = &rest[close + 1..];

        if !target.starts_with("references/")
            && !target.starts_with("scripts/")
            && !target.starts_with("assets/")
        {
            continue;
        }

        let full = skill_dir.join(target);
        if !full.exists() {
            warnings.push(format!("broken reference: {target} does not exist"));
            continue;
        }
        if let Err(e) = validate_path_within(&full, skill_dir) {
            warnings.push(format!("unsafe reference {target}: {e}"));
        }
    }
    warnings
}

/// Load a skill from a SKILL.md file with YAML frontmatter.
///
/// # Errors
///
/// Returns an error if the file cannot be read or the frontmatter is missing/invalid.
pub fn load_skill(path: &Path) -> Result<Skill, SkillError> {
    let meta = load_skill_meta(path)?;
    let body = load_skill_body(&meta)?;

    for warning in validate_skill_references(&body, &meta.skill_dir) {
        tracing::warn!(skill = %meta.name, "{warning}");
    }

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
            "---\nname: my-skill\ndescription: desc\ncompatibility: linux\nlicense: MIT\nallowed-tools: bash python\ncustom-key: custom-value\n---\nbody",
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
    fn allowed_tools_with_parens() {
        let raw = parse_frontmatter("allowed-tools: Bash(git:*) Bash(jq:*) Read\n");
        assert_eq!(raw.allowed_tools, vec!["Bash(git:*)", "Bash(jq:*)", "Read"]);
    }

    #[test]
    fn allowed_tools_empty() {
        let raw = parse_frontmatter("allowed-tools:\n");
        assert!(raw.allowed_tools.is_empty());
    }

    #[test]
    fn metadata_nested_block() {
        let yaml = "metadata:\n  author: example-org\n  version: \"1.0\"\n";
        let raw = parse_frontmatter(yaml);
        assert_eq!(
            raw.metadata,
            vec![
                ("author".into(), "example-org".into()),
                ("version".into(), "\"1.0\"".into()),
            ]
        );
    }

    #[test]
    fn metadata_nested_with_other_fields() {
        let yaml = "name: my-skill\nmetadata:\n  author: example-org\nlicense: MIT\n";
        let raw = parse_frontmatter(yaml);
        assert_eq!(raw.name.as_deref(), Some("my-skill"));
        assert_eq!(raw.license.as_deref(), Some("MIT"));
        assert_eq!(raw.metadata, vec![("author".into(), "example-org".into())]);
    }

    #[test]
    fn metadata_flat_still_works() {
        let yaml = "custom-key: custom-value\n";
        let raw = parse_frontmatter(yaml);
        assert_eq!(
            raw.metadata,
            vec![("custom-key".into(), "custom-value".into())]
        );
    }

    #[test]
    fn description_exceeds_max_length() {
        let dir = tempfile::tempdir().unwrap();
        let desc = "a".repeat(1025);
        let path = write_skill(
            dir.path(),
            "my-skill",
            &format!("---\nname: my-skill\ndescription: {desc}\n---\nbody"),
        );
        let err = load_skill_meta(&path).unwrap_err();
        assert!(format!("{err:#}").contains("description exceeds 1024 characters"));
    }

    #[test]
    fn description_at_max_length() {
        let dir = tempfile::tempdir().unwrap();
        let desc = "a".repeat(1024);
        let path = write_skill(
            dir.path(),
            "my-skill",
            &format!("---\nname: my-skill\ndescription: {desc}\n---\nbody"),
        );
        assert!(load_skill_meta(&path).is_ok());
    }

    #[test]
    fn compatibility_exceeds_max_length() {
        let dir = tempfile::tempdir().unwrap();
        let compat = "a".repeat(501);
        let path = write_skill(
            dir.path(),
            "my-skill",
            &format!("---\nname: my-skill\ndescription: desc\ncompatibility: {compat}\n---\nbody"),
        );
        let err = load_skill_meta(&path).unwrap_err();
        assert!(format!("{err:#}").contains("compatibility exceeds 500 characters"));
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

    #[test]
    fn x_requires_secrets_parsed_from_frontmatter() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_skill(
            dir.path(),
            "github-api",
            "---\nname: github-api\ndescription: GitHub integration.\nx-requires-secrets: github-token, github-org\n---\nbody",
        );
        let meta = load_skill_meta(&path).unwrap();
        assert_eq!(meta.requires_secrets, vec!["github_token", "github_org"]);
    }

    #[test]
    fn requires_secrets_deprecated_backward_compat() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_skill(
            dir.path(),
            "github-api",
            "---\nname: github-api\ndescription: GitHub integration.\nrequires-secrets: github-token, github-org\n---\nbody",
        );
        // Old form still works (backward compat), but emits a deprecation warning.
        let meta = load_skill_meta(&path).unwrap();
        assert_eq!(meta.requires_secrets, vec!["github_token", "github_org"]);
    }

    #[test]
    fn x_requires_secrets_takes_precedence_over_deprecated() {
        // When both are present, x-requires-secrets wins regardless of order.
        let raw = parse_frontmatter("x-requires-secrets: key_a\nrequires-secrets: key_b\n");
        assert_eq!(raw.requires_secrets, vec!["key_a"]);
        assert!(raw.deprecated_requires_secrets);
    }

    #[test]
    fn requires_secrets_empty_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_skill(
            dir.path(),
            "no-secrets",
            "---\nname: no-secrets\ndescription: No secrets needed.\n---\nbody",
        );
        let meta = load_skill_meta(&path).unwrap();
        assert!(meta.requires_secrets.is_empty());
    }

    #[test]
    fn requires_secrets_lowercased() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_skill(
            dir.path(),
            "mixed-case",
            "---\nname: mixed-case\ndescription: Case test.\nrequires-secrets: MY-KEY, Another-Key\n---\nbody",
        );
        let meta = load_skill_meta(&path).unwrap();
        assert_eq!(meta.requires_secrets, vec!["my_key", "another_key"]);
    }

    #[test]
    fn requires_secrets_single_value() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_skill(
            dir.path(),
            "single",
            "---\nname: single\ndescription: One secret.\nrequires-secrets: github_token\n---\nbody",
        );
        let meta = load_skill_meta(&path).unwrap();
        assert_eq!(meta.requires_secrets, vec!["github_token"]);
    }

    #[test]
    fn requires_secrets_trailing_comma() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_skill(
            dir.path(),
            "trailing",
            "---\nname: trailing\ndescription: Trailing comma.\nrequires-secrets: key_a, key_b,\n---\nbody",
        );
        let meta = load_skill_meta(&path).unwrap();
        assert_eq!(meta.requires_secrets, vec!["key_a", "key_b"]);
    }

    #[test]
    fn validate_references_valid() {
        let dir = tempfile::tempdir().unwrap();
        let refs = dir.path().join("references");
        std::fs::create_dir_all(&refs).unwrap();
        std::fs::write(refs.join("api.md"), "api docs").unwrap();

        let body = "Use [api docs](references/api.md) for details.";
        let warnings = validate_skill_references(body, dir.path());
        assert!(
            warnings.is_empty(),
            "expected no warnings, got: {warnings:?}"
        );
    }

    #[test]
    fn validate_references_broken_link() {
        let dir = tempfile::tempdir().unwrap();
        let body = "See [missing](references/missing.md).";
        let warnings = validate_skill_references(body, dir.path());
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("broken reference"));
        assert!(warnings[0].contains("references/missing.md"));
    }

    #[test]
    fn validate_references_multiple_links_on_one_line() {
        let dir = tempfile::tempdir().unwrap();
        let refs = dir.path().join("references");
        std::fs::create_dir_all(&refs).unwrap();
        std::fs::write(refs.join("a.md"), "a").unwrap();
        // b.md does not exist

        let body = "See [a](references/a.md) and [b](references/b.md) on the same line.";
        let warnings = validate_skill_references(body, dir.path());
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("references/b.md"));
    }

    #[test]
    fn validate_references_ignores_external_links() {
        let dir = tempfile::tempdir().unwrap();
        let body = "See [external](https://example.com) and [local](docs/guide.md).";
        let warnings = validate_skill_references(body, dir.path());
        assert!(warnings.is_empty());
    }

    #[test]
    fn validate_references_scripts_and_assets() {
        let dir = tempfile::tempdir().unwrap();
        // scripts/run.sh exists, assets/logo.png does not
        let scripts = dir.path().join("scripts");
        std::fs::create_dir_all(&scripts).unwrap();
        std::fs::write(scripts.join("run.sh"), "#!/bin/sh").unwrap();

        let body = "Run [script](scripts/run.sh). See [logo](assets/logo.png).";
        let warnings = validate_skill_references(body, dir.path());
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("assets/logo.png"));
    }

    #[test]
    #[cfg(unix)]
    fn validate_references_rejects_traversal() {
        let base = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let outside_file = outside.path().join("secret.txt");
        std::fs::write(&outside_file, "secret").unwrap();

        let refs = base.path().join("references");
        std::fs::create_dir_all(&refs).unwrap();
        let link = refs.join("evil.md");
        std::os::unix::fs::symlink(&outside_file, &link).unwrap();

        let body = "See [evil](references/evil.md).";
        let warnings = validate_skill_references(body, base.path());
        assert_eq!(warnings.len(), 1);
        assert!(
            warnings[0].contains("unsafe reference"),
            "expected traversal warning, got: {:?}",
            warnings[0]
        );
    }

    #[test]
    fn requires_secrets_underscores_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_skill(
            dir.path(),
            "underscored",
            "---\nname: underscored\ndescription: Already underscored.\nrequires-secrets: my_api_key, another_token\n---\nbody",
        );
        let meta = load_skill_meta(&path).unwrap();
        assert_eq!(meta.requires_secrets, vec!["my_api_key", "another_token"]);
    }

    #[test]
    fn empty_compatibility_produces_none() {
        let raw = parse_frontmatter("compatibility:\n");
        assert!(raw.compatibility.is_none());
    }

    #[test]
    fn empty_license_produces_none() {
        let raw = parse_frontmatter("license:\n");
        assert!(raw.license.is_none());
    }

    #[test]
    fn nonempty_compatibility_produces_some() {
        let raw = parse_frontmatter("compatibility: linux\n");
        assert_eq!(raw.compatibility.as_deref(), Some("linux"));
    }

    #[test]
    fn metadata_value_with_colon() {
        let yaml = "metadata:\n  url: https://example.com\n";
        let raw = parse_frontmatter(yaml);
        assert_eq!(
            raw.metadata,
            vec![("url".into(), "https://example.com".into())]
        );
    }

    #[test]
    fn metadata_empty_block() {
        let yaml = "metadata:\nname: my-skill\n";
        let raw = parse_frontmatter(yaml);
        assert!(raw.metadata.is_empty());
        assert_eq!(raw.name.as_deref(), Some("my-skill"));
    }
}
