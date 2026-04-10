// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! SKILL.md frontmatter parser and file-system loader.
//!
//! Each skill lives in its own directory:
//!
//! ```text
//! skills/
//!   my-skill/
//!     SKILL.md          ← required; frontmatter + Markdown body
//!     scripts/          ← optional helper scripts
//!     references/       ← optional reference documents
//!     assets/           ← optional static assets
//!     .bundled          ← marker written by the bundled provisioner
//! ```
//!
//! # SKILL.md Format
//!
//! ```text
//! ---
//! name: my-skill
//! description: What this skill does and when to invoke it.
//! category: web
//! license: MIT
//! allowed-tools: bash web_scrape
//! x-requires-secrets: MY_API_KEY
//! x-source-url: https://github.com/example/my-skill
//! ---
//!
//! # My Skill
//!
//! Markdown body with usage examples.
//! ```
//!
//! # Frontmatter Fields
//!
//! | Field | Required | Description |
//! |-------|----------|-------------|
//! | `name` | yes | Skill identifier: lowercase letters, digits, hyphens (1–64 chars) |
//! | `description` | yes | One-to-two sentence capability description (max 1024 chars) |
//! | `category` | no | Optional grouping key for two-stage matching |
//! | `license` | no | SPDX license identifier |
//! | `allowed-tools` | no | Space-separated list of tools the skill may invoke |
//! | `x-requires-secrets` | no | Comma-separated vault key names needed at runtime |
//! | `x-source-url` | no | Upstream URL used during install |
//! | `x-git-hash` | no | Git commit hash captured at install time |
//! | `metadata` | no | Arbitrary key-value block for custom attributes |

use std::path::{Path, PathBuf};

use crate::error::SkillError;

/// Parsed frontmatter metadata for a single skill.
///
/// Loaded lazily by [`crate::registry::SkillRegistry`] — the body string is **not**
/// stored here. Use [`crate::registry::SkillRegistry::get_skill`] to retrieve the full
/// [`Skill`] struct including the Markdown body.
#[derive(Clone, Debug)]
pub struct SkillMeta {
    /// Unique skill identifier (`name` frontmatter field).
    pub name: String,
    /// Short capability description used for embedding-based matching.
    pub description: String,
    /// Optional agent version or runtime compatibility constraint.
    pub compatibility: Option<String>,
    /// SPDX license identifier.
    pub license: Option<String>,
    /// Arbitrary key-value pairs from the `metadata:` block.
    pub metadata: Vec<(String, String)>,
    /// Tool names declared in `allowed-tools`.
    pub allowed_tools: Vec<String>,
    /// Vault key names required at runtime (`x-requires-secrets`).
    pub requires_secrets: Vec<String>,
    /// Directory containing this skill's `SKILL.md` and resource subdirectories.
    pub skill_dir: PathBuf,
    /// Upstream URL where this skill was obtained (from `x-source-url` frontmatter field).
    pub source_url: Option<String>,
    /// Upstream git commit hash at install time (from `x-git-hash` frontmatter field).
    pub git_hash: Option<String>,
    /// Optional category grouping (from `category` frontmatter field, e.g. "web", "data", "dev", "system").
    pub category: Option<String>,
}

/// A fully loaded skill: metadata plus the raw Markdown body.
///
/// Obtain via [`crate::registry::SkillRegistry::get_skill`] or
/// [`crate::registry::SkillRegistry::into_skills`].
///
/// # Examples
///
/// ```rust,no_run
/// use zeph_skills::registry::SkillRegistry;
///
/// let registry = SkillRegistry::load(&["/path/to/skills"]);
/// # fn try_main() -> Result<(), zeph_skills::SkillError> {
/// # let registry = zeph_skills::registry::SkillRegistry::load(&["/tmp"]);
/// let skill = registry.get_skill("my-skill")?;
/// println!("name: {}", skill.name());
/// println!("body: {}", skill.body);
/// # Ok(())
/// # }
/// ```
#[derive(Clone, Debug)]
pub struct Skill {
    /// Parsed frontmatter metadata.
    pub meta: SkillMeta,
    /// Raw Markdown body (everything after the closing `---` delimiter).
    pub body: String,
}

impl Skill {
    /// Returns the skill's unique name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.meta.name
    }

    /// Returns the skill's capability description.
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
    source_url: Option<String>,
    git_hash: Option<String>,
    category: Option<String>,
}

/// Validate a skill category name.
///
/// Rules (consistent with skill name validation, minus directory-match and length requirements):
/// - 1–32 characters
/// - Lowercase ASCII letters, digits, and hyphens only
/// - No leading, trailing, or consecutive hyphens
fn validate_category(category: &str) -> Result<(), SkillError> {
    if category.is_empty() || category.len() > 32 {
        return Err(SkillError::Invalid(format!(
            "category must be 1-32 characters, got {}",
            category.len()
        )));
    }
    if !category
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err(SkillError::Invalid(format!(
            "category must contain only lowercase letters, digits, and hyphens: {category}"
        )));
    }
    if category.starts_with('-') || category.ends_with('-') {
        return Err(SkillError::Invalid(format!(
            "category must not start or end with hyphen: {category}"
        )));
    }
    if category.contains("--") {
        return Err(SkillError::Invalid(format!(
            "category must not contain consecutive hyphens: {category}"
        )));
    }
    Ok(())
}

/// Detect whether `value` is a YAML block scalar indicator (`>` or `|`),
/// optionally followed by chomping/indent modifiers (`-`, `+`, digits).
///
/// Returns `Some(folded)` for plain `>` or `|`, and `None` if not a block scalar.
/// Returns an `Err` if a modifier is present (chomping/indent indicators are not supported).
fn detect_block_scalar(value: &str) -> Result<Option<bool>, SkillError> {
    match value {
        ">" => Ok(Some(true)),
        "|" => Ok(Some(false)),
        v if v.starts_with('>') || v.starts_with('|') => Err(SkillError::Invalid(format!(
            "YAML block scalar modifiers are not supported (got '{v}'): \
             use plain '>' or '|' without chomping or indentation indicators"
        ))),
        _ => Ok(None),
    }
}

/// Collect YAML block scalar continuation lines (for `>` folded and `|` literal).
///
/// Returns the assembled string value. Lines after the indicator must be indented.
/// For `>` (folded): blank lines become `\n`, non-blank lines are joined with spaces.
/// For `|` (literal): relative indentation beyond the block indent level is preserved.
fn collect_block_scalar<'a>(
    lines: &mut std::iter::Peekable<impl Iterator<Item = &'a str>>,
    folded: bool,
) -> String {
    // Collect raw (untrimmed) lines that belong to this block scalar.
    // Blank lines between indented lines are part of the block (YAML spec).
    let mut raw_parts: Vec<&'a str> = Vec::new();
    let mut tab_warned = false;

    while let Some(&next) = lines.peek() {
        if next.starts_with(' ') {
            lines.next();
            raw_parts.push(next);
        } else if next.starts_with('\t') {
            // YAML 1.2 forbids tabs for indentation; warn but accept leniently.
            if !tab_warned {
                tracing::warn!(
                    "tab indentation in YAML block scalar is not spec-compliant (YAML 1.2 §8.1)"
                );
                tab_warned = true;
            }
            lines.next();
            raw_parts.push(next);
        } else if next.trim().is_empty() {
            // Blank line: eagerly consume; trailing blanks are stripped below.
            lines.next();
            raw_parts.push("");
        } else {
            break;
        }
    }

    // Strip trailing blank parts (they are outside the block content).
    while raw_parts.last() == Some(&"") {
        raw_parts.pop();
    }

    if raw_parts.is_empty() {
        return String::new();
    }

    if folded {
        // Folded (`>`): trim each line (indentation is not meaningful), then
        // join non-blank runs with spaces and blank lines become paragraph breaks.
        let parts: Vec<&str> = raw_parts.iter().map(|s| s.trim()).collect();
        let mut result = String::new();
        let mut i = 0;
        while i < parts.len() {
            if parts[i].is_empty() {
                result.push('\n');
                i += 1;
            } else {
                let start = i;
                while i < parts.len() && !parts[i].is_empty() {
                    i += 1;
                }
                if !result.is_empty() && !result.ends_with('\n') {
                    result.push(' ');
                }
                result.push_str(&parts[start..i].join(" "));
            }
        }
        result.trim().to_string()
    } else {
        // Literal (`|`): compute block indentation from first non-blank content line,
        // strip exactly that many leading spaces from every line (preserving extra indent).
        let block_indent = raw_parts
            .iter()
            .find(|s| !s.trim().is_empty())
            .map_or(0, |s| s.len() - s.trim_start().len());
        let parts: Vec<&str> = raw_parts
            .iter()
            .map(|s| {
                if s.trim().is_empty() {
                    ""
                } else {
                    &s[block_indent.min(s.len())..]
                }
            })
            .collect();
        parts.join("\n").trim().to_string()
    }
}

/// Apply a parsed inline (non-block-scalar) key-value pair to `raw`.
fn apply_field(raw: &mut RawFrontmatter, key: &str, value: String) {
    match key {
        "name" => raw.name = Some(value),
        "description" => raw.description = Some(value),
        "compatibility" => {
            if !value.is_empty() {
                raw.compatibility = Some(value);
            }
        }
        "license" => {
            if !value.is_empty() {
                raw.license = Some(value);
            }
        }
        "allowed-tools" => {
            raw.allowed_tools = value.split_whitespace().map(ToString::to_string).collect();
        }
        "x-requires-secrets" => {
            raw.requires_secrets = value
                .split(',')
                .map(|s| s.trim().to_lowercase().replace('-', "_"))
                .filter(|s| !s.is_empty())
                .collect();
        }
        "x-source-url" => {
            if !value.is_empty() {
                raw.source_url = Some(value);
            }
        }
        "x-git-hash" => {
            if !value.is_empty() {
                raw.git_hash = Some(value);
            }
        }
        "requires-secrets" => {
            raw.deprecated_requires_secrets = true;
            // Only apply if x-requires-secrets was not already parsed.
            // The canonical x-requires-secrets always wins over the deprecated form.
            if raw.requires_secrets.is_empty() {
                raw.requires_secrets = value
                    .split(',')
                    .map(|s| s.trim().to_lowercase().replace('-', "_"))
                    .filter(|s| !s.is_empty())
                    .collect();
            }
        }
        "category" => {
            if !value.is_empty() {
                match validate_category(&value) {
                    Ok(()) => raw.category = Some(value),
                    Err(e) => tracing::warn!("frontmatter key 'category': {e}"),
                }
            }
        }
        "metadata" if value.is_empty() => {
            // Handled by caller — sets in_metadata flag.
        }
        _ => {
            if !value.is_empty() {
                raw.metadata.push((key.to_string(), value));
            }
        }
    }
}

fn parse_frontmatter(yaml_str: &str) -> RawFrontmatter {
    let mut raw = RawFrontmatter {
        name: None,
        description: None,
        compatibility: None,
        license: None,
        metadata: Vec::new(),
        allowed_tools: Vec::new(),
        requires_secrets: Vec::new(),
        deprecated_requires_secrets: false,
        source_url: None,
        git_hash: None,
        category: None,
    };
    let mut in_metadata = false;

    let mut lines = yaml_str.lines().peekable();

    while let Some(line) = lines.next() {
        if in_metadata {
            if line.starts_with("  ") || line.starts_with('\t') {
                let trimmed = line.trim();
                if let Some((k, v)) = trimmed.split_once(':') {
                    let v = v.trim();
                    if !v.is_empty() {
                        raw.metadata.push((k.trim().to_string(), v.to_string()));
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
            let value = value.trim();

            match detect_block_scalar(value) {
                Err(e) => {
                    tracing::warn!("frontmatter key '{key}': {e}");
                    continue;
                }
                Ok(Some(folded)) => {
                    let collected = collect_block_scalar(&mut lines, folded);
                    match key {
                        "name" => {
                            if !collected.is_empty() {
                                raw.name = Some(collected);
                            }
                        }
                        "description" => {
                            if !collected.is_empty() {
                                raw.description = Some(collected);
                            }
                        }
                        "compatibility" => {
                            if !collected.is_empty() {
                                raw.compatibility = Some(collected);
                            }
                        }
                        "license" => {
                            if !collected.is_empty() {
                                raw.license = Some(collected);
                            }
                        }
                        other => {
                            tracing::warn!(
                                "frontmatter key '{other}' does not support block scalars; value ignored"
                            );
                        }
                    }
                    continue;
                }
                Ok(None) => {}
            }

            let value = value.to_string();
            if key == "metadata" && value.is_empty() {
                in_metadata = true;
            } else {
                apply_field(&mut raw, key, value);
            }
        }
    }

    raw
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

/// Parse a SKILL.md string in memory, returning `(meta, body)`.
///
/// The `skill_dir` in the returned `SkillMeta` is set to `PathBuf::new()` (empty) because
/// no filesystem path is available. Callers that write to disk should update `skill_dir`
/// after saving.
///
/// Unlike [`load_skill_meta`], this variant skips the directory-name ↔ skill-name consistency
/// check, since the skill has not yet been written to any directory.
///
/// # Errors
///
/// Returns `SkillError::Invalid` if frontmatter is missing or fields fail validation.
pub fn load_skill_meta_from_str(content: &str) -> Result<(SkillMeta, String), SkillError> {
    let (yaml_str, body) = split_frontmatter(content)?;
    let raw = parse_frontmatter(yaml_str);

    let name = raw
        .name
        .filter(|s| !s.is_empty())
        .ok_or_else(|| SkillError::Invalid("missing 'name' in frontmatter".into()))?;
    let description = raw
        .description
        .filter(|s| !s.is_empty())
        .ok_or_else(|| SkillError::Invalid("missing 'description' in frontmatter".into()))?;

    if description.len() > 1024 {
        return Err(SkillError::Invalid(format!(
            "description exceeds 1024 characters ({})",
            description.len()
        )));
    }

    if let Some(ref c) = raw.compatibility
        && c.len() > 500
    {
        return Err(SkillError::Invalid(format!(
            "compatibility exceeds 500 characters ({})",
            c.len()
        )));
    }

    if let Some(ref cat) = raw.category {
        validate_category(cat)?;
    }

    if raw.deprecated_requires_secrets {
        tracing::warn!("'requires-secrets' is deprecated, use 'x-requires-secrets'");
    }

    let meta = SkillMeta {
        name,
        description,
        compatibility: raw.compatibility,
        license: raw.license,
        metadata: raw.metadata,
        allowed_tools: raw.allowed_tools,
        requires_secrets: raw.requires_secrets,
        skill_dir: PathBuf::new(),
        source_url: raw.source_url,
        git_hash: raw.git_hash,
        category: raw.category,
    };

    Ok((meta, body.to_string()))
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
        source_url: raw.source_url,
        git_hash: raw.git_hash,
        category: raw.category,
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
    fn description_folded_block_scalar() {
        let yaml = "description: >\n  Create, cancel, and manage periodic tasks.\n  Use when the user wants to schedule recurring work.\n";
        let raw = parse_frontmatter(yaml);
        assert_eq!(
            raw.description.as_deref(),
            Some(
                "Create, cancel, and manage periodic tasks. Use when the user wants to schedule recurring work."
            )
        );
    }

    #[test]
    fn description_literal_block_scalar() {
        let yaml = "description: |\n  First line.\n  Second line.\n";
        let raw = parse_frontmatter(yaml);
        assert_eq!(
            raw.description.as_deref(),
            Some("First line.\nSecond line.")
        );
    }

    #[test]
    fn description_folded_blank_line_becomes_paragraph_break() {
        let yaml = "description: >\n  First paragraph.\n\n  Second paragraph.\n";
        let raw = parse_frontmatter(yaml);
        assert_eq!(
            raw.description.as_deref(),
            Some("First paragraph.\nSecond paragraph.")
        );
    }

    #[test]
    fn compatibility_folded_block_scalar() {
        let yaml = "compatibility: >\n  linux\n  macos\n";
        let raw = parse_frontmatter(yaml);
        assert_eq!(raw.compatibility.as_deref(), Some("linux macos"));
    }

    #[test]
    fn license_literal_block_scalar() {
        let yaml = "license: |\n  MIT OR Apache-2.0\n";
        let raw = parse_frontmatter(yaml);
        assert_eq!(raw.license.as_deref(), Some("MIT OR Apache-2.0"));
    }

    #[test]
    fn block_scalar_followed_by_other_fields() {
        let yaml = "name: my-skill\ndescription: >\n  A folded\n  description here.\ncompatibility: linux\n";
        let raw = parse_frontmatter(yaml);
        assert_eq!(raw.name.as_deref(), Some("my-skill"));
        assert_eq!(
            raw.description.as_deref(),
            Some("A folded description here.")
        );
        assert_eq!(raw.compatibility.as_deref(), Some("linux"));
    }

    #[test]
    fn block_scalar_single_line_folded() {
        let yaml = "description: >\n  Single line only.\n";
        let raw = parse_frontmatter(yaml);
        assert_eq!(raw.description.as_deref(), Some("Single line only."));
    }

    #[test]
    fn full_skill_with_folded_description() {
        let dir = tempfile::tempdir().unwrap();
        let content = "---\nname: my-skill\ndescription: >\n  Create, cancel, and manage periodic tasks.\n  Use when the user wants to schedule recurring work.\n---\n# Body\n";
        let path = write_skill(dir.path(), "my-skill", content);
        let meta = load_skill_meta(&path).unwrap();
        assert_eq!(
            meta.description,
            "Create, cancel, and manage periodic tasks. Use when the user wants to schedule recurring work."
        );
    }

    #[test]
    fn full_skill_with_literal_description() {
        let dir = tempfile::tempdir().unwrap();
        let content =
            "---\nname: my-skill\ndescription: |\n  Line one.\n  Line two.\n---\n# Body\n";
        let path = write_skill(dir.path(), "my-skill", content);
        let meta = load_skill_meta(&path).unwrap();
        assert_eq!(meta.description, "Line one.\nLine two.");
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

    // --- Block scalar edge cases ---

    #[test]
    fn literal_block_scalar_preserves_relative_indentation() {
        // S1: literal | must not strip extra indentation beyond the base level.
        let yaml = "description: |\n  def foo():\n    return 1\n";
        let raw = parse_frontmatter(yaml);
        assert_eq!(raw.description.as_deref(), Some("def foo():\n  return 1"));
    }

    #[test]
    fn chomping_indicator_produces_error_and_is_skipped() {
        // S2: values like `>-`, `>+`, `|-`, `|+`, `>2` must not silently store the indicator.
        let yaml = "description: >-\n  Some text.\nname: my-skill\n";
        let raw = parse_frontmatter(yaml);
        // The unsupported modifier must be rejected; description stays None.
        assert!(
            raw.description.is_none(),
            "expected description=None for unsupported >- modifier, got {:?}",
            raw.description
        );
        // Other fields after the bad line still parse correctly.
        assert_eq!(raw.name.as_deref(), Some("my-skill"));
    }

    #[test]
    fn pipe_modifier_indicator_produces_error_and_is_skipped() {
        let yaml = "description: |+\n  Some text.\nname: my-skill\n";
        let raw = parse_frontmatter(yaml);
        assert!(raw.description.is_none());
        assert_eq!(raw.name.as_deref(), Some("my-skill"));
    }

    #[test]
    fn indent_modifier_indicator_produces_error_and_is_skipped() {
        let yaml = "description: >2\n  Some text.\n";
        let raw = parse_frontmatter(yaml);
        assert!(raw.description.is_none());
    }

    #[test]
    fn empty_block_scalar_produces_none_for_description() {
        // Tester gap 1: `description: >` with no continuation lines → None.
        let yaml = "description: >\nname: my-skill\n";
        let raw = parse_frontmatter(yaml);
        assert!(
            raw.description.is_none(),
            "empty block scalar should produce None for description"
        );
        assert_eq!(raw.name.as_deref(), Some("my-skill"));
    }

    #[test]
    fn block_scalar_as_last_frontmatter_field() {
        // Tester gap 2: block scalar is the last field before closing `---`.
        let dir = tempfile::tempdir().unwrap();
        let content = "---\nname: my-skill\ndescription: >\n  The last field.\n---\n# Body\n";
        let path = write_skill(dir.path(), "my-skill", content);
        let meta = load_skill_meta(&path).unwrap();
        assert_eq!(meta.description, "The last field.");
    }

    #[test]
    fn block_scalar_followed_by_allowed_tools() {
        // Tester gap 3: block scalar description followed by allowed-tools.
        let yaml = "description: >\n  Some desc.\nallowed-tools: bash python\n";
        let raw = parse_frontmatter(yaml);
        assert_eq!(raw.description.as_deref(), Some("Some desc."));
        assert_eq!(raw.allowed_tools, vec!["bash", "python"]);
    }

    #[test]
    fn block_scalar_followed_by_requires_secrets() {
        // Tester gap 4: block scalar description followed by x-requires-secrets.
        let yaml = "description: >\n  Some desc.\nx-requires-secrets: github_token\n";
        let raw = parse_frontmatter(yaml);
        assert_eq!(raw.description.as_deref(), Some("Some desc."));
        assert_eq!(raw.requires_secrets, vec!["github_token"]);
    }

    #[test]
    fn name_folded_block_scalar() {
        // Tester gap 5: name field with > block scalar (unusual but supported).
        // Note: name validation will reject multi-word or invalid names downstream,
        // but the parser should assemble the value correctly.
        let yaml = "name: >\n  my-skill\n";
        let raw = parse_frontmatter(yaml);
        assert_eq!(raw.name.as_deref(), Some("my-skill"));
    }

    #[test]
    fn block_scalar_continuation_trailing_whitespace_stripped() {
        // Tester gap 6: trailing whitespace on continuation lines is stripped.
        let yaml = "description: >\n  Line with spaces.   \n  More text.  \n";
        let raw = parse_frontmatter(yaml);
        assert_eq!(
            raw.description.as_deref(),
            Some("Line with spaces. More text.")
        );
    }

    #[test]
    fn block_scalar_unsupported_key_does_not_populate_allowed_tools() {
        // M2: block scalar on unsupported key (allowed-tools) must be silently discarded.
        let yaml = "allowed-tools: >\n  bash\n  python\n";
        let raw = parse_frontmatter(yaml);
        assert!(
            raw.allowed_tools.is_empty(),
            "block scalar on allowed-tools should be discarded, got {:?}",
            raw.allowed_tools
        );
    }

    #[test]
    fn provenance_fields_parsed_from_frontmatter() {
        let yaml = "x-source-url: https://github.com/example/skill\nx-git-hash: deadbeef\n";
        let raw = parse_frontmatter(yaml);
        assert_eq!(
            raw.source_url.as_deref(),
            Some("https://github.com/example/skill")
        );
        assert_eq!(raw.git_hash.as_deref(), Some("deadbeef"));
    }

    #[test]
    fn provenance_fields_optional() {
        let yaml = "name: git\ndescription: git helper\n";
        let raw = parse_frontmatter(yaml);
        assert!(raw.source_url.is_none());
        assert!(raw.git_hash.is_none());
    }

    #[test]
    fn provenance_fields_empty_value_ignored() {
        let yaml = "x-source-url: \nx-git-hash: \n";
        let raw = parse_frontmatter(yaml);
        assert!(raw.source_url.is_none());
        assert!(raw.git_hash.is_none());
    }

    #[test]
    fn category_valid_stored_on_meta() {
        let raw = parse_frontmatter("category: web\n");
        assert_eq!(raw.category.as_deref(), Some("web"));
    }

    #[test]
    fn category_with_digits_and_hyphens_valid() {
        let raw = parse_frontmatter("category: data-v2\n");
        assert_eq!(raw.category.as_deref(), Some("data-v2"));
    }

    #[test]
    fn category_too_long_ignored() {
        let long = "a".repeat(33);
        let yaml = format!("category: {long}\n");
        let raw = parse_frontmatter(&yaml);
        assert!(raw.category.is_none());
    }

    #[test]
    fn category_exactly_32_chars_valid() {
        let exactly = "a".repeat(32);
        let yaml = format!("category: {exactly}\n");
        let raw = parse_frontmatter(&yaml);
        assert!(raw.category.is_some());
    }

    #[test]
    fn category_uppercase_rejected() {
        let raw = parse_frontmatter("category: Web\n");
        assert!(raw.category.is_none());
    }

    #[test]
    fn category_leading_hyphen_rejected() {
        let raw = parse_frontmatter("category: -web\n");
        assert!(raw.category.is_none());
    }

    #[test]
    fn category_trailing_hyphen_rejected() {
        let raw = parse_frontmatter("category: web-\n");
        assert!(raw.category.is_none());
    }

    #[test]
    fn category_consecutive_hyphens_rejected() {
        let raw = parse_frontmatter("category: web--tools\n");
        assert!(raw.category.is_none());
    }

    #[test]
    fn category_empty_value_produces_none() {
        let raw = parse_frontmatter("category: \n");
        assert!(raw.category.is_none());
    }

    #[test]
    fn category_stored_on_loaded_skill_meta() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_skill(
            dir.path(),
            "my-skill",
            "---\nname: my-skill\ndescription: A test skill.\ncategory: dev\n---\nbody",
        );
        let meta = load_skill_meta(&path).unwrap();
        assert_eq!(meta.category.as_deref(), Some("dev"));
    }
}
