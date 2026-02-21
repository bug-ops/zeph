use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::error::SubAgentError;

// ── Public types ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubAgentDef {
    pub name: String,
    pub description: String,
    pub model: Option<String>,
    pub tools: ToolPolicy,
    pub permissions: SubAgentPermissions,
    pub skills: SkillFilter,
    pub system_prompt: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolPolicy {
    AllowList(Vec<String>),
    DenyList(Vec<String>),
    InheritAll,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubAgentPermissions {
    pub secrets: Vec<String>,
    pub max_turns: u32,
    pub background: bool,
    pub timeout_secs: u64,
    pub ttl_secs: u64,
}

impl Default for SubAgentPermissions {
    fn default() -> Self {
        Self {
            secrets: Vec::new(),
            max_turns: 20,
            background: false,
            timeout_secs: 600,
            ttl_secs: 300,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SkillFilter {
    pub include: Vec<String>,
    pub exclude: Vec<String>,
}

// ── Raw TOML deserialization structs ─────────────────────────────────────────

#[derive(Deserialize)]
struct RawSubAgentDef {
    name: String,
    description: String,
    model: Option<String>,
    #[serde(default)]
    tools: RawToolPolicy,
    #[serde(default)]
    permissions: RawPermissions,
    #[serde(default)]
    skills: RawSkillFilter,
}

#[derive(Default, Deserialize)]
struct RawToolPolicy {
    allow: Option<Vec<String>>,
    deny: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct RawPermissions {
    #[serde(default)]
    secrets: Vec<String>,
    #[serde(default = "default_max_turns")]
    max_turns: u32,
    #[serde(default)]
    background: bool,
    #[serde(default = "default_timeout")]
    timeout_secs: u64,
    #[serde(default = "default_ttl")]
    ttl_secs: u64,
}

impl Default for RawPermissions {
    fn default() -> Self {
        Self {
            secrets: Vec::new(),
            max_turns: default_max_turns(),
            background: false,
            timeout_secs: default_timeout(),
            ttl_secs: default_ttl(),
        }
    }
}

#[derive(Default, Deserialize)]
struct RawSkillFilter {
    #[serde(default)]
    include: Vec<String>,
    #[serde(default)]
    exclude: Vec<String>,
}

fn default_max_turns() -> u32 {
    20
}
fn default_timeout() -> u64 {
    600
}
fn default_ttl() -> u64 {
    300
}

// ── Parser ────────────────────────────────────────────────────────────────────

/// Split TOML frontmatter from markdown body.
///
/// Expected format:
/// ```text
/// +++
/// <toml content>
/// +++
///
/// <body>
/// ```
fn split_toml_frontmatter<'a>(
    content: &'a str,
    path: &str,
) -> Result<(&'a str, &'a str), SubAgentError> {
    let make_err = |reason: &str| SubAgentError::Parse {
        path: path.to_owned(),
        reason: reason.to_owned(),
    };

    let rest = content
        .strip_prefix("+++")
        .and_then(|s| s.strip_prefix('\n').or_else(|| s.strip_prefix("\r\n")))
        .ok_or_else(|| make_err("missing opening `+++` delimiter"))?;

    let (toml_str, after) = rest
        .split_once("\n+++")
        .ok_or_else(|| make_err("missing closing `+++` delimiter"))?;

    // body starts after optional newline following the closing +++
    let body = after
        .strip_prefix('\n')
        .or_else(|| after.strip_prefix("\r\n"))
        .unwrap_or(after);

    Ok((toml_str, body))
}

impl SubAgentDef {
    /// Parse a sub-agent definition from its markdown+TOML frontmatter content.
    ///
    /// # Errors
    ///
    /// Returns [`SubAgentError::Parse`] if the frontmatter delimiters are missing or the
    /// TOML is malformed, and [`SubAgentError::Invalid`] if required fields are empty or
    /// `tools.allow` and `tools.deny` are both specified.
    pub fn parse(content: &str) -> Result<Self, SubAgentError> {
        Self::parse_with_path(content, "<unknown>")
    }

    fn parse_with_path(content: &str, path: &str) -> Result<Self, SubAgentError> {
        let (toml_str, body) = split_toml_frontmatter(content, path)?;

        let raw: RawSubAgentDef = toml::from_str(toml_str).map_err(|e| SubAgentError::Parse {
            path: path.to_owned(),
            reason: e.to_string(),
        })?;

        if raw.name.trim().is_empty() {
            return Err(SubAgentError::Invalid("name must not be empty".into()));
        }
        if raw.description.trim().is_empty() {
            return Err(SubAgentError::Invalid(
                "description must not be empty".into(),
            ));
        }

        let tools = match (raw.tools.allow, raw.tools.deny) {
            (None, None) => ToolPolicy::InheritAll,
            (Some(list), None) => ToolPolicy::AllowList(list),
            (None, Some(list)) => ToolPolicy::DenyList(list),
            (Some(_), Some(_)) => {
                return Err(SubAgentError::Invalid(
                    "tools.allow and tools.deny are mutually exclusive".into(),
                ));
            }
        };

        let p = raw.permissions;
        Ok(Self {
            name: raw.name,
            description: raw.description,
            model: raw.model,
            tools,
            permissions: SubAgentPermissions {
                secrets: p.secrets,
                max_turns: p.max_turns,
                background: p.background,
                timeout_secs: p.timeout_secs,
                ttl_secs: p.ttl_secs,
            },
            skills: SkillFilter {
                include: raw.skills.include,
                exclude: raw.skills.exclude,
            },
            system_prompt: body.trim().to_owned(),
        })
    }

    /// Load a single definition from a `.md` file.
    ///
    /// # Errors
    ///
    /// Returns [`SubAgentError::Parse`] if the file cannot be read or parsed.
    pub fn load(path: &Path) -> Result<Self, SubAgentError> {
        let content = std::fs::read_to_string(path).map_err(|e| SubAgentError::Parse {
            path: path.display().to_string(),
            reason: e.to_string(),
        })?;
        Self::parse_with_path(&content, &path.display().to_string())
    }

    /// Load all definitions from a list of directories.
    ///
    /// Directories are processed in order; when two files share the same agent
    /// `name`, the first one wins (higher-priority path takes precedence).
    /// Non-existent directories are silently skipped.
    ///
    /// # Errors
    ///
    /// Returns [`SubAgentError`] if any `.md` file fails to parse.
    pub fn load_all(dirs: &[PathBuf]) -> Result<Vec<Self>, SubAgentError> {
        let mut seen: HashSet<String> = HashSet::new();
        let mut result = Vec::new();

        for dir in dirs {
            let Ok(read_dir) = std::fs::read_dir(dir) else {
                continue; // directory doesn't exist — skip silently
            };

            let mut entries: Vec<PathBuf> = read_dir
                .filter_map(std::result::Result::ok)
                .map(|e| e.path())
                .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("md"))
                .collect();

            entries.sort(); // deterministic order within a directory

            for path in entries {
                let def = Self::load(&path)?;
                if seen.contains(&def.name) {
                    tracing::debug!(
                        name = %def.name,
                        path = %path.display(),
                        "skipping duplicate sub-agent definition (shadowed by higher-priority path)"
                    );
                    continue;
                }
                seen.insert(def.name.clone());
                result.push(def);
            }
        }

        Ok(result)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const FULL_DEF: &str = r#"+++
name = "code-reviewer"
description = "Reviews code changes for correctness and style"
model = "claude-sonnet-4-20250514"

[tools]
allow = ["shell", "web_scrape"]

[permissions]
secrets = ["github-token"]
max_turns = 10
background = false
timeout_secs = 300
ttl_secs = 120

[skills]
include = ["git-*", "rust-*"]
exclude = ["deploy-*"]
+++

You are a code reviewer. Report findings with severity.
"#;

    const MINIMAL_DEF: &str = "+++\nname = \"bot\"\ndescription = \"A bot\"\n+++\n\nDo things.\n";

    #[test]
    fn parse_full_definition() {
        let def = SubAgentDef::parse(FULL_DEF).unwrap();
        assert_eq!(def.name, "code-reviewer");
        assert_eq!(
            def.description,
            "Reviews code changes for correctness and style"
        );
        assert_eq!(def.model.as_deref(), Some("claude-sonnet-4-20250514"));
        assert!(matches!(def.tools, ToolPolicy::AllowList(ref v) if v == &["shell", "web_scrape"]));
        assert_eq!(def.permissions.max_turns, 10);
        assert_eq!(def.permissions.secrets, ["github-token"]);
        assert_eq!(def.skills.include, ["git-*", "rust-*"]);
        assert_eq!(def.skills.exclude, ["deploy-*"]);
        assert!(def.system_prompt.contains("code reviewer"));
    }

    #[test]
    fn parse_minimal_definition() {
        let def = SubAgentDef::parse(MINIMAL_DEF).unwrap();
        assert_eq!(def.name, "bot");
        assert_eq!(def.description, "A bot");
        assert!(def.model.is_none());
        assert!(matches!(def.tools, ToolPolicy::InheritAll));
        assert_eq!(def.permissions.max_turns, 20);
        assert_eq!(def.permissions.timeout_secs, 600);
        assert_eq!(def.permissions.ttl_secs, 300);
        assert!(!def.permissions.background);
        assert_eq!(def.system_prompt, "Do things.");
    }

    #[test]
    fn tool_policy_deny_list() {
        let content =
            "+++\nname = \"a\"\ndescription = \"b\"\n[tools]\ndeny = [\"shell\"]\n+++\n\nbody\n";
        let def = SubAgentDef::parse(content).unwrap();
        assert!(matches!(def.tools, ToolPolicy::DenyList(ref v) if v == &["shell"]));
    }

    #[test]
    fn tool_policy_inherit_all() {
        let def = SubAgentDef::parse(MINIMAL_DEF).unwrap();
        assert!(matches!(def.tools, ToolPolicy::InheritAll));
    }

    #[test]
    fn tool_policy_both_specified_is_error() {
        let content = "+++\nname = \"a\"\ndescription = \"b\"\n[tools]\nallow = [\"x\"]\ndeny = [\"y\"]\n+++\n\nbody\n";
        let err = SubAgentDef::parse(content).unwrap_err();
        assert!(matches!(err, SubAgentError::Invalid(_)));
    }

    #[test]
    fn missing_opening_delimiter() {
        let err = SubAgentDef::parse("name = \"a\"\n+++\nbody\n").unwrap_err();
        assert!(matches!(err, SubAgentError::Parse { .. }));
    }

    #[test]
    fn missing_closing_delimiter() {
        let err = SubAgentDef::parse("+++\nname = \"a\"\ndescription = \"b\"\n").unwrap_err();
        assert!(matches!(err, SubAgentError::Parse { .. }));
    }

    #[test]
    fn missing_required_field_name() {
        let content = "+++\ndescription = \"b\"\n+++\n\nbody\n";
        let err = SubAgentDef::parse(content).unwrap_err();
        assert!(matches!(err, SubAgentError::Parse { .. }));
    }

    #[test]
    fn missing_required_field_description() {
        let content = "+++\nname = \"a\"\n+++\n\nbody\n";
        let err = SubAgentDef::parse(content).unwrap_err();
        assert!(matches!(err, SubAgentError::Parse { .. }));
    }

    #[test]
    fn empty_name_is_invalid() {
        let content = "+++\nname = \"\"\ndescription = \"b\"\n+++\n\nbody\n";
        let err = SubAgentDef::parse(content).unwrap_err();
        assert!(matches!(err, SubAgentError::Invalid(_)));
    }

    #[test]
    fn load_all_deduplication_by_name() {
        use std::io::Write as _;
        let dir1 = tempfile::tempdir().unwrap();
        let dir2 = tempfile::tempdir().unwrap();

        // Same name "bot" in both dirs — dir1 wins (higher priority)
        let content1 = "+++\nname = \"bot\"\ndescription = \"from dir1\"\n+++\n\ndir1 prompt\n";
        let content2 = "+++\nname = \"bot\"\ndescription = \"from dir2\"\n+++\n\ndir2 prompt\n";

        let mut f1 = std::fs::File::create(dir1.path().join("bot.md")).unwrap();
        f1.write_all(content1.as_bytes()).unwrap();

        let mut f2 = std::fs::File::create(dir2.path().join("bot.md")).unwrap();
        f2.write_all(content2.as_bytes()).unwrap();

        let dirs = vec![dir1.path().to_path_buf(), dir2.path().to_path_buf()];
        let defs = SubAgentDef::load_all(&dirs).unwrap();

        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].description, "from dir1");
    }

    #[test]
    fn default_permissions_values() {
        let p = SubAgentPermissions::default();
        assert_eq!(p.max_turns, 20);
        assert_eq!(p.timeout_secs, 600);
        assert_eq!(p.ttl_secs, 300);
        assert!(!p.background);
        assert!(p.secrets.is_empty());
    }

    #[test]
    fn whitespace_only_description_is_invalid() {
        let content = "+++\nname = \"a\"\ndescription = \"   \"\n+++\n\nbody\n";
        let err = SubAgentDef::parse(content).unwrap_err();
        assert!(matches!(err, SubAgentError::Invalid(_)));
    }

    #[test]
    fn load_nonexistent_file_returns_parse_error() {
        let err =
            SubAgentDef::load(std::path::Path::new("/tmp/does-not-exist-zeph.md")).unwrap_err();
        assert!(matches!(err, SubAgentError::Parse { .. }));
    }

    #[test]
    fn load_all_stops_on_parse_error_mid_scan() {
        use std::io::Write as _;
        let dir = tempfile::tempdir().unwrap();

        let valid = "+++\nname = \"good\"\ndescription = \"ok\"\n+++\n\nbody\n";
        let invalid = "this is not valid frontmatter";

        let mut f1 = std::fs::File::create(dir.path().join("a_good.md")).unwrap();
        f1.write_all(valid.as_bytes()).unwrap();

        let mut f2 = std::fs::File::create(dir.path().join("b_bad.md")).unwrap();
        f2.write_all(invalid.as_bytes()).unwrap();

        let err = SubAgentDef::load_all(&[dir.path().to_path_buf()]).unwrap_err();
        assert!(matches!(err, SubAgentError::Parse { .. }));
    }
}
