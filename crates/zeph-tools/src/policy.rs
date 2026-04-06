// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Declarative policy compiler for tool call authorization.
//!
//! Evaluates TOML-based access-control rules before any tool executes.
//! Deny-wins semantics: deny rules checked first, then allow rules, then `default_effect`.

use std::path::{Path, PathBuf};

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::SkillTrustLevel;

// Max rules to prevent startup OOM from misconfigured policy files.
const MAX_RULES: usize = 256;
// Max regex pattern length in bytes.
const MAX_REGEX_LEN: usize = 1024;

/// Effect applied when a rule matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyEffect {
    Allow,
    Deny,
}

/// Default effect when no rule matches.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DefaultEffect {
    Allow,
    #[default]
    Deny,
}

fn default_deny() -> DefaultEffect {
    DefaultEffect::Deny
}

/// TOML-deserializable policy configuration.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct PolicyConfig {
    /// Whether to enforce policy rules. When false, all calls are allowed.
    #[serde(default)]
    pub enabled: bool,
    /// Fallback effect when no rule matches.
    #[serde(default = "default_deny")]
    pub default_effect: DefaultEffect,
    /// Inline policy rules.
    #[serde(default)]
    pub rules: Vec<PolicyRuleConfig>,
    /// Optional external policy file (TOML). When set, overrides inline rules.
    pub policy_file: Option<String>,
}

/// A single policy rule as read from TOML.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PolicyRuleConfig {
    pub effect: PolicyEffect,
    /// Glob pattern matching the tool id. Required.
    pub tool: String,
    /// Path globs matched against path-like params. Rule fires if ANY path matches.
    #[serde(default)]
    pub paths: Vec<String>,
    /// Env var names that must all be present in `PolicyContext.env`.
    #[serde(default)]
    pub env: Vec<String>,
    /// Minimum required trust level (rule fires only when context trust <= threshold).
    pub trust_level: Option<SkillTrustLevel>,
    /// Regex matched against individual string param values.
    pub args_match: Option<String>,
    /// Named capabilities associated with this rule (e.g., "fs:write", "net:external").
    /// Config-only field: capability matching is deferred until tools expose capability metadata.
    #[serde(default)]
    pub capabilities: Vec<String>,
}

/// Runtime context passed to `PolicyEnforcer::evaluate`.
#[derive(Debug, Clone)]
pub struct PolicyContext {
    pub trust_level: SkillTrustLevel,
    pub env: std::collections::HashMap<String, String>,
}

/// Result of a policy evaluation.
#[derive(Debug, Clone)]
pub enum PolicyDecision {
    Allow { trace: String },
    Deny { trace: String },
}

/// Errors that can occur when compiling a `PolicyConfig`.
#[derive(Debug, thiserror::Error)]
pub enum PolicyCompileError {
    #[error("invalid glob pattern in rule {index}: {source}")]
    InvalidGlob {
        index: usize,
        source: glob::PatternError,
    },

    #[error("invalid regex in rule {index}: {source}")]
    InvalidRegex { index: usize, source: regex::Error },

    #[error("regex pattern in rule {index} exceeds maximum length ({MAX_REGEX_LEN} bytes)")]
    RegexTooLong { index: usize },

    #[error("too many rules: {count} exceeds maximum of {MAX_RULES}")]
    TooManyRules { count: usize },

    #[error("failed to load policy file {path}: {source}")]
    FileLoad {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("policy file too large: {path}")]
    FileTooLarge { path: PathBuf },

    #[error("policy file escapes project root: {path}")]
    FileEscapesRoot { path: PathBuf },

    #[error("failed to parse policy file {path}: {source}")]
    FileParse {
        path: PathBuf,
        source: toml::de::Error,
    },
}

/// Pre-compiled rule for zero-cost evaluation on the hot path.
#[derive(Debug)]
struct CompiledRule {
    effect: PolicyEffect,
    tool_matcher: glob::Pattern,
    path_matchers: Vec<glob::Pattern>,
    env_required: Vec<String>,
    trust_threshold: Option<SkillTrustLevel>,
    args_regex: Option<Regex>,
    source_index: usize,
}

impl CompiledRule {
    /// Check whether this rule matches the given tool call and context.
    fn matches(
        &self,
        tool_name: &str,
        params: &serde_json::Map<String, serde_json::Value>,
        context: &PolicyContext,
    ) -> bool {
        // Tool name glob match.
        if !self.tool_matcher.matches(tool_name) {
            return false;
        }

        // Path condition: any extracted path must match any path pattern.
        if !self.path_matchers.is_empty() {
            let paths = extract_paths(params);
            let any_path_matches = paths.iter().any(|p| {
                let normalized = crate::file::normalize_path(Path::new(p))
                    .to_string_lossy()
                    .into_owned();
                self.path_matchers
                    .iter()
                    .any(|pat| pat.matches(&normalized))
            });
            if !any_path_matches {
                return false;
            }
        }

        // Env condition: all required env vars must be present.
        if !self
            .env_required
            .iter()
            .all(|k| context.env.contains_key(k.as_str()))
        {
            return false;
        }

        // Trust level condition: context trust must be <= threshold (more trusted).
        if self
            .trust_threshold
            .is_some_and(|t| context.trust_level.severity() > t.severity())
        {
            return false;
        }

        // Args regex: matched against individual string param values.
        if let Some(re) = &self.args_regex {
            let any_matches = params.values().any(|v| {
                if let Some(s) = v.as_str() {
                    re.is_match(s)
                } else {
                    false
                }
            });
            if !any_matches {
                return false;
            }
        }

        true
    }
}

/// Deterministic policy evaluator. Constructed once from config, immutable thereafter.
#[derive(Debug)]
pub struct PolicyEnforcer {
    rules: Vec<CompiledRule>,
    default_effect: DefaultEffect,
}

impl PolicyEnforcer {
    /// Compile a `PolicyConfig` into a `PolicyEnforcer`.
    ///
    /// # Errors
    ///
    /// Returns `PolicyCompileError` if any glob or regex in the config is invalid,
    /// or if the policy file cannot be loaded or parsed.
    pub fn compile(config: &PolicyConfig) -> Result<Self, PolicyCompileError> {
        let rule_configs: Vec<PolicyRuleConfig> = if let Some(path) = &config.policy_file {
            load_policy_file(Path::new(path))?
        } else {
            config.rules.clone()
        };

        if rule_configs.len() > MAX_RULES {
            return Err(PolicyCompileError::TooManyRules {
                count: rule_configs.len(),
            });
        }

        let mut rules = Vec::with_capacity(rule_configs.len());
        for (i, rule) in rule_configs.iter().enumerate() {
            // Normalize tool name: lowercase, strip whitespace, then resolve aliases.
            let normalized_tool =
                resolve_tool_alias(rule.tool.trim().to_lowercase().as_str()).to_owned();

            let tool_matcher = glob::Pattern::new(&normalized_tool)
                .map_err(|source| PolicyCompileError::InvalidGlob { index: i, source })?;

            let path_matchers = rule
                .paths
                .iter()
                .map(|p| {
                    glob::Pattern::new(p)
                        .map_err(|source| PolicyCompileError::InvalidGlob { index: i, source })
                })
                .collect::<Result<Vec<_>, _>>()?;

            let args_regex = if let Some(pattern) = &rule.args_match {
                if pattern.len() > MAX_REGEX_LEN {
                    return Err(PolicyCompileError::RegexTooLong { index: i });
                }
                Some(
                    Regex::new(pattern)
                        .map_err(|source| PolicyCompileError::InvalidRegex { index: i, source })?,
                )
            } else {
                None
            };

            rules.push(CompiledRule {
                effect: rule.effect,
                tool_matcher,
                path_matchers,
                env_required: rule.env.clone(),
                trust_threshold: rule.trust_level,
                args_regex,
                source_index: i,
            });
        }

        Ok(Self {
            rules,
            default_effect: config.default_effect,
        })
    }

    /// Return the total number of compiled rules (inline + file-loaded).
    #[must_use]
    pub fn rule_count(&self) -> usize {
        self.rules.len()
    }

    /// Evaluate a tool call against the compiled policy rules.
    ///
    /// Returns `PolicyDecision::Deny` when any deny rule matches.
    /// Returns `PolicyDecision::Allow` when any `allow`/`allow_if` rule matches.
    /// Falls back to `default_effect` when no rule matches.
    ///
    /// Tool name is normalized (lowercase, trimmed) before matching.
    #[must_use]
    pub fn evaluate(
        &self,
        tool_name: &str,
        params: &serde_json::Map<String, serde_json::Value>,
        context: &PolicyContext,
    ) -> PolicyDecision {
        let normalized = resolve_tool_alias(tool_name.trim().to_lowercase().as_str()).to_owned();

        // Deny-wins: check all deny rules first.
        for rule in &self.rules {
            if rule.effect == PolicyEffect::Deny && rule.matches(&normalized, params, context) {
                let trace = format!(
                    "rule[{}] deny: tool={} matched {}",
                    rule.source_index, tool_name, rule.tool_matcher
                );
                return PolicyDecision::Deny { trace };
            }
        }

        // Then check allow rules.
        for rule in &self.rules {
            if rule.effect != PolicyEffect::Deny && rule.matches(&normalized, params, context) {
                let trace = format!(
                    "rule[{}] allow: tool={} matched {}",
                    rule.source_index, tool_name, rule.tool_matcher
                );
                return PolicyDecision::Allow { trace };
            }
        }

        // Default effect.
        match self.default_effect {
            DefaultEffect::Allow => PolicyDecision::Allow {
                trace: "default: allow (no matching rules)".to_owned(),
            },
            DefaultEffect::Deny => PolicyDecision::Deny {
                trace: "default: deny (no matching rules)".to_owned(),
            },
        }
    }
}

/// Resolve tool name aliases so policy rules are tool-id-agnostic.
///
/// `ShellExecutor` registers as `tool_id="bash"` but users naturally write `tool="shell"`.
/// Both forms (and `"sh"`) are normalized to `"shell"` before matching.
fn resolve_tool_alias(name: &str) -> &str {
    match name {
        "bash" | "sh" => "shell",
        other => other,
    }
}

/// Load and parse a `PolicyConfig::rules` from an external TOML file.
///
/// # Errors
///
/// Returns an error if the file cannot be read, parsed, or if its canonical path
/// escapes the process working directory (symlink boundary check).
fn load_policy_file(path: &Path) -> Result<Vec<PolicyRuleConfig>, PolicyCompileError> {
    // 256 KiB limit, same as instruction files.
    const MAX_POLICY_FILE_BYTES: u64 = 256 * 1024;

    #[derive(Deserialize)]
    struct PolicyFile {
        #[serde(default)]
        rules: Vec<PolicyRuleConfig>,
    }

    // Canonicalize first to resolve symlinks before opening — eliminates TOCTOU race.
    let canonical = std::fs::canonicalize(path).map_err(|source| PolicyCompileError::FileLoad {
        path: path.to_owned(),
        source,
    })?;

    // Symlink boundary check: canonical path must stay within the process working directory.
    let canonical_base = std::env::current_dir()
        .and_then(std::fs::canonicalize)
        .map_err(|source| PolicyCompileError::FileLoad {
            path: path.to_owned(),
            source,
        })?;

    if !canonical.starts_with(&canonical_base) {
        tracing::warn!(
            path = %canonical.display(),
            "policy file escapes project root, rejecting"
        );
        return Err(PolicyCompileError::FileEscapesRoot {
            path: path.to_owned(),
        });
    }

    // Use the canonical path for all subsequent I/O — no TOCTOU window for symlink swap.
    let meta = std::fs::metadata(&canonical).map_err(|source| PolicyCompileError::FileLoad {
        path: path.to_owned(),
        source,
    })?;
    if meta.len() > MAX_POLICY_FILE_BYTES {
        return Err(PolicyCompileError::FileTooLarge {
            path: path.to_owned(),
        });
    }

    let content =
        std::fs::read_to_string(&canonical).map_err(|source| PolicyCompileError::FileLoad {
            path: path.to_owned(),
            source,
        })?;

    let parsed: PolicyFile =
        toml::from_str(&content).map_err(|source| PolicyCompileError::FileParse {
            path: path.to_owned(),
            source,
        })?;

    Ok(parsed.rules)
}

/// Extract path-like string values from tool params.
///
/// Checks well-known path param keys, and for `command` params extracts
/// absolute paths via a simple regex heuristic.
fn extract_paths(params: &serde_json::Map<String, serde_json::Value>) -> Vec<String> {
    static ABS_PATH_RE: std::sync::LazyLock<Regex> =
        std::sync::LazyLock::new(|| Regex::new(r"(/[^\s;|&<>]+)").expect("valid regex"));

    let mut paths = Vec::new();

    for key in &["file_path", "path", "uri", "url", "query"] {
        if let Some(v) = params.get(*key).and_then(|v| v.as_str()) {
            paths.push(v.to_owned());
        }
    }

    // For `command` params, extract embedded absolute paths.
    if let Some(cmd) = params.get("command").and_then(|v| v.as_str()) {
        for cap in ABS_PATH_RE.captures_iter(cmd) {
            if let Some(m) = cap.get(1) {
                paths.push(m.as_str().to_owned());
            }
        }
    }

    paths
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn make_context(trust: SkillTrustLevel) -> PolicyContext {
        PolicyContext {
            trust_level: trust,
            env: HashMap::new(),
        }
    }

    fn make_params(key: &str, value: &str) -> serde_json::Map<String, serde_json::Value> {
        let mut m = serde_json::Map::new();
        m.insert(key.to_owned(), serde_json::Value::String(value.to_owned()));
        m
    }

    fn empty_params() -> serde_json::Map<String, serde_json::Value> {
        serde_json::Map::new()
    }

    // ── CRIT-01: path traversal normalization ─────────────────────────────────

    #[test]
    fn test_path_normalization() {
        // deny shell /etc/* -> call with /tmp/../etc/passwd -> Deny
        let config = PolicyConfig {
            enabled: true,
            default_effect: DefaultEffect::Allow,
            rules: vec![PolicyRuleConfig {
                effect: PolicyEffect::Deny,
                tool: "shell".to_owned(),
                paths: vec!["/etc/*".to_owned()],
                env: vec![],
                trust_level: None,
                args_match: None,
                capabilities: vec![],
            }],
            policy_file: None,
        };
        let enforcer = PolicyEnforcer::compile(&config).unwrap();
        let params = make_params("file_path", "/tmp/../etc/passwd");
        let ctx = make_context(SkillTrustLevel::Trusted);
        assert!(
            matches!(
                enforcer.evaluate("shell", &params, &ctx),
                PolicyDecision::Deny { .. }
            ),
            "path traversal must be caught after normalization"
        );
    }

    #[test]
    fn test_path_normalization_dot_segments() {
        let config = PolicyConfig {
            enabled: true,
            default_effect: DefaultEffect::Allow,
            rules: vec![PolicyRuleConfig {
                effect: PolicyEffect::Deny,
                tool: "shell".to_owned(),
                paths: vec!["/etc/*".to_owned()],
                env: vec![],
                trust_level: None,
                args_match: None,
                capabilities: vec![],
            }],
            policy_file: None,
        };
        let enforcer = PolicyEnforcer::compile(&config).unwrap();
        let params = make_params("file_path", "/etc/./shadow");
        let ctx = make_context(SkillTrustLevel::Trusted);
        assert!(matches!(
            enforcer.evaluate("shell", &params, &ctx),
            PolicyDecision::Deny { .. }
        ));
    }

    // ── CRIT-02: tool name normalization ──────────────────────────────────────

    #[test]
    fn test_tool_name_normalization() {
        // deny "Shell" (uppercase in rule) -> call with "shell" -> Deny
        let config = PolicyConfig {
            enabled: true,
            default_effect: DefaultEffect::Allow,
            rules: vec![PolicyRuleConfig {
                effect: PolicyEffect::Deny,
                tool: "Shell".to_owned(),
                paths: vec![],
                env: vec![],
                trust_level: None,
                args_match: None,
                capabilities: vec![],
            }],
            policy_file: None,
        };
        let enforcer = PolicyEnforcer::compile(&config).unwrap();
        let ctx = make_context(SkillTrustLevel::Trusted);
        assert!(matches!(
            enforcer.evaluate("shell", &empty_params(), &ctx),
            PolicyDecision::Deny { .. }
        ));
        // Also uppercase call -> normalized tool name -> Deny
        assert!(matches!(
            enforcer.evaluate("SHELL", &empty_params(), &ctx),
            PolicyDecision::Deny { .. }
        ));
    }

    // ── Deny-wins semantics ───────────────────────────────────────────────────

    #[test]
    fn test_deny_wins() {
        // allow shell /tmp/*, deny shell /tmp/secret.sh -> call with /tmp/secret.sh -> Deny
        let config = PolicyConfig {
            enabled: true,
            default_effect: DefaultEffect::Allow,
            rules: vec![
                PolicyRuleConfig {
                    effect: PolicyEffect::Allow,
                    tool: "shell".to_owned(),
                    paths: vec!["/tmp/*".to_owned()],
                    env: vec![],
                    trust_level: None,
                    args_match: None,
                    capabilities: vec![],
                },
                PolicyRuleConfig {
                    effect: PolicyEffect::Deny,
                    tool: "shell".to_owned(),
                    paths: vec!["/tmp/secret.sh".to_owned()],
                    env: vec![],
                    trust_level: None,
                    args_match: None,
                    capabilities: vec![],
                },
            ],
            policy_file: None,
        };
        let enforcer = PolicyEnforcer::compile(&config).unwrap();
        let params = make_params("file_path", "/tmp/secret.sh");
        let ctx = make_context(SkillTrustLevel::Trusted);
        assert!(
            matches!(
                enforcer.evaluate("shell", &params, &ctx),
                PolicyDecision::Deny { .. }
            ),
            "deny must win over allow for the same path"
        );
    }

    // GAP-02: deny-wins must hold regardless of insertion order.
    #[test]
    fn deny_wins_deny_first() {
        // Deny rule at index 0, allow rule at index 1.
        let config = PolicyConfig {
            enabled: true,
            default_effect: DefaultEffect::Allow,
            rules: vec![
                PolicyRuleConfig {
                    effect: PolicyEffect::Deny,
                    tool: "shell".to_owned(),
                    paths: vec!["/etc/*".to_owned()],
                    env: vec![],
                    trust_level: None,
                    args_match: None,
                    capabilities: vec![],
                },
                PolicyRuleConfig {
                    effect: PolicyEffect::Allow,
                    tool: "shell".to_owned(),
                    paths: vec!["/etc/*".to_owned()],
                    env: vec![],
                    trust_level: None,
                    args_match: None,
                    capabilities: vec![],
                },
            ],
            policy_file: None,
        };
        let enforcer = PolicyEnforcer::compile(&config).unwrap();
        let params = make_params("file_path", "/etc/passwd");
        let ctx = make_context(SkillTrustLevel::Trusted);
        assert!(
            matches!(
                enforcer.evaluate("shell", &params, &ctx),
                PolicyDecision::Deny { .. }
            ),
            "deny must win when deny rule is first"
        );
    }

    #[test]
    fn deny_wins_deny_last() {
        // Allow rule at index 0, deny rule at index 1 (last).
        let config = PolicyConfig {
            enabled: true,
            default_effect: DefaultEffect::Allow,
            rules: vec![
                PolicyRuleConfig {
                    effect: PolicyEffect::Allow,
                    tool: "shell".to_owned(),
                    paths: vec!["/etc/*".to_owned()],
                    env: vec![],
                    trust_level: None,
                    args_match: None,
                    capabilities: vec![],
                },
                PolicyRuleConfig {
                    effect: PolicyEffect::Deny,
                    tool: "shell".to_owned(),
                    paths: vec!["/etc/*".to_owned()],
                    env: vec![],
                    trust_level: None,
                    args_match: None,
                    capabilities: vec![],
                },
            ],
            policy_file: None,
        };
        let enforcer = PolicyEnforcer::compile(&config).unwrap();
        let params = make_params("file_path", "/etc/passwd");
        let ctx = make_context(SkillTrustLevel::Trusted);
        assert!(
            matches!(
                enforcer.evaluate("shell", &params, &ctx),
                PolicyDecision::Deny { .. }
            ),
            "deny must win even when deny rule is last"
        );
    }

    // ── Default effects ───────────────────────────────────────────────────────

    #[test]
    fn test_default_deny() {
        let config = PolicyConfig {
            enabled: true,
            default_effect: DefaultEffect::Deny,
            rules: vec![],
            policy_file: None,
        };
        let enforcer = PolicyEnforcer::compile(&config).unwrap();
        let ctx = make_context(SkillTrustLevel::Trusted);
        assert!(matches!(
            enforcer.evaluate("bash", &empty_params(), &ctx),
            PolicyDecision::Deny { .. }
        ));
    }

    #[test]
    fn test_default_allow() {
        let config = PolicyConfig {
            enabled: true,
            default_effect: DefaultEffect::Allow,
            rules: vec![],
            policy_file: None,
        };
        let enforcer = PolicyEnforcer::compile(&config).unwrap();
        let ctx = make_context(SkillTrustLevel::Trusted);
        assert!(matches!(
            enforcer.evaluate("bash", &empty_params(), &ctx),
            PolicyDecision::Allow { .. }
        ));
    }

    // ── Trust level condition ─────────────────────────────────────────────────

    #[test]
    fn test_trust_level_condition() {
        // allow shell trust_level=verified -> Trusted (severity 0 <= 1) -> Allow
        //                                  -> Quarantined (severity 2 > 1) -> default deny
        let config = PolicyConfig {
            enabled: true,
            default_effect: DefaultEffect::Deny,
            rules: vec![PolicyRuleConfig {
                effect: PolicyEffect::Allow,
                tool: "shell".to_owned(),
                paths: vec![],
                env: vec![],
                trust_level: Some(SkillTrustLevel::Verified),
                args_match: None,
                capabilities: vec![],
            }],
            policy_file: None,
        };
        let enforcer = PolicyEnforcer::compile(&config).unwrap();

        let trusted_ctx = make_context(SkillTrustLevel::Trusted);
        assert!(
            matches!(
                enforcer.evaluate("shell", &empty_params(), &trusted_ctx),
                PolicyDecision::Allow { .. }
            ),
            "Trusted (severity 0) <= Verified threshold (severity 1) -> Allow"
        );

        let quarantined_ctx = make_context(SkillTrustLevel::Quarantined);
        assert!(
            matches!(
                enforcer.evaluate("shell", &empty_params(), &quarantined_ctx),
                PolicyDecision::Deny { .. }
            ),
            "Quarantined (severity 2) > Verified threshold (severity 1) -> falls through to default deny"
        );
    }

    // ── Max rules limit ───────────────────────────────────────────────────────

    #[test]
    fn test_too_many_rules_rejected() {
        let rules: Vec<PolicyRuleConfig> = (0..=MAX_RULES)
            .map(|i| PolicyRuleConfig {
                effect: PolicyEffect::Allow,
                tool: format!("tool_{i}"),
                paths: vec![],
                env: vec![],
                trust_level: None,
                args_match: None,
                capabilities: vec![],
            })
            .collect();
        let config = PolicyConfig {
            enabled: true,
            default_effect: DefaultEffect::Deny,
            rules,
            policy_file: None,
        };
        assert!(matches!(
            PolicyEnforcer::compile(&config),
            Err(PolicyCompileError::TooManyRules { .. })
        ));
    }

    #[test]
    fn deep_dotdot_traversal_blocked_by_deny_rule() {
        // GAP-01 integration: deny /etc/* must catch a deep .. traversal.
        let config = PolicyConfig {
            enabled: true,
            default_effect: DefaultEffect::Allow,
            rules: vec![PolicyRuleConfig {
                effect: PolicyEffect::Deny,
                tool: "shell".to_owned(),
                paths: vec!["/etc/*".to_owned()],
                env: vec![],
                trust_level: None,
                args_match: None,
                capabilities: vec![],
            }],
            policy_file: None,
        };
        let enforcer = PolicyEnforcer::compile(&config).unwrap();
        let params = make_params("file_path", "/a/b/c/d/../../../../../../etc/passwd");
        let ctx = make_context(SkillTrustLevel::Trusted);
        assert!(
            matches!(
                enforcer.evaluate("shell", &params, &ctx),
                PolicyDecision::Deny { .. }
            ),
            "deep .. chain traversal to /etc/passwd must be caught"
        );
    }

    // ── args_match on individual values ──────────────────────────────────────

    #[test]
    fn test_args_match_matches_param_value() {
        let config = PolicyConfig {
            enabled: true,
            default_effect: DefaultEffect::Allow,
            rules: vec![PolicyRuleConfig {
                effect: PolicyEffect::Deny,
                tool: "bash".to_owned(),
                paths: vec![],
                env: vec![],
                trust_level: None,
                args_match: Some(".*sudo.*".to_owned()),
                capabilities: vec![],
            }],
            policy_file: None,
        };
        let enforcer = PolicyEnforcer::compile(&config).unwrap();
        let ctx = make_context(SkillTrustLevel::Trusted);

        let params = make_params("command", "sudo rm -rf /");
        assert!(matches!(
            enforcer.evaluate("bash", &params, &ctx),
            PolicyDecision::Deny { .. }
        ));

        let safe_params = make_params("command", "echo hello");
        assert!(matches!(
            enforcer.evaluate("bash", &safe_params, &ctx),
            PolicyDecision::Allow { .. }
        ));
    }

    // ── TOML round-trip ───────────────────────────────────────────────────────

    #[test]
    fn policy_config_toml_round_trip() {
        let toml_str = r#"
            enabled = true
            default_effect = "deny"

            [[rules]]
            effect = "deny"
            tool = "shell"
            paths = ["/etc/*"]

            [[rules]]
            effect = "allow"
            tool = "shell"
            paths = ["/tmp/*"]
            trust_level = "verified"
        "#;
        let config: PolicyConfig = toml::from_str(toml_str).unwrap();
        assert!(config.enabled);
        assert_eq!(config.default_effect, DefaultEffect::Deny);
        assert_eq!(config.rules.len(), 2);
        assert_eq!(config.rules[0].effect, PolicyEffect::Deny);
        assert_eq!(config.rules[0].paths[0], "/etc/*");
        assert_eq!(config.rules[1].trust_level, Some(SkillTrustLevel::Verified));
    }

    #[test]
    fn policy_config_default_is_disabled_deny() {
        let config = PolicyConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.default_effect, DefaultEffect::Deny);
        assert!(config.rules.is_empty());
    }

    // ── load_policy_file security ─────────────────────────────────────────────

    #[test]
    fn policy_file_loaded_from_cwd_subdir() {
        let dir = tempfile::tempdir().unwrap();
        // Change into the temp dir so the boundary check passes.
        let original_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();

        let policy_path = dir.path().join("policy.toml");
        std::fs::write(
            &policy_path,
            r#"[[rules]]
effect = "deny"
tool = "shell"
"#,
        )
        .unwrap();

        let config = PolicyConfig {
            enabled: true,
            default_effect: DefaultEffect::Allow,
            rules: vec![],
            policy_file: Some(policy_path.to_string_lossy().into_owned()),
        };
        let result = PolicyEnforcer::compile(&config);
        std::env::set_current_dir(&original_cwd).unwrap();
        assert!(result.is_ok(), "policy file within cwd must be accepted");
    }

    #[cfg(unix)]
    #[test]
    fn policy_file_symlink_escaping_project_root_is_rejected() {
        use std::os::unix::fs::symlink;

        let outside = tempfile::tempdir().unwrap();
        let inside = tempfile::tempdir().unwrap();

        std::fs::write(
            outside.path().join("outside.toml"),
            "[[rules]]\neffect = \"deny\"\ntool = \"*\"\n",
        )
        .unwrap();

        // Symlink inside the project dir pointing to a file outside.
        let link = inside.path().join("evil.toml");
        symlink(outside.path().join("outside.toml"), &link).unwrap();

        let original_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(inside.path()).unwrap();

        let config = PolicyConfig {
            enabled: true,
            default_effect: DefaultEffect::Allow,
            rules: vec![],
            policy_file: Some(link.to_string_lossy().into_owned()),
        };
        let result = PolicyEnforcer::compile(&config);
        std::env::set_current_dir(&original_cwd).unwrap();

        assert!(
            matches!(result, Err(PolicyCompileError::FileEscapesRoot { .. })),
            "symlink escaping project root must be rejected"
        );
    }

    // ── Tool alias resolution (#1877) ─────────────────────────────────────────

    // Rule uses "shell", runtime tool_id is "bash" — the core bug case.
    #[test]
    fn alias_shell_rule_matches_bash_tool_id() {
        let config = PolicyConfig {
            enabled: true,
            default_effect: DefaultEffect::Allow,
            rules: vec![PolicyRuleConfig {
                effect: PolicyEffect::Deny,
                tool: "shell".to_owned(),
                paths: vec![],
                env: vec![],
                trust_level: None,
                args_match: None,
                capabilities: vec![],
            }],
            policy_file: None,
        };
        let enforcer = PolicyEnforcer::compile(&config).unwrap();
        let ctx = make_context(SkillTrustLevel::Trusted);
        assert!(
            matches!(
                enforcer.evaluate("bash", &empty_params(), &ctx),
                PolicyDecision::Deny { .. }
            ),
            "rule tool='shell' must match runtime tool_id='bash' via alias"
        );
    }

    // Rule uses "bash" — must still work (no regression).
    #[test]
    fn alias_bash_rule_matches_bash_tool_id() {
        let config = PolicyConfig {
            enabled: true,
            default_effect: DefaultEffect::Allow,
            rules: vec![PolicyRuleConfig {
                effect: PolicyEffect::Deny,
                tool: "bash".to_owned(),
                paths: vec![],
                env: vec![],
                trust_level: None,
                args_match: None,
                capabilities: vec![],
            }],
            policy_file: None,
        };
        let enforcer = PolicyEnforcer::compile(&config).unwrap();
        let ctx = make_context(SkillTrustLevel::Trusted);
        assert!(
            matches!(
                enforcer.evaluate("bash", &empty_params(), &ctx),
                PolicyDecision::Deny { .. }
            ),
            "rule tool='bash' must still match runtime tool_id='bash'"
        );
    }

    // Rule uses "sh" — must also match "bash" via alias.
    #[test]
    fn alias_sh_rule_matches_bash_tool_id() {
        let config = PolicyConfig {
            enabled: true,
            default_effect: DefaultEffect::Allow,
            rules: vec![PolicyRuleConfig {
                effect: PolicyEffect::Deny,
                tool: "sh".to_owned(),
                paths: vec![],
                env: vec![],
                trust_level: None,
                args_match: None,
                capabilities: vec![],
            }],
            policy_file: None,
        };
        let enforcer = PolicyEnforcer::compile(&config).unwrap();
        let ctx = make_context(SkillTrustLevel::Trusted);
        assert!(
            matches!(
                enforcer.evaluate("bash", &empty_params(), &ctx),
                PolicyDecision::Deny { .. }
            ),
            "rule tool='sh' must match runtime tool_id='bash' via alias"
        );
    }

    // ── MAX_RULES boundary ────────────────────────────────────────────────────

    // GAP-04: exactly MAX_RULES (256) rules must compile without error.
    #[test]
    fn max_rules_exactly_256_compiles() {
        let rules: Vec<PolicyRuleConfig> = (0..MAX_RULES)
            .map(|i| PolicyRuleConfig {
                effect: PolicyEffect::Allow,
                tool: format!("tool_{i}"),
                paths: vec![],
                env: vec![],
                trust_level: None,
                args_match: None,
                capabilities: vec![],
            })
            .collect();
        let config = PolicyConfig {
            enabled: true,
            default_effect: DefaultEffect::Deny,
            rules,
            policy_file: None,
        };
        assert!(
            PolicyEnforcer::compile(&config).is_ok(),
            "exactly {MAX_RULES} rules must compile successfully"
        );
    }

    // ── policy_file external TOML loading ─────────────────────────────────────

    // GAP-03a: happy path — file with a deny rule is loaded and evaluated correctly.
    //
    // The file must reside within the process cwd (boundary check in load_policy_file).
    // We create a tempdir inside the cwd so canonicalization passes without changing
    // global process state.
    #[test]
    fn policy_file_happy_path() {
        let cwd = std::env::current_dir().unwrap();
        let dir = tempfile::tempdir_in(&cwd).unwrap();
        let policy_path = dir.path().join("policy.toml");
        std::fs::write(
            &policy_path,
            "[[rules]]\neffect = \"deny\"\ntool = \"shell\"\npaths = [\"/etc/*\"]\n",
        )
        .unwrap();
        let config = PolicyConfig {
            enabled: true,
            default_effect: DefaultEffect::Allow,
            rules: vec![],
            policy_file: Some(policy_path.to_string_lossy().into_owned()),
        };
        let enforcer = PolicyEnforcer::compile(&config).unwrap();
        let params = make_params("file_path", "/etc/passwd");
        let ctx = make_context(SkillTrustLevel::Trusted);
        assert!(
            matches!(
                enforcer.evaluate("shell", &params, &ctx),
                PolicyDecision::Deny { .. }
            ),
            "deny rule loaded from file must block the matching call"
        );
    }

    // GAP-03b: FileTooLarge — file exceeding 256 KiB must be rejected.
    #[test]
    fn policy_file_too_large() {
        let cwd = std::env::current_dir().unwrap();
        let dir = tempfile::tempdir_in(&cwd).unwrap();
        let policy_path = dir.path().join("big.toml");
        std::fs::write(&policy_path, vec![b'x'; 256 * 1024 + 1]).unwrap();
        let config = PolicyConfig {
            enabled: true,
            default_effect: DefaultEffect::Allow,
            rules: vec![],
            policy_file: Some(policy_path.to_string_lossy().into_owned()),
        };
        assert!(
            matches!(
                PolicyEnforcer::compile(&config),
                Err(PolicyCompileError::FileTooLarge { .. })
            ),
            "file exceeding 256 KiB must return FileTooLarge"
        );
    }

    // GAP-03c: FileLoad — nonexistent path must return FileLoad error.
    // A nonexistent path fails at the canonicalize() call → FileLoad.
    #[test]
    fn policy_file_load_error() {
        let config = PolicyConfig {
            enabled: true,
            default_effect: DefaultEffect::Allow,
            rules: vec![],
            policy_file: Some("/tmp/__zeph_no_such_policy_file__.toml".to_owned()),
        };
        assert!(
            matches!(
                PolicyEnforcer::compile(&config),
                Err(PolicyCompileError::FileLoad { .. })
            ),
            "nonexistent policy file must return FileLoad"
        );
    }

    // GAP-03d: FileParse — malformed TOML must return FileParse error.
    #[test]
    fn policy_file_parse_error() {
        let cwd = std::env::current_dir().unwrap();
        let dir = tempfile::tempdir_in(&cwd).unwrap();
        let policy_path = dir.path().join("bad.toml");
        std::fs::write(&policy_path, "not valid toml = = =\n[[[\n").unwrap();
        let config = PolicyConfig {
            enabled: true,
            default_effect: DefaultEffect::Allow,
            rules: vec![],
            policy_file: Some(policy_path.to_string_lossy().into_owned()),
        };
        assert!(
            matches!(
                PolicyEnforcer::compile(&config),
                Err(PolicyCompileError::FileParse { .. })
            ),
            "malformed TOML must return FileParse"
        );
    }

    // Unknown tool names are not aliased.
    #[test]
    fn alias_unknown_tool_unaffected() {
        let config = PolicyConfig {
            enabled: true,
            default_effect: DefaultEffect::Allow,
            rules: vec![PolicyRuleConfig {
                effect: PolicyEffect::Deny,
                tool: "shell".to_owned(),
                paths: vec![],
                env: vec![],
                trust_level: None,
                args_match: None,
                capabilities: vec![],
            }],
            policy_file: None,
        };
        let enforcer = PolicyEnforcer::compile(&config).unwrap();
        let ctx = make_context(SkillTrustLevel::Trusted);
        // "web_scrape" is not an alias for anything — must not be denied by shell rule.
        assert!(
            matches!(
                enforcer.evaluate("web_scrape", &empty_params(), &ctx),
                PolicyDecision::Allow { .. }
            ),
            "unknown tool names must not be affected by alias resolution"
        );
    }
}
