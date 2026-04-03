// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

use super::error::SubAgentError;
use super::hooks::SubagentHooks;

pub use zeph_config::{MemoryScope, ModelSpec, PermissionMode, SkillFilter, ToolPolicy};

/// Validated agent name pattern: ASCII alphanumeric, hyphen, underscore.
/// Must start with alphanumeric, max 64 chars. Rejects unicode homoglyphs.
pub(super) static AGENT_NAME_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-zA-Z0-9][a-zA-Z0-9_-]{0,63}$").unwrap());

/// Returns true if the given name passes the agent name validation regex.
pub fn is_valid_agent_name(name: &str) -> bool {
    AGENT_NAME_RE.is_match(name)
}

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubAgentDef {
    pub name: String,
    pub description: String,
    pub model: Option<ModelSpec>,
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
    /// Persistent memory scope. When set, a memory directory is created at spawn time
    /// and `MEMORY.md` content is injected into the system prompt.
    pub memory: Option<MemoryScope>,
    /// Scope label and filename of the definition file (populated by `load` / `load_all`).
    ///
    /// Stored as `"<scope>/<filename>"` (e.g., `"project/my-agent.md"`).
    /// The full absolute path is intentionally not stored to avoid leaking local
    /// filesystem layout in diagnostics and `/agent list` output.
    #[serde(skip)]
    pub source: Option<String>,
    /// Full filesystem path of the definition file (populated by `load_with_boundary`).
    ///
    /// Used internally by edit/delete operations. Not included in diagnostics output.
    #[serde(skip)]
    pub file_path: Option<PathBuf>,
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

// ── Raw deserialization structs ───────────────────────────────────────────────
// These work for both YAML and TOML deserializers — only the deserializer call
// differs based on detected frontmatter format.

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSubAgentDef {
    name: String,
    description: String,
    model: Option<ModelSpec>,
    #[serde(default)]
    tools: RawToolPolicy,
    #[serde(default)]
    permissions: RawPermissions,
    #[serde(default)]
    skills: RawSkillFilter,
    #[serde(default)]
    hooks: SubagentHooks,
    #[serde(default)]
    memory: Option<MemoryScope>,
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
        // CRIT-01: unified name validation — ASCII-only, path-safe, max 64 chars.
        // Rejects unicode homoglyphs, full-width chars, path separators, and control chars.
        if !AGENT_NAME_RE.is_match(&raw.name) {
            return Err(SubAgentError::Invalid(format!(
                "name '{}' is invalid: must match ^[a-zA-Z0-9][a-zA-Z0-9_-]{{0,63}}$ \
                 (ASCII only, no spaces or special characters)",
                raw.name
            )));
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
            memory: raw.memory,
            system_prompt: body.trim().to_owned(),
            source: None,
            file_path: None,
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
        // Populate file_path for edit/delete operations (not used in diagnostics output).
        def.file_path = Some(canonical);

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

            // Compute boundary for symlink protection. CLI dirs are trusted (user-supplied,
            // already validated by the shell). All other dirs (project, user, extra) get a
            // canonical boundary check to reject symlinks that escape the allowed directory.
            let is_cli_dir = cli_agents.iter().any(|c| c == path);
            let boundary = if is_cli_dir {
                None
            } else {
                // Canonicalize the directory itself as the boundary.
                // This applies to project dir (.zeph/agents) as well — a symlink at
                // .zeph/agents pointing outside the project would be rejected.
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

// ── Serialization helpers ────────────────────────────────────────────────────

/// Mirror of `RawSubAgentDef` with correct `tools.except` nesting for round-trip
/// serialization. Avoids the IMP-CRIT-04 serde asymmetry on `SubAgentDef`.
#[derive(Serialize)]
struct WritableRawDef<'a> {
    name: &'a str,
    description: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<&'a ModelSpec>,
    #[serde(skip_serializing_if = "WritableToolPolicy::is_inherit_all")]
    tools: WritableToolPolicy<'a>,
    #[serde(skip_serializing_if = "WritablePermissions::is_default")]
    permissions: WritablePermissions<'a>,
    #[serde(skip_serializing_if = "SkillFilter::is_empty")]
    skills: &'a SkillFilter,
    #[serde(skip_serializing_if = "SubagentHooks::is_empty")]
    hooks: &'a SubagentHooks,
    #[serde(skip_serializing_if = "Option::is_none")]
    memory: Option<MemoryScope>,
}

#[derive(Serialize)]
struct WritableToolPolicy<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    allow: Option<&'a Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    deny: Option<&'a Vec<String>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    except: &'a Vec<String>,
}

impl<'a> WritableToolPolicy<'a> {
    fn from_def(policy: &'a ToolPolicy, except: &'a Vec<String>) -> Self {
        match policy {
            ToolPolicy::AllowList(v) => Self {
                allow: Some(v),
                deny: None,
                except,
            },
            ToolPolicy::DenyList(v) => Self {
                allow: None,
                deny: Some(v),
                except,
            },
            ToolPolicy::InheritAll => Self {
                allow: None,
                deny: None,
                except,
            },
        }
    }

    fn is_inherit_all(&self) -> bool {
        self.allow.is_none() && self.deny.is_none() && self.except.is_empty()
    }
}

#[derive(Serialize)]
struct WritablePermissions<'a> {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    secrets: &'a Vec<String>,
    max_turns: u32,
    background: bool,
    timeout_secs: u64,
    ttl_secs: u64,
    permission_mode: PermissionMode,
}

impl<'a> WritablePermissions<'a> {
    fn from_def(p: &'a SubAgentPermissions) -> Self {
        Self {
            secrets: &p.secrets,
            max_turns: p.max_turns,
            background: p.background,
            timeout_secs: p.timeout_secs,
            ttl_secs: p.ttl_secs,
            permission_mode: p.permission_mode,
        }
    }

    fn is_default(&self) -> bool {
        self.secrets.is_empty()
            && self.max_turns == default_max_turns()
            && !self.background
            && self.timeout_secs == default_timeout()
            && self.ttl_secs == default_ttl()
            && self.permission_mode == PermissionMode::Default
    }
}

impl SubAgentDef {
    /// Serialize the definition to YAML frontmatter + markdown body.
    ///
    /// Uses `WritableRawDef` (with correct `tools.except` nesting) to avoid the
    /// IMP-CRIT-04 serde asymmetry. The result can be re-parsed with `SubAgentDef::parse`.
    ///
    /// # Panics
    ///
    /// Panics if `serde_norway` serialization fails (should not happen for valid structs).
    #[must_use]
    pub fn serialize_to_markdown(&self) -> String {
        let tools = WritableToolPolicy::from_def(&self.tools, &self.disallowed_tools);
        let permissions = WritablePermissions::from_def(&self.permissions);

        let writable = WritableRawDef {
            name: &self.name,
            description: &self.description,
            model: self.model.as_ref(),
            tools,
            permissions,
            skills: &self.skills,
            hooks: &self.hooks,
            memory: self.memory,
        };

        let yaml = serde_norway::to_string(&writable).expect("serialization cannot fail");
        if self.system_prompt.is_empty() {
            format!("---\n{yaml}---\n")
        } else {
            format!("---\n{yaml}---\n\n{}\n", self.system_prompt)
        }
    }

    /// Write definition to `{dir}/{self.name}.md` atomically using temp+rename.
    ///
    /// Creates parent directories if needed. Uses `tempfile::NamedTempFile` in the same
    /// directory for automatic cleanup on failure.
    ///
    /// # Errors
    ///
    /// Returns [`SubAgentError::Invalid`] if the agent name fails validation (prevents path traversal).
    /// Returns [`SubAgentError::Io`] if directory creation, write, or rename fails.
    pub fn save_atomic(&self, dir: &Path) -> Result<PathBuf, SubAgentError> {
        if !AGENT_NAME_RE.is_match(&self.name) {
            return Err(SubAgentError::Invalid(format!(
                "name '{}' is invalid: must match ^[a-zA-Z0-9][a-zA-Z0-9_-]{{0,63}}$",
                self.name
            )));
        }
        std::fs::create_dir_all(dir).map_err(|e| SubAgentError::Io {
            path: dir.display().to_string(),
            reason: format!("cannot create directory: {e}"),
        })?;

        let content = self.serialize_to_markdown();
        let target = dir.join(format!("{}.md", self.name));

        let mut tmp = NamedTempFile::new_in(dir).map_err(|e| SubAgentError::Io {
            path: dir.display().to_string(),
            reason: format!("cannot create temp file: {e}"),
        })?;

        std::io::Write::write_all(&mut tmp, content.as_bytes()).map_err(|e| SubAgentError::Io {
            path: dir.display().to_string(),
            reason: format!("cannot write temp file: {e}"),
        })?;

        tmp.persist(&target).map_err(|e| SubAgentError::Io {
            path: target.display().to_string(),
            reason: format!("cannot rename temp file: {e}"),
        })?;

        Ok(target)
    }

    /// Delete a definition file from disk.
    ///
    /// # Errors
    ///
    /// Returns [`SubAgentError::Io`] if the file does not exist or cannot be removed.
    pub fn delete_file(path: &Path) -> Result<(), SubAgentError> {
        std::fs::remove_file(path).map_err(|e| SubAgentError::Io {
            path: path.display().to_string(),
            reason: e.to_string(),
        })
    }

    /// Create a minimal definition suitable for the create wizard.
    ///
    /// Sets sensible defaults: `InheritAll` tools, default permissions, empty system prompt.
    #[must_use]
    pub fn default_template(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            model: None,
            tools: ToolPolicy::InheritAll,
            disallowed_tools: Vec::new(),
            permissions: SubAgentPermissions::default(),
            skills: SkillFilter::default(),
            hooks: SubagentHooks::default(),
            memory: None,
            system_prompt: String::new(),
            source: None,
            file_path: None,
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    #![allow(clippy::cloned_ref_to_slice_refs)]

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
        assert_eq!(
            def.model,
            Some(ModelSpec::Named("claude-sonnet-4-20250514".to_owned()))
        );
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
        assert_eq!(
            def.model,
            Some(ModelSpec::Named("claude-sonnet-4-20250514".to_owned()))
        );
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

        let search_dirs = vec![dir1.path().to_path_buf(), dir2.path().to_path_buf()];
        let defs = SubAgentDef::load_all(&search_dirs).unwrap();

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
        let err = SubAgentDef::load_all_with_sources(
            std::slice::from_ref(&bad_path),
            std::slice::from_ref(&bad_path),
            None,
            &[],
        )
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

    // ── MemoryScope / memory field tests ────────────────────────────────────

    #[test]
    fn parse_yaml_memory_scope_project() {
        let content =
            "---\nname: reviewer\ndescription: A reviewer\nmemory: project\n---\n\nBody.\n";
        let def = SubAgentDef::parse(content).unwrap();
        assert_eq!(def.memory, Some(MemoryScope::Project));
    }

    #[test]
    fn parse_yaml_memory_scope_user() {
        let content = "---\nname: reviewer\ndescription: A reviewer\nmemory: user\n---\n\nBody.\n";
        let def = SubAgentDef::parse(content).unwrap();
        assert_eq!(def.memory, Some(MemoryScope::User));
    }

    #[test]
    fn parse_yaml_memory_scope_local() {
        let content = "---\nname: reviewer\ndescription: A reviewer\nmemory: local\n---\n\nBody.\n";
        let def = SubAgentDef::parse(content).unwrap();
        assert_eq!(def.memory, Some(MemoryScope::Local));
    }

    #[test]
    fn parse_yaml_memory_absent_gives_none() {
        let content = "---\nname: reviewer\ndescription: A reviewer\n---\n\nBody.\n";
        let def = SubAgentDef::parse(content).unwrap();
        assert!(def.memory.is_none());
    }

    #[test]
    fn parse_yaml_memory_invalid_value_is_error() {
        let content =
            "---\nname: reviewer\ndescription: A reviewer\nmemory: global\n---\n\nBody.\n";
        let err = SubAgentDef::parse(content).unwrap_err();
        assert!(matches!(err, SubAgentError::Parse { .. }));
    }

    #[test]
    fn memory_scope_serde_roundtrip() {
        for scope in [MemoryScope::User, MemoryScope::Project, MemoryScope::Local] {
            let json = serde_json::to_string(&scope).unwrap();
            let parsed: MemoryScope = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, scope);
        }
    }

    // ── Agent name validation tests (CRIT-01) ────────────────────────────────

    #[test]
    fn parse_yaml_name_with_unicode_is_invalid() {
        // Cyrillic 'а' (U+0430) looks like Latin 'a' but is rejected.
        let content = "---\nname: аgent\ndescription: b\n---\n\nbody\n";
        let err = SubAgentDef::parse(content).unwrap_err();
        assert!(matches!(err, SubAgentError::Invalid(_)));
    }

    #[test]
    fn parse_yaml_name_with_space_is_invalid() {
        let content = "---\nname: my agent\ndescription: b\n---\n\nbody\n";
        let err = SubAgentDef::parse(content).unwrap_err();
        assert!(matches!(err, SubAgentError::Invalid(_)));
    }

    #[test]
    fn parse_yaml_name_with_dot_is_invalid() {
        let content = "---\nname: my.agent\ndescription: b\n---\n\nbody\n";
        let err = SubAgentDef::parse(content).unwrap_err();
        assert!(matches!(err, SubAgentError::Invalid(_)));
    }

    #[test]
    fn parse_yaml_name_single_char_is_valid() {
        let content = "---\nname: a\ndescription: b\n---\n\nbody\n";
        let def = SubAgentDef::parse(content).unwrap();
        assert_eq!(def.name, "a");
    }

    #[test]
    fn parse_yaml_name_with_underscore_and_hyphen_is_valid() {
        let content = "---\nname: my_agent-v2\ndescription: b\n---\n\nbody\n";
        let def = SubAgentDef::parse(content).unwrap();
        assert_eq!(def.name, "my_agent-v2");
    }

    // ── Serialization / save / delete / template tests ────────────────────────

    #[test]
    fn default_template_valid() {
        let def = SubAgentDef::default_template("tester", "Runs tests");
        assert_eq!(def.name, "tester");
        assert_eq!(def.description, "Runs tests");
        assert!(def.model.is_none());
        assert!(matches!(def.tools, ToolPolicy::InheritAll));
        assert!(def.system_prompt.is_empty());
    }

    #[test]
    fn default_template_roundtrip() {
        let def = SubAgentDef::default_template("tester", "Runs tests");
        let markdown = def.serialize_to_markdown();
        let parsed = SubAgentDef::parse(&markdown).unwrap();
        assert_eq!(parsed.name, "tester");
        assert_eq!(parsed.description, "Runs tests");
    }

    #[test]
    fn serialize_minimal() {
        let def = SubAgentDef::default_template("bot", "A bot");
        let md = def.serialize_to_markdown();
        assert!(md.starts_with("---\n"));
        assert!(md.contains("name: bot"));
        assert!(md.contains("description: A bot"));
    }

    #[test]
    fn serialize_roundtrip() {
        let content = indoc! {"
            ---
            name: code-reviewer
            description: Reviews code changes for correctness and style
            model: claude-sonnet-4-20250514
            tools:
              allow:
                - shell
                - web_scrape
            permissions:
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
        let def = SubAgentDef::parse(content).unwrap();
        let serialized = def.serialize_to_markdown();
        let reparsed = SubAgentDef::parse(&serialized).unwrap();
        assert_eq!(reparsed.name, def.name);
        assert_eq!(reparsed.description, def.description);
        assert_eq!(reparsed.model, def.model);
        assert_eq!(reparsed.permissions.max_turns, def.permissions.max_turns);
        assert_eq!(
            reparsed.permissions.timeout_secs,
            def.permissions.timeout_secs
        );
        assert_eq!(reparsed.permissions.ttl_secs, def.permissions.ttl_secs);
        assert_eq!(reparsed.permissions.background, def.permissions.background);
        assert_eq!(
            reparsed.permissions.permission_mode,
            def.permissions.permission_mode
        );
        assert_eq!(reparsed.skills.include, def.skills.include);
        assert_eq!(reparsed.skills.exclude, def.skills.exclude);
        assert_eq!(reparsed.system_prompt, def.system_prompt);
        assert!(
            matches!(&reparsed.tools, ToolPolicy::AllowList(v) if v == &["shell", "web_scrape"])
        );
    }

    #[test]
    fn serialize_roundtrip_tools_except() {
        let content = indoc! {"
            ---
            name: auditor
            description: Security auditor
            tools:
              allow:
                - shell
              except:
                - shell_sudo
                - shell_rm
            ---

            Audit mode.
        "};
        let def = SubAgentDef::parse(content).unwrap();
        let serialized = def.serialize_to_markdown();
        let reparsed = SubAgentDef::parse(&serialized).unwrap();
        assert_eq!(reparsed.disallowed_tools, def.disallowed_tools);
        assert_eq!(reparsed.disallowed_tools, ["shell_sudo", "shell_rm"]);
        assert!(matches!(&reparsed.tools, ToolPolicy::AllowList(v) if v == &["shell"]));
    }

    #[test]
    fn serialize_all_fields() {
        let content = indoc! {"
            ---
            name: full-agent
            description: Full featured agent
            model: claude-opus-4-6
            tools:
              allow:
                - shell
              except:
                - shell_sudo
            permissions:
              max_turns: 5
              background: true
              timeout_secs: 120
              ttl_secs: 60
            skills:
              include:
                - \"git-*\"
            ---

            System prompt here.
        "};
        let def = SubAgentDef::parse(content).unwrap();
        let md = def.serialize_to_markdown();
        assert!(md.contains("model: claude-opus-4-6"));
        assert!(md.contains("except:"));
        assert!(md.contains("shell_sudo"));
        assert!(md.contains("background: true"));
        assert!(md.contains("System prompt here."));
    }

    #[test]
    fn save_atomic_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        let def = SubAgentDef::default_template("myagent", "A test agent");
        let path = def.save_atomic(dir.path()).unwrap();
        assert!(path.exists());
        assert_eq!(path.file_name().unwrap(), "myagent.md");
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("name: myagent"));
    }

    #[test]
    fn save_atomic_creates_parent_dirs() {
        let base = tempfile::tempdir().unwrap();
        let nested = base.path().join("a").join("b").join("c");
        let def = SubAgentDef::default_template("nested", "Nested dir test");
        let path = def.save_atomic(&nested).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn save_atomic_overwrites_existing() {
        let dir = tempfile::tempdir().unwrap();
        let def1 = SubAgentDef::default_template("agent", "First description");
        def1.save_atomic(dir.path()).unwrap();

        let def2 = SubAgentDef::default_template("agent", "Second description");
        def2.save_atomic(dir.path()).unwrap();

        let content = std::fs::read_to_string(dir.path().join("agent.md")).unwrap();
        assert!(content.contains("Second description"));
        assert!(!content.contains("First description"));
    }

    #[test]
    fn delete_file_removes() {
        let dir = tempfile::tempdir().unwrap();
        let def = SubAgentDef::default_template("todelete", "Will be deleted");
        let path = def.save_atomic(dir.path()).unwrap();
        assert!(path.exists());
        SubAgentDef::delete_file(&path).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn delete_file_nonexistent_errors() {
        let path = std::path::PathBuf::from("/tmp/does-not-exist-zeph-test.md");
        let result = SubAgentDef::delete_file(&path);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), SubAgentError::Io { .. }));
    }

    #[test]
    fn save_atomic_rejects_invalid_name() {
        let dir = tempfile::tempdir().unwrap();
        let mut def = SubAgentDef::default_template("valid-name", "desc");
        // Bypass default_template to inject an invalid name.
        def.name = "../../etc/cron.d/agent".to_owned();
        let result = def.save_atomic(dir.path());
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), SubAgentError::Invalid(_)));
    }

    #[test]
    fn is_valid_agent_name_accepts_valid() {
        assert!(super::is_valid_agent_name("reviewer"));
        assert!(super::is_valid_agent_name("code-reviewer"));
        assert!(super::is_valid_agent_name("code_reviewer"));
        assert!(super::is_valid_agent_name("a"));
        assert!(super::is_valid_agent_name("A1"));
    }

    #[test]
    fn is_valid_agent_name_rejects_invalid() {
        assert!(!super::is_valid_agent_name(""));
        assert!(!super::is_valid_agent_name("my agent"));
        assert!(!super::is_valid_agent_name("../../etc"));
        assert!(!super::is_valid_agent_name("-starts-with-dash"));
        assert!(!super::is_valid_agent_name("has.dot"));
    }
}
