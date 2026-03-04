// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::error::SubAgentError;
use super::hooks::SubagentHooks;

/// Maximum allowed size for a sub-agent definition file (256 KiB).
///
/// Files larger than this are rejected before parsing to cap memory usage.
const MAX_DEF_SIZE: usize = 256 * 1024;

/// Maximum number of `.md` files scanned per directory.
///
/// Prevents accidental denial-of-service when `--agents /home` or similar large flat
/// directories are passed. A warning is emitted when the cap is hit.
const MAX_ENTRIES_PER_DIR: usize = 100;

// ── Public types ──────────────────────────────────────────────────────────────

/// Controls tool execution and prompt interactivity for a sub-agent.
///
/// For sub-agents (non-interactive), `Default`, `AcceptEdits`, `DontAsk`, and
/// `BypassPermissions` are functionally equivalent — sub-agents never prompt the
/// user. The meaningful differentiator is `Plan` mode, which suppresses all tool
/// execution and returns only the plan text.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    /// Standard behavior — prompt for each action (sub-agents auto-approve).
    #[default]
    Default,
    /// Auto-accept file edits without prompting.
    AcceptEdits,
    /// Auto-approve all tool calls without prompting.
    DontAsk,
    /// Unrestricted tool access; emits a warning when loaded.
    BypassPermissions,
    /// Read-only planning: tools are visible in the catalog but execution is blocked.
    Plan,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubAgentDef {
    pub name: String,
    pub description: String,
    pub model: Option<String>,
    pub tools: ToolPolicy,
    /// Additional denylist applied after the base `tools` policy.
    ///
    /// Populated from `tools.except` in YAML frontmatter. Deny wins: tools listed
    /// here are blocked even when they appear in `tools.allow`.
    ///
    /// # Serde asymmetry (IMP-CRIT-04)
    ///
    /// Deserialization reads this field from the nested `tools.except` key in YAML/TOML
    /// frontmatter. Serialization (via `#[derive(Serialize)]`) writes it as a flat
    /// top-level `disallowed_tools` key — not under `tools`. Round-trip serialization
    /// is therefore not supported: a serialized `SubAgentDef` cannot be parsed back
    /// as a valid frontmatter file. This is intentional for the current MVP but must
    /// be addressed before v1.0.0 (see GitHub issue filed under IMP-CRIT-04).
    pub disallowed_tools: Vec<String>,
    pub permissions: SubAgentPermissions,
    pub skills: SkillFilter,
    pub system_prompt: String,
    /// Per-agent hooks (`PreToolUse` / `PostToolUse`) from frontmatter.
    ///
    /// Hooks are only honored for project-level and CLI-level definitions.
    /// User-level definitions (~/.zeph/agents/) have hooks stripped on load.
    pub hooks: SubagentHooks,
    /// Scope label and filename of the definition file (populated by `load` / `load_all`).
    ///
    /// Stored as `"<scope>/<filename>"` (e.g., `"project/my-agent.md"`).
    /// The full absolute path is intentionally not stored to avoid leaking local
    /// filesystem layout in diagnostics and `/agent list` output.
    #[serde(skip)]
    pub source: Option<String>,
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
    pub permission_mode: PermissionMode,
}

impl Default for SubAgentPermissions {
    fn default() -> Self {
        Self {
            secrets: Vec::new(),
            max_turns: 20,
            background: false,
            timeout_secs: 600,
            ttl_secs: 300,
            permission_mode: PermissionMode::Default,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SkillFilter {
    pub include: Vec<String>,
    pub exclude: Vec<String>,
}

// ── Raw deserialization structs ───────────────────────────────────────────────
// These work for both YAML and TOML deserializers — only the deserializer call
// differs based on detected frontmatter format.

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
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
    #[serde(default)]
    hooks: SubagentHooks,
}

// Note: `RawToolPolicy` and `RawPermissions` intentionally do not carry
// `#[serde(deny_unknown_fields)]`. They are nested under `RawSubAgentDef` (which does have
// `deny_unknown_fields`), but serde does not propagate that attribute into nested structs.
// Adding it here would reject currently-valid frontmatter that omits optional fields via
// serde's default mechanism. A follow-up issue should evaluate whether strict rejection of
// unknown nested keys is desirable before adding it.
#[derive(Default, Deserialize)]
struct RawToolPolicy {
    allow: Option<Vec<String>>,
    deny: Option<Vec<String>>,
    /// Additional denylist applied on top of `allow` or `deny`. Use `tools.except` to
    /// block specific tools while still using an allow-list (deny wins over allow).
    #[serde(default)]
    except: Vec<String>,
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
    #[serde(default)]
    permission_mode: PermissionMode,
}

impl Default for RawPermissions {
    fn default() -> Self {
        Self {
            secrets: Vec::new(),
            max_turns: default_max_turns(),
            background: false,
            timeout_secs: default_timeout(),
            ttl_secs: default_ttl(),
            permission_mode: PermissionMode::Default,
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

// ── Frontmatter format detection ──────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FrontmatterFormat {
    Yaml,
    Toml,
}

/// Split frontmatter from markdown body, detecting format from opening delimiter.
///
/// YAML frontmatter (primary):
/// ```text
/// ---
/// <yaml content>
/// ---
///
/// <body>
/// ```
///
/// TOML frontmatter (deprecated):
/// ```text
/// +++
/// <toml content>
/// +++
///
/// <body>
/// ```
fn split_frontmatter<'a>(
    content: &'a str,
    path: &str,
) -> Result<(&'a str, &'a str, FrontmatterFormat), SubAgentError> {
    let make_err = |reason: &str| SubAgentError::Parse {
        path: path.to_owned(),
        reason: reason.to_owned(),
    };

    if let Some(rest) = content
        .strip_prefix("---")
        .and_then(|s| s.strip_prefix('\n').or_else(|| s.strip_prefix("\r\n")))
    {
        // YAML: closing delimiter is \n---\n or \n--- at EOF.
        // Note: `split_once("\n---")` matches `\r\n---` because `\r\n` contains `\n`.
        // The leading `\r` is left in `yaml_str` but removed by CRLF normalization in
        // `parse_with_path`. Do not remove that normalization without updating this search.
        let (yaml_str, after) = rest
            .split_once("\n---")
            .ok_or_else(|| make_err("missing closing `---` delimiter for YAML frontmatter"))?;
        let body = after
            .strip_prefix('\n')
            .or_else(|| after.strip_prefix("\r\n"))
            .unwrap_or(after);
        return Ok((yaml_str, body, FrontmatterFormat::Yaml));
    }

    if let Some(rest) = content
        .strip_prefix("+++")
        .and_then(|s| s.strip_prefix('\n').or_else(|| s.strip_prefix("\r\n")))
    {
        // Same CRLF note as YAML branch above: trailing `\r` is cleaned by normalization.
        let (toml_str, after) = rest
            .split_once("\n+++")
            .ok_or_else(|| make_err("missing closing `+++` delimiter for TOML frontmatter"))?;
        let body = after
            .strip_prefix('\n')
            .or_else(|| after.strip_prefix("\r\n"))
            .unwrap_or(after);
        return Ok((toml_str, body, FrontmatterFormat::Toml));
    }

    Err(make_err(
        "missing frontmatter delimiters: expected `---` (YAML) or `+++` (TOML, deprecated)",
    ))
}

impl SubAgentDef {
    /// Parse a sub-agent definition from its frontmatter+markdown content.
    ///
    /// The primary format uses YAML frontmatter delimited by `---`:
    ///
    /// ```text
    /// ---
    /// name: my-agent
    /// description: Does something useful
    /// model: claude-sonnet-4-20250514
    /// tools:
    ///   allow:
    ///     - shell
    /// permissions:
    ///   max_turns: 10
    /// skills:
    ///   include:
    ///     - "git-*"
    /// ---
    ///
    /// You are a helpful agent.
    /// ```
    ///
    /// TOML frontmatter (`+++`) is supported as a deprecated fallback and will emit a
    /// `tracing::warn!` message. It will be removed in v1.0.0.
    ///
    /// # Errors
    ///
    /// Returns [`SubAgentError::Parse`] if the frontmatter delimiters are missing or the
    /// content is malformed, and [`SubAgentError::Invalid`] if required fields are empty or
    /// `tools.allow` and `tools.deny` are both specified.
    pub fn parse(content: &str) -> Result<Self, SubAgentError> {
        Self::parse_with_path(content, "<unknown>")
    }

    fn parse_with_path(content: &str, path: &str) -> Result<Self, SubAgentError> {
        let (frontmatter_str, body, format) = split_frontmatter(content, path)?;

        let raw: RawSubAgentDef = match format {
            FrontmatterFormat::Yaml => {
                // Normalize CRLF so numeric/bool fields parse correctly on Windows line endings.
                let yaml_normalized;
                let yaml_str = if frontmatter_str.contains('\r') {
                    yaml_normalized = frontmatter_str.replace("\r\n", "\n").replace('\r', "\n");
                    &yaml_normalized
                } else {
                    frontmatter_str
                };
                serde_norway::from_str(yaml_str).map_err(|e| SubAgentError::Parse {
                    path: path.to_owned(),
                    reason: e.to_string(),
                })?
            }
            FrontmatterFormat::Toml => {
                tracing::warn!(
                    path,
                    "sub-agent definition uses deprecated +++ TOML frontmatter, migrate to --- YAML"
                );
                // Normalize CRLF — the `toml` crate rejects bare `\r`.
                let toml_normalized;
                let toml_str = if frontmatter_str.contains('\r') {
                    toml_normalized = frontmatter_str.replace("\r\n", "\n").replace('\r', "\n");
                    &toml_normalized
                } else {
                    frontmatter_str
                };
                toml::from_str(toml_str).map_err(|e| SubAgentError::Parse {
                    path: path.to_owned(),
                    reason: e.to_string(),
                })?
            }
        };

        if raw.name.trim().is_empty() {
            return Err(SubAgentError::Invalid("name must not be empty".into()));
        }
        if raw.description.trim().is_empty() {
            return Err(SubAgentError::Invalid(
                "description must not be empty".into(),
            ));
        }
        if raw
            .name
            .chars()
            .any(|c| (c < '\x20' && c != '\t') || c == '\x7F')
        {
            return Err(SubAgentError::Invalid(
                "name must not contain control characters".into(),
            ));
        }
        if raw
            .description
            .chars()
            .any(|c| (c < '\x20' && c != '\t') || c == '\x7F')
        {
            return Err(SubAgentError::Invalid(
                "description must not contain control characters".into(),
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

        let disallowed_tools = raw.tools.except;

        let p = raw.permissions;
        if p.permission_mode == PermissionMode::BypassPermissions {
            tracing::warn!(
                name = %raw.name,
                "sub-agent definition uses bypass_permissions mode — grants unrestricted tool access"
            );
        }
        Ok(Self {
            name: raw.name,
            description: raw.description,
            model: raw.model,
            tools,
            disallowed_tools,
            permissions: SubAgentPermissions {
                secrets: p.secrets,
                max_turns: p.max_turns,
                background: p.background,
                timeout_secs: p.timeout_secs,
                ttl_secs: p.ttl_secs,
                permission_mode: p.permission_mode,
            },
            skills: SkillFilter {
                include: raw.skills.include,
                exclude: raw.skills.exclude,
            },
            hooks: raw.hooks,
            system_prompt: body.trim().to_owned(),
            source: None,
        })
    }

    /// Load a single definition from a `.md` file.
    ///
    /// When `boundary` is provided, the file's canonical path must start with
    /// `boundary` — this rejects symlinks that escape the allowed directory.
    ///
    /// # Errors
    ///
    /// Returns [`SubAgentError::Parse`] if the file cannot be read, exceeds 256 KiB,
    /// escapes the boundary via symlink, or fails to parse.
    pub fn load(path: &Path) -> Result<Self, SubAgentError> {
        Self::load_with_boundary(path, None, None)
    }

    /// Load with optional symlink boundary and scope label for the `source` field.
    pub(crate) fn load_with_boundary(
        path: &Path,
        boundary: Option<&Path>,
        scope: Option<&str>,
    ) -> Result<Self, SubAgentError> {
        let path_str = path.display().to_string();

        // Canonicalize to resolve any symlinks before reading.
        let canonical = std::fs::canonicalize(path).map_err(|e| SubAgentError::Parse {
            path: path_str.clone(),
            reason: format!("cannot resolve path: {e}"),
        })?;

        // Boundary check: reject symlinks that escape the allowed directory.
        if let Some(boundary) = boundary
            && !canonical.starts_with(boundary)
        {
            return Err(SubAgentError::Parse {
                path: path_str.clone(),
                reason: format!(
                    "definition file escapes allowed directory boundary ({})",
                    boundary.display()
                ),
            });
        }

        let content = std::fs::read_to_string(&canonical).map_err(|e| SubAgentError::Parse {
            path: path_str.clone(),
            reason: e.to_string(),
        })?;
        if content.len() > MAX_DEF_SIZE {
            return Err(SubAgentError::Parse {
                path: path_str.clone(),
                reason: format!(
                    "definition file exceeds maximum size of {} KiB",
                    MAX_DEF_SIZE / 1024
                ),
            });
        }
        let mut def = Self::parse_with_path(&content, &path_str)?;

        // Security: strip hooks from user-level definitions — only project-level
        // (scope = "project") and CLI-level (scope = "cli" or None) definitions may
        // carry hooks. User-level agents come from ~/.zeph/agents/ and are untrusted.
        if scope == Some("user") {
            if !def.hooks.pre_tool_use.is_empty() || !def.hooks.post_tool_use.is_empty() {
                tracing::warn!(
                    path = %path_str,
                    "user-level agent definition contains hooks — stripping for security"
                );
            }
            def.hooks = SubagentHooks::default();
        }

        // Populate source as "<scope>/<filename>" — no full path to avoid privacy leak.
        let filename = path
            .file_name()
            .and_then(|f| f.to_str())
            .unwrap_or("<unknown>");
        def.source = Some(if let Some(scope) = scope {
            format!("{scope}/{filename}")
        } else {
            filename.to_owned()
        });

        Ok(def)
    }

    /// Load all definitions from a list of paths (files or directories).
    ///
    /// Paths are processed in order; when two entries share the same agent
    /// `name`, the first one wins (higher-priority path takes precedence).
    /// Non-existent directories are silently skipped.
    ///
    /// For directory entries from user/extra dirs: parse errors are warned and skipped.
    /// For CLI file entries (`is_cli_source = true`): parse errors are hard failures.
    ///
    /// # Errors
    ///
    /// Returns [`SubAgentError`] if a CLI-sourced `.md` file fails to parse.
    pub fn load_all(paths: &[PathBuf]) -> Result<Vec<Self>, SubAgentError> {
        Self::load_all_with_sources(paths, &[], None, &[])
    }

    /// Load all definitions with scope context for source tracking and security checks.
    ///
    /// `cli_agents` — CLI paths (hard errors on parse failure, no boundary check).
    /// `config_user_dir` — optional user-level dir override.
    /// `extra_dirs` — extra dirs from config.
    ///
    /// # Errors
    ///
    /// Returns [`SubAgentError`] if a CLI-sourced `.md` file fails to parse.
    pub fn load_all_with_sources(
        ordered_paths: &[PathBuf],
        cli_agents: &[PathBuf],
        config_user_dir: Option<&PathBuf>,
        extra_dirs: &[PathBuf],
    ) -> Result<Vec<Self>, SubAgentError> {
        let mut seen: HashSet<String> = HashSet::new();
        let mut result = Vec::new();

        for path in ordered_paths {
            if path.is_file() {
                // Single file path: only CLI --agents flag produces file entries in ordered_paths
                // (project/user/extra_dirs are always directories). Scope label "cli" is
                // therefore always correct here.
                let is_cli = cli_agents.iter().any(|c| c == path);
                match Self::load_with_boundary(path, None, Some("cli")) {
                    Ok(def) => {
                        if seen.contains(&def.name) {
                            tracing::debug!(
                                name = %def.name,
                                path = %path.display(),
                                "skipping duplicate sub-agent definition"
                            );
                        } else {
                            seen.insert(def.name.clone());
                            result.push(def);
                        }
                    }
                    Err(e) if is_cli => return Err(e),
                    Err(e) => {
                        tracing::warn!(path = %path.display(), error = %e, "skipping malformed agent definition");
                    }
                }
                continue;
            }

            let Ok(read_dir) = std::fs::read_dir(path) else {
                continue; // directory doesn't exist — skip silently
            };

            // Compute boundary for symlink protection on non-project directories.
            // Project dir (.zeph/agents) is trusted; user/extra dirs get boundary checks.
            let is_cli_dir = cli_agents.iter().any(|c| c == path);
            let is_project_dir = path == &PathBuf::from(".zeph/agents");
            let boundary = if is_cli_dir || is_project_dir {
                None
            } else {
                // Canonicalize the directory itself as the boundary.
                std::fs::canonicalize(path).ok()
            };

            let scope = super::resolve::scope_label(path, cli_agents, config_user_dir, extra_dirs);
            let is_cli_scope = is_cli_dir;

            let mut entries: Vec<PathBuf> = read_dir
                .filter_map(std::result::Result::ok)
                .map(|e| e.path())
                .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("md"))
                .collect();

            entries.sort(); // deterministic order within a directory

            if entries.len() > MAX_ENTRIES_PER_DIR {
                tracing::warn!(
                    dir = %path.display(),
                    count = entries.len(),
                    cap = MAX_ENTRIES_PER_DIR,
                    "agent directory exceeds entry cap; processing only first {MAX_ENTRIES_PER_DIR} files"
                );
                entries.truncate(MAX_ENTRIES_PER_DIR);
            }

            for entry_path in entries {
                let load_result =
                    Self::load_with_boundary(&entry_path, boundary.as_deref(), Some(scope));

                let def = match load_result {
                    Ok(d) => d,
                    Err(e) if is_cli_scope => return Err(e),
                    Err(e) => {
                        tracing::warn!(
                            path = %entry_path.display(),
                            error = %e,
                            "skipping malformed agent definition"
                        );
                        continue;
                    }
                };

                if seen.contains(&def.name) {
                    tracing::debug!(
                        name = %def.name,
                        path = %entry_path.display(),
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
    use indoc::indoc;

    use super::*;

    // ── YAML fixtures (primary format) ─────────────────────────────────────────

    const FULL_DEF_YAML: &str = indoc! {"
        ---
        name: code-reviewer
        description: Reviews code changes for correctness and style
        model: claude-sonnet-4-20250514
        tools:
          allow:
            - shell
            - web_scrape
        permissions:
          secrets:
            - github-token
          max_turns: 10
          background: false
          timeout_secs: 300
          ttl_secs: 120
        skills:
          include:
            - \"git-*\"
            - \"rust-*\"
          exclude:
            - \"deploy-*\"
        ---

        You are a code reviewer. Report findings with severity.
    "};

    const MINIMAL_DEF_YAML: &str = indoc! {"
        ---
        name: bot
        description: A bot
        ---

        Do things.
    "};

    // ── TOML fixtures (deprecated fallback) ────────────────────────────────────

    const FULL_DEF_TOML: &str = indoc! {"
        +++
        name = \"code-reviewer\"
        description = \"Reviews code changes for correctness and style\"
        model = \"claude-sonnet-4-20250514\"

        [tools]
        allow = [\"shell\", \"web_scrape\"]

        [permissions]
        secrets = [\"github-token\"]
        max_turns = 10
        background = false
        timeout_secs = 300
        ttl_secs = 120

        [skills]
        include = [\"git-*\", \"rust-*\"]
        exclude = [\"deploy-*\"]
        +++

        You are a code reviewer. Report findings with severity.
    "};

    const MINIMAL_DEF_TOML: &str = indoc! {"
        +++
        name = \"bot\"
        description = \"A bot\"
        +++

        Do things.
    "};

    // ── YAML tests ─────────────────────────────────────────────────────────────

    #[test]
    fn parse_yaml_full_definition() {
        let def = SubAgentDef::parse(FULL_DEF_YAML).unwrap();
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
    fn parse_yaml_minimal_definition() {
        let def = SubAgentDef::parse(MINIMAL_DEF_YAML).unwrap();
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
    fn parse_yaml_with_dashes_in_body() {
        // --- in the body after the closing --- delimiter must not break the parser
        let content = "---\nname: agent\ndescription: desc\n---\n\nSome text\n---\nMore text\n";
        let def = SubAgentDef::parse(content).unwrap();
        assert_eq!(def.name, "agent");
        assert!(def.system_prompt.contains("Some text"));
        assert!(def.system_prompt.contains("More text"));
    }

    #[test]
    fn parse_yaml_tool_deny_list() {
        let content = "---\nname: a\ndescription: b\ntools:\n  deny:\n    - shell\n---\n\nbody\n";
        let def = SubAgentDef::parse(content).unwrap();
        assert!(matches!(def.tools, ToolPolicy::DenyList(ref v) if v == &["shell"]));
    }

    #[test]
    fn parse_yaml_tool_inherit_all() {
        // Explicit tools section with neither allow nor deny also yields InheritAll.
        let content = "---\nname: a\ndescription: b\ntools: {}\n---\n\nbody\n";
        let def = SubAgentDef::parse(content).unwrap();
        assert!(matches!(def.tools, ToolPolicy::InheritAll));
    }

    #[test]
    fn parse_yaml_tool_both_specified_is_error() {
        let content = "---\nname: a\ndescription: b\ntools:\n  allow:\n    - x\n  deny:\n    - y\n---\n\nbody\n";
        let err = SubAgentDef::parse(content).unwrap_err();
        assert!(matches!(err, SubAgentError::Invalid(_)));
    }

    #[test]
    fn parse_yaml_missing_closing_delimiter() {
        let err = SubAgentDef::parse("---\nname: a\ndescription: b\n").unwrap_err();
        assert!(matches!(err, SubAgentError::Parse { .. }));
    }

    #[test]
    fn parse_yaml_crlf_line_endings() {
        let content = "---\r\nname: bot\r\ndescription: A bot\r\n---\r\n\r\nDo things.\r\n";
        let def = SubAgentDef::parse(content).unwrap();
        assert_eq!(def.name, "bot");
        assert_eq!(def.description, "A bot");
        assert!(!def.system_prompt.is_empty());
    }

    #[test]
    fn parse_yaml_missing_required_field_name() {
        let content = "---\ndescription: b\n---\n\nbody\n";
        let err = SubAgentDef::parse(content).unwrap_err();
        assert!(matches!(err, SubAgentError::Parse { .. }));
    }

    #[test]
    fn parse_yaml_missing_required_field_description() {
        let content = "---\nname: a\n---\n\nbody\n";
        let err = SubAgentDef::parse(content).unwrap_err();
        assert!(matches!(err, SubAgentError::Parse { .. }));
    }

    #[test]
    fn parse_yaml_empty_name_is_invalid() {
        let content = "---\nname: \"\"\ndescription: b\n---\n\nbody\n";
        let err = SubAgentDef::parse(content).unwrap_err();
        assert!(matches!(err, SubAgentError::Invalid(_)));
    }

    #[test]
    fn parse_yaml_whitespace_only_description_is_invalid() {
        let content = "---\nname: a\ndescription: \"   \"\n---\n\nbody\n";
        let err = SubAgentDef::parse(content).unwrap_err();
        assert!(matches!(err, SubAgentError::Invalid(_)));
    }

    #[test]
    fn parse_yaml_crlf_with_numeric_fields() {
        let content = "---\r\nname: bot\r\ndescription: A bot\r\npermissions:\r\n  max_turns: 5\r\n  timeout_secs: 120\r\n---\r\n\r\nDo things.\r\n";
        let def = SubAgentDef::parse(content).unwrap();
        assert_eq!(def.permissions.max_turns, 5);
        assert_eq!(def.permissions.timeout_secs, 120);
    }

    #[test]
    fn parse_yaml_no_trailing_newline() {
        let content = "---\nname: a\ndescription: b\n---";
        let def = SubAgentDef::parse(content).unwrap();
        assert_eq!(def.system_prompt, "");
    }

    // ── TOML deprecated fallback tests ─────────────────────────────────────────

    #[test]
    fn parse_full_definition() {
        let def = SubAgentDef::parse(FULL_DEF_TOML).unwrap();
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
        let def = SubAgentDef::parse(MINIMAL_DEF_TOML).unwrap();
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
        let def = SubAgentDef::parse(MINIMAL_DEF_TOML).unwrap();
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

        let content1 = "---\nname: bot\ndescription: from dir1\n---\n\ndir1 prompt\n";
        let content2 = "---\nname: bot\ndescription: from dir2\n---\n\ndir2 prompt\n";

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
    fn parse_crlf_line_endings() {
        let content =
            "+++\r\nname = \"bot\"\r\ndescription = \"A bot\"\r\n+++\r\n\r\nDo things.\r\n";
        let def = SubAgentDef::parse(content).unwrap();
        assert_eq!(def.name, "bot");
        assert_eq!(def.description, "A bot");
        assert!(!def.system_prompt.is_empty());
    }

    #[test]
    fn parse_crlf_closing_delimiter() {
        let content = "+++\r\nname = \"bot\"\r\ndescription = \"A bot\"\r\n+++\r\nPrompt here.\r\n";
        let def = SubAgentDef::parse(content).unwrap();
        assert!(def.system_prompt.contains("Prompt here"));
    }

    #[test]
    fn load_all_warn_and_skip_on_parse_error_for_non_cli_source() {
        use std::io::Write as _;
        let dir = tempfile::tempdir().unwrap();

        let valid = "---\nname: good\ndescription: ok\n---\n\nbody\n";
        let invalid = "this is not valid frontmatter";

        let mut f1 = std::fs::File::create(dir.path().join("a_good.md")).unwrap();
        f1.write_all(valid.as_bytes()).unwrap();

        let mut f2 = std::fs::File::create(dir.path().join("b_bad.md")).unwrap();
        f2.write_all(invalid.as_bytes()).unwrap();

        // Non-CLI source: bad file is warned and skipped, good file is loaded.
        let defs = SubAgentDef::load_all(&[dir.path().to_path_buf()]).unwrap();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "good");
    }

    #[test]
    fn load_all_with_sources_hard_error_for_cli_file() {
        use std::io::Write as _;
        let dir = tempfile::tempdir().unwrap();

        let invalid = "this is not valid frontmatter";
        let bad_path = dir.path().join("bad.md");
        let mut f = std::fs::File::create(&bad_path).unwrap();
        f.write_all(invalid.as_bytes()).unwrap();

        // CLI source: bad file causes hard error.
        let err = SubAgentDef::load_all_with_sources(&[bad_path.clone()], &[bad_path], None, &[])
            .unwrap_err();
        assert!(matches!(err, SubAgentError::Parse { .. }));
    }

    #[test]
    fn load_all_with_sources_max_entries_per_dir_cap() {
        // Create MAX_ENTRIES_PER_DIR + 10 files; only first 100 should be loaded.
        let dir = tempfile::tempdir().unwrap();
        let total = MAX_ENTRIES_PER_DIR + 10;
        for i in 0..total {
            let content =
                format!("---\nname: agent-{i:04}\ndescription: Agent {i}\n---\n\nBody {i}\n");
            std::fs::write(dir.path().join(format!("agent-{i:04}.md")), &content).unwrap();
        }
        let defs = SubAgentDef::load_all(&[dir.path().to_path_buf()]).unwrap();
        assert_eq!(
            defs.len(),
            MAX_ENTRIES_PER_DIR,
            "must cap at MAX_ENTRIES_PER_DIR=100"
        );
    }

    #[test]
    fn load_with_boundary_rejects_symlink_escape() {
        // Create two separate dirs. Place a real file in dir_b, then create a symlink in
        // dir_a pointing to the file in dir_b. Loading with dir_a as boundary must fail.
        let dir_a = tempfile::tempdir().unwrap();
        let dir_b = tempfile::tempdir().unwrap();

        let real_file = dir_b.path().join("agent.md");
        std::fs::write(
            &real_file,
            "---\nname: escape\ndescription: Escaped\n---\n\nBody\n",
        )
        .unwrap();

        #[cfg(not(unix))]
        {
            // Symlink boundary test is unix-specific; skip on other platforms.
            let _ = (dir_a, dir_b, real_file);
            return;
        }

        #[cfg(unix)]
        {
            let link_path = dir_a.path().join("agent.md");
            std::os::unix::fs::symlink(&real_file, &link_path).unwrap();
            let boundary = std::fs::canonicalize(dir_a.path()).unwrap();
            let err =
                SubAgentDef::load_with_boundary(&link_path, Some(&boundary), None).unwrap_err();
            assert!(
                matches!(&err, SubAgentError::Parse { reason, .. } if reason.contains("escapes allowed directory boundary")),
                "expected boundary violation error, got: {err}"
            );
        }
    }

    #[test]
    fn load_all_with_sources_source_field_has_correct_scope_label() {
        use std::io::Write as _;
        // Create a dir that will be treated as the user-level dir.
        let user_dir = tempfile::tempdir().unwrap();
        let user_dir_path = user_dir.path().to_path_buf();
        let content = "---\nname: my-agent\ndescription: test\n---\n\nBody\n";
        let mut f = std::fs::File::create(user_dir_path.join("my-agent.md")).unwrap();
        f.write_all(content.as_bytes()).unwrap();

        // Use user_dir as config_user_dir so scope_label returns "user".
        let paths = vec![user_dir_path.clone()];
        let defs =
            SubAgentDef::load_all_with_sources(&paths, &[], Some(&user_dir_path), &[]).unwrap();

        assert_eq!(defs.len(), 1);
        let source = defs[0].source.as_deref().unwrap_or("");
        assert!(
            source.starts_with("user/"),
            "expected source to start with 'user/', got: {source}"
        );
    }

    #[test]
    fn load_all_with_sources_priority_first_name_wins() {
        use std::io::Write as _;
        let dir1 = tempfile::tempdir().unwrap();
        let dir2 = tempfile::tempdir().unwrap();

        // Both dirs contain an agent with the same name "bot".
        let content1 = "---\nname: bot\ndescription: from dir1\n---\n\ndir1 prompt\n";
        let content2 = "---\nname: bot\ndescription: from dir2\n---\n\ndir2 prompt\n";

        let mut f1 = std::fs::File::create(dir1.path().join("bot.md")).unwrap();
        f1.write_all(content1.as_bytes()).unwrap();
        let mut f2 = std::fs::File::create(dir2.path().join("bot.md")).unwrap();
        f2.write_all(content2.as_bytes()).unwrap();

        // dir1 is first (higher priority), dir2 is second.
        let paths = vec![dir1.path().to_path_buf(), dir2.path().to_path_buf()];
        let defs = SubAgentDef::load_all_with_sources(&paths, &[], None, &[]).unwrap();

        assert_eq!(defs.len(), 1, "name collision: only first wins");
        assert_eq!(defs[0].description, "from dir1");
    }

    #[test]
    fn load_all_with_sources_user_agents_dir_none_skips_gracefully() {
        // When config_user_dir is not provided to load_all_with_sources (None),
        // and the resolved ordered_paths has no user dir entry, loading must succeed.
        let dir = tempfile::tempdir().unwrap();
        let content = "---\nname: ok\ndescription: fine\n---\n\nBody\n";
        std::fs::write(dir.path().join("ok.md"), content).unwrap();

        // Pass only project-level-like path — no user dir at all.
        let paths = vec![dir.path().to_path_buf()];
        let defs = SubAgentDef::load_all_with_sources(&paths, &[], None, &[]).unwrap();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "ok");
    }

    // ── PermissionMode tests ────────────────────────────────────────────────

    #[test]
    fn parse_yaml_permission_mode_default_when_omitted() {
        let def = SubAgentDef::parse(MINIMAL_DEF_YAML).unwrap();
        assert_eq!(def.permissions.permission_mode, PermissionMode::Default);
    }

    #[test]
    fn parse_yaml_permission_mode_dont_ask() {
        let content = "---\nname: a\ndescription: b\npermissions:\n  permission_mode: dont_ask\n---\n\nbody\n";
        let def = SubAgentDef::parse(content).unwrap();
        assert_eq!(def.permissions.permission_mode, PermissionMode::DontAsk);
    }

    #[test]
    fn parse_yaml_permission_mode_accept_edits() {
        let content = "---\nname: a\ndescription: b\npermissions:\n  permission_mode: accept_edits\n---\n\nbody\n";
        let def = SubAgentDef::parse(content).unwrap();
        assert_eq!(def.permissions.permission_mode, PermissionMode::AcceptEdits);
    }

    #[test]
    fn parse_yaml_permission_mode_bypass_permissions() {
        let content = "---\nname: a\ndescription: b\npermissions:\n  permission_mode: bypass_permissions\n---\n\nbody\n";
        let def = SubAgentDef::parse(content).unwrap();
        assert_eq!(
            def.permissions.permission_mode,
            PermissionMode::BypassPermissions
        );
    }

    #[test]
    fn parse_yaml_permission_mode_plan() {
        let content =
            "---\nname: a\ndescription: b\npermissions:\n  permission_mode: plan\n---\n\nbody\n";
        let def = SubAgentDef::parse(content).unwrap();
        assert_eq!(def.permissions.permission_mode, PermissionMode::Plan);
    }

    #[test]
    fn parse_yaml_disallowed_tools_from_except() {
        let content = "---\nname: a\ndescription: b\ntools:\n  allow:\n    - shell\n    - web\n  except:\n    - shell\n---\n\nbody\n";
        let def = SubAgentDef::parse(content).unwrap();
        assert!(
            matches!(def.tools, ToolPolicy::AllowList(ref v) if v.contains(&"shell".to_owned()))
        );
        assert_eq!(def.disallowed_tools, ["shell"]);
    }

    #[test]
    fn parse_yaml_disallowed_tools_empty_when_no_except() {
        let def = SubAgentDef::parse(MINIMAL_DEF_YAML).unwrap();
        assert!(def.disallowed_tools.is_empty());
    }

    #[test]
    fn parse_yaml_all_new_fields_together() {
        let content = indoc! {"
            ---
            name: planner
            description: Plans things
            tools:
              allow:
                - shell
                - web
              except:
                - dangerous
            permissions:
              max_turns: 5
              background: true
              permission_mode: plan
            ---

            You are a planner.
        "};
        let def = SubAgentDef::parse(content).unwrap();
        assert_eq!(def.permissions.permission_mode, PermissionMode::Plan);
        assert!(def.permissions.background);
        assert_eq!(def.permissions.max_turns, 5);
        assert_eq!(def.disallowed_tools, ["dangerous"]);
    }

    #[test]
    fn default_permissions_includes_permission_mode_default() {
        let p = SubAgentPermissions::default();
        assert_eq!(p.permission_mode, PermissionMode::Default);
    }

    // ── #1185: additional test gaps ────────────────────────────────────────

    #[test]
    fn parse_yaml_unknown_permission_mode_variant_is_error() {
        // Unknown variant (e.g. "banana_mode") must fail with a parse error.
        let content = "---\nname: a\ndescription: b\npermissions:\n  permission_mode: banana_mode\n---\n\nbody\n";
        let err = SubAgentDef::parse(content).unwrap_err();
        assert!(matches!(err, SubAgentError::Parse { .. }));
    }

    #[test]
    fn parse_yaml_permission_mode_case_sensitive_camel_is_error() {
        // "DontAsk" (camelCase) must not parse — only snake_case is accepted.
        let content =
            "---\nname: a\ndescription: b\npermissions:\n  permission_mode: DontAsk\n---\n\nbody\n";
        let err = SubAgentDef::parse(content).unwrap_err();
        assert!(matches!(err, SubAgentError::Parse { .. }));
    }

    #[test]
    fn parse_yaml_explicit_empty_except_gives_empty_disallowed_tools() {
        let content = "---\nname: a\ndescription: b\ntools:\n  allow:\n    - shell\n  except: []\n---\n\nbody\n";
        let def = SubAgentDef::parse(content).unwrap();
        assert!(def.disallowed_tools.is_empty());
    }

    #[test]
    fn parse_yaml_disallowed_tools_with_deny_list_deny_wins() {
        // disallowed_tools (tools.except) blocks a tool even when DenyList base policy
        // would otherwise allow it (deny wins).
        let content = "---\nname: a\ndescription: b\ntools:\n  deny:\n    - dangerous\n  except:\n    - web\n---\n\nbody\n";
        let def = SubAgentDef::parse(content).unwrap();
        // base policy: DenyList blocks "dangerous", allows everything else
        assert!(matches!(def.tools, ToolPolicy::DenyList(ref v) if v == &["dangerous"]));
        // disallowed_tools: "web" is additionally blocked by except
        assert!(def.disallowed_tools.contains(&"web".to_owned()));
    }

    #[test]
    fn parse_toml_background_true_frontmatter() {
        // background: true via TOML (+++) frontmatter must parse correctly.
        let content = "+++\nname = \"bg-agent\"\ndescription = \"Runs in background\"\n[permissions]\nbackground = true\n+++\n\nSystem prompt.\n";
        let def = SubAgentDef::parse(content).unwrap();
        assert!(def.permissions.background);
        assert_eq!(def.name, "bg-agent");
    }

    #[test]
    fn parse_yaml_unknown_top_level_field_is_error() {
        // deny_unknown_fields on RawSubAgentDef: typos like "permisions:" must be rejected.
        let content = "---\nname: a\ndescription: b\npermisions:\n  max_turns: 5\n---\n\nbody\n";
        let err = SubAgentDef::parse(content).unwrap_err();
        assert!(matches!(err, SubAgentError::Parse { .. }));
    }
}
