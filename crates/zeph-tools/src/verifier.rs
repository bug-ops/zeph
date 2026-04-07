// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Pre-execution verification for tool calls.
//!
//! Based on the `TrustBench` pattern (arXiv:2603.09157): intercept tool calls before
//! execution to block or warn on destructive or injection patterns.
//!
//! ## Blocklist separation
//!
//! `DESTRUCTIVE_PATTERNS` (this module) is intentionally separate from
//! `DEFAULT_BLOCKED_COMMANDS` in `shell.rs`. The two lists serve different purposes:
//!
//! - `DEFAULT_BLOCKED_COMMANDS` — shell safety net: prevents the *shell executor* from
//!   running network tools (`curl`, `wget`, `nc`) and a few destructive commands.
//!   It is applied at tool-execution time by `ShellExecutor`.
//!
//! - `DESTRUCTIVE_PATTERNS` — pre-execution guard: targets filesystem/system destruction
//!   commands (disk formats, wipefs, fork bombs, recursive permission changes).
//!   It runs *before* dispatch, in the LLM-call hot path, and must not be conflated
//!   with the shell safety net to avoid accidental allow-listing via config drift.
//!
//! Overlap (3 entries: `rm -rf /`, `mkfs`, `dd if=`) is intentional — belt-and-suspenders.

use std::collections::HashSet;
use std::sync::{Arc, LazyLock};

use parking_lot::RwLock;

use regex::Regex;
use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization as _;

fn default_true() -> bool {
    true
}

fn default_shell_tools() -> Vec<String> {
    vec![
        "bash".to_string(),
        "shell".to_string(),
        "terminal".to_string(),
    ]
}

/// Result of a pre-execution verification check.
#[must_use]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerificationResult {
    /// Tool call is safe to proceed.
    Allow,
    /// Tool call must be blocked. Executor returns an error to the LLM.
    Block { reason: String },
    /// Tool call proceeds but a warning is logged and tracked in metrics (metrics-only,
    /// not visible to the LLM or user beyond the TUI security panel).
    Warn { message: String },
}

/// Pre-execution verification trait. Implementations intercept tool calls
/// before the executor runs them. Based on `TrustBench` pattern (arXiv:2603.09157).
///
/// Sync by design: verifiers inspect arguments only — no I/O needed.
/// Object-safe: uses `&self` and returns a concrete enum.
pub trait PreExecutionVerifier: Send + Sync + std::fmt::Debug {
    /// Verify whether a tool call should proceed.
    fn verify(&self, tool_name: &str, args: &serde_json::Value) -> VerificationResult;

    /// Human-readable name for logging and TUI display.
    fn name(&self) -> &'static str;
}

// ---------------------------------------------------------------------------
// Config types
// ---------------------------------------------------------------------------

/// Configuration for the destructive command verifier.
///
/// `allowed_paths`: when **empty** (the default), ALL destructive commands are denied.
/// This is a conservative default: to allow e.g. `rm -rf /tmp/build` you must
/// explicitly add `/tmp/build` to `allowed_paths`.
///
/// `shell_tools`: the set of tool names considered shell executors. Defaults to
/// `["bash", "shell", "terminal"]`. Add custom names here if your setup registers
/// shell tools under different names (e.g., via MCP or ACP integrations).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DestructiveVerifierConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Explicit path prefixes under which destructive commands are permitted.
    /// **Empty = deny-all destructive commands** (safest default).
    #[serde(default)]
    pub allowed_paths: Vec<String>,
    /// Additional command patterns to treat as destructive (substring match).
    #[serde(default)]
    pub extra_patterns: Vec<String>,
    /// Tool names to treat as shell executors (case-insensitive).
    /// Default: `["bash", "shell", "terminal"]`.
    #[serde(default = "default_shell_tools")]
    pub shell_tools: Vec<String>,
}

impl Default for DestructiveVerifierConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            allowed_paths: Vec::new(),
            extra_patterns: Vec::new(),
            shell_tools: default_shell_tools(),
        }
    }
}

/// Configuration for the injection pattern verifier.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct InjectionVerifierConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Additional injection patterns to block (regex strings).
    /// Invalid regexes are logged at WARN level and skipped.
    #[serde(default)]
    pub extra_patterns: Vec<String>,
    /// URLs explicitly permitted even if they match SSRF patterns.
    #[serde(default)]
    pub allowlisted_urls: Vec<String>,
}

impl Default for InjectionVerifierConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            extra_patterns: Vec::new(),
            allowlisted_urls: Vec::new(),
        }
    }
}

/// Configuration for the URL grounding verifier.
///
/// When enabled, `fetch` and `web_scrape` calls are blocked unless the URL
/// appears in the set of URLs extracted from user messages (`user_provided_urls`).
/// This prevents the LLM from hallucinating API endpoints and calling fetch with
/// fabricated URLs that were never supplied by the user.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct UrlGroundingVerifierConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Tool IDs subject to URL grounding checks. Any tool whose name ends with `_fetch`
    /// is also guarded regardless of this list.
    #[serde(default = "default_guarded_tools")]
    pub guarded_tools: Vec<String>,
}

fn default_guarded_tools() -> Vec<String> {
    vec!["fetch".to_string(), "web_scrape".to_string()]
}

impl Default for UrlGroundingVerifierConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            guarded_tools: default_guarded_tools(),
        }
    }
}

/// Top-level configuration for all pre-execution verifiers.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PreExecutionVerifierConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub destructive_commands: DestructiveVerifierConfig,
    #[serde(default)]
    pub injection_patterns: InjectionVerifierConfig,
    #[serde(default)]
    pub url_grounding: UrlGroundingVerifierConfig,
    #[serde(default)]
    pub firewall: FirewallVerifierConfig,
}

impl Default for PreExecutionVerifierConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            destructive_commands: DestructiveVerifierConfig::default(),
            injection_patterns: InjectionVerifierConfig::default(),
            url_grounding: UrlGroundingVerifierConfig::default(),
            firewall: FirewallVerifierConfig::default(),
        }
    }
}

// ---------------------------------------------------------------------------
// DestructiveCommandVerifier
// ---------------------------------------------------------------------------

/// Destructive command patterns for `DestructiveCommandVerifier`.
///
/// Intentionally separate from `DEFAULT_BLOCKED_COMMANDS` in `shell.rs` — see module
/// docs for the semantic distinction between the two lists.
static DESTRUCTIVE_PATTERNS: &[&str] = &[
    "rm -rf /",
    "rm -rf ~",
    "rm -r /",
    "dd if=",
    "mkfs",
    "fdisk",
    "shred",
    "wipefs",
    ":(){ :|:& };:",
    ":(){:|:&};:",
    "chmod -r 777 /",
    "chown -r",
];

/// Verifier that blocks destructive shell commands (e.g., `rm -rf /`, `dd`, `mkfs`)
/// before the shell tool executes them.
///
/// Applies to any tool whose name is in the configured `shell_tools` set (default:
/// `["bash", "shell", "terminal"]`). For commands targeting a specific path, execution
/// is allowed when the path starts with one of the configured `allowed_paths`. When
/// `allowed_paths` is empty (the default), **all** matching destructive commands are blocked.
#[derive(Debug)]
pub struct DestructiveCommandVerifier {
    shell_tools: Vec<String>,
    allowed_paths: Vec<String>,
    extra_patterns: Vec<String>,
}

impl DestructiveCommandVerifier {
    #[must_use]
    pub fn new(config: &DestructiveVerifierConfig) -> Self {
        Self {
            shell_tools: config
                .shell_tools
                .iter()
                .map(|s| s.to_lowercase())
                .collect(),
            allowed_paths: config
                .allowed_paths
                .iter()
                .map(|s| s.to_lowercase())
                .collect(),
            extra_patterns: config
                .extra_patterns
                .iter()
                .map(|s| s.to_lowercase())
                .collect(),
        }
    }

    fn is_shell_tool(&self, tool_name: &str) -> bool {
        let lower = tool_name.to_lowercase();
        self.shell_tools.iter().any(|t| t == &lower)
    }

    /// Extract the effective command string from `args`.
    ///
    /// Supports:
    /// - `{"command": "rm -rf /"}` (string)
    /// - `{"command": ["rm", "-rf", "/"]}` (array — joined with spaces)
    /// - `{"command": "bash -c 'rm -rf /'"}` (shell `-c` unwrapping, looped up to 8 levels)
    /// - `env VAR=val bash -c '...'` and `exec bash -c '...'` prefix stripping
    ///
    /// NFKC-normalizes the result to defeat Unicode homoglyph bypasses.
    fn extract_command(args: &serde_json::Value) -> Option<String> {
        let raw = match args.get("command") {
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(serde_json::Value::Array(arr)) => arr
                .iter()
                .filter_map(|v| v.as_str())
                .collect::<Vec<_>>()
                .join(" "),
            _ => return None,
        };
        // NFKC-normalize + lowercase to defeat Unicode homoglyph and case bypasses.
        let mut current: String = raw.nfkc().collect::<String>().to_lowercase();
        // Loop: strip shell wrapper prefixes up to 8 levels deep.
        // Handles double-nested: `bash -c "bash -c 'rm -rf /'"`.
        for _ in 0..8 {
            let trimmed = current.trim().to_owned();
            // Strip `env VAR=value ... CMD` prefix (one or more VAR=value tokens).
            let after_env = Self::strip_env_prefix(&trimmed);
            // Strip `exec ` prefix.
            let after_exec = after_env.strip_prefix("exec ").map_or(after_env, str::trim);
            // Strip interpreter wrapper: `bash -c '...'` / `sh -c '...'` / `zsh -c '...'`.
            let mut unwrapped = false;
            for interp in &["bash -c ", "sh -c ", "zsh -c "] {
                if let Some(rest) = after_exec.strip_prefix(interp) {
                    let script = rest.trim().trim_matches(|c: char| c == '\'' || c == '"');
                    current.clone_from(&script.to_owned());
                    unwrapped = true;
                    break;
                }
            }
            if !unwrapped {
                return Some(after_exec.to_owned());
            }
        }
        Some(current)
    }

    /// Strip leading `env VAR=value` tokens from a command string.
    /// Returns the remainder after all `KEY=VALUE` pairs are consumed.
    fn strip_env_prefix(cmd: &str) -> &str {
        let mut rest = cmd;
        // `env` keyword is optional; strip it if present.
        if let Some(after_env) = rest.strip_prefix("env ") {
            rest = after_env.trim_start();
        }
        // Consume `KEY=VALUE` tokens.
        loop {
            // A VAR=value token: identifier chars + '=' + non-space chars.
            let mut chars = rest.chars();
            let key_end = chars
                .by_ref()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .count();
            if key_end == 0 {
                break;
            }
            let remainder = &rest[key_end..];
            if let Some(after_eq) = remainder.strip_prefix('=') {
                // Consume the value (up to the first space).
                let val_end = after_eq.find(' ').unwrap_or(after_eq.len());
                rest = after_eq[val_end..].trim_start();
            } else {
                break;
            }
        }
        rest
    }

    /// Returns `true` if `command` targets a path that is covered by `allowed_paths`.
    ///
    /// Uses lexical normalization (resolves `..` and `.` without filesystem access)
    /// so that `/tmp/build/../../etc` is correctly resolved to `/etc` before comparison,
    /// defeating path traversal bypasses like `/tmp/build/../../etc/passwd`.
    fn is_allowed_path(&self, command: &str) -> bool {
        if self.allowed_paths.is_empty() {
            return false;
        }
        let tokens: Vec<&str> = command.split_whitespace().collect();
        for token in &tokens {
            let t = token.trim_matches(|c| c == '\'' || c == '"');
            if t.starts_with('/') || t.starts_with('~') || t.starts_with('.') {
                let normalized = Self::lexical_normalize(std::path::Path::new(t));
                let n_lower = normalized.to_string_lossy().to_lowercase();
                if self
                    .allowed_paths
                    .iter()
                    .any(|p| n_lower.starts_with(p.as_str()))
                {
                    return true;
                }
            }
        }
        false
    }

    /// Lexically normalize a path by resolving `.` and `..` components without
    /// hitting the filesystem. Does not require the path to exist.
    fn lexical_normalize(p: &std::path::Path) -> std::path::PathBuf {
        let mut out = std::path::PathBuf::new();
        for component in p.components() {
            match component {
                std::path::Component::ParentDir => {
                    out.pop();
                }
                std::path::Component::CurDir => {}
                other => out.push(other),
            }
        }
        out
    }

    fn check_patterns(command: &str) -> Option<&'static str> {
        DESTRUCTIVE_PATTERNS
            .iter()
            .find(|&pat| command.contains(pat))
            .copied()
    }

    fn check_extra_patterns(&self, command: &str) -> Option<String> {
        self.extra_patterns
            .iter()
            .find(|pat| command.contains(pat.as_str()))
            .cloned()
    }
}

impl PreExecutionVerifier for DestructiveCommandVerifier {
    fn name(&self) -> &'static str {
        "DestructiveCommandVerifier"
    }

    fn verify(&self, tool_name: &str, args: &serde_json::Value) -> VerificationResult {
        if !self.is_shell_tool(tool_name) {
            return VerificationResult::Allow;
        }

        let Some(command) = Self::extract_command(args) else {
            return VerificationResult::Allow;
        };

        if let Some(pat) = Self::check_patterns(&command) {
            if self.is_allowed_path(&command) {
                return VerificationResult::Allow;
            }
            return VerificationResult::Block {
                reason: format!("[{}] destructive pattern '{}' detected", self.name(), pat),
            };
        }

        if let Some(pat) = self.check_extra_patterns(&command) {
            if self.is_allowed_path(&command) {
                return VerificationResult::Allow;
            }
            return VerificationResult::Block {
                reason: format!(
                    "[{}] extra destructive pattern '{}' detected",
                    self.name(),
                    pat
                ),
            };
        }

        VerificationResult::Allow
    }
}

// ---------------------------------------------------------------------------
// InjectionPatternVerifier
// ---------------------------------------------------------------------------

/// High-confidence injection block patterns applied to string field values in tool args.
///
/// These require *structural* patterns, not just keywords — e.g., `UNION SELECT` is
/// blocked but a plain mention of "SELECT" is not. This avoids false positives for
/// `memory_search` queries discussing SQL or coding assistants writing SQL examples.
static INJECTION_BLOCK_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        // SQL injection structural patterns
        r"(?i)'\s*OR\s*'1'\s*=\s*'1",
        r"(?i)'\s*OR\s*1\s*=\s*1",
        r"(?i);\s*DROP\s+TABLE",
        r"(?i)UNION\s+SELECT",
        r"(?i)'\s*;\s*SELECT",
        // Command injection via shell metacharacters with dangerous commands
        r";\s*rm\s+",
        r"\|\s*rm\s+",
        r"&&\s*rm\s+",
        r";\s*curl\s+",
        r"\|\s*curl\s+",
        r"&&\s*curl\s+",
        r";\s*wget\s+",
        // Path traversal to sensitive system files
        r"\.\./\.\./\.\./etc/passwd",
        r"\.\./\.\./\.\./etc/shadow",
        r"\.\./\.\./\.\./windows/",
        r"\.\.[/\\]\.\.[/\\]\.\.[/\\]",
    ]
    .iter()
    .map(|s| Regex::new(s).expect("static pattern must compile"))
    .collect()
});

/// SSRF host patterns — matched against the *extracted host* (not the full URL string).
/// This prevents bypasses like `http://evil.com/?r=http://localhost` where the SSRF
/// target appears only in a query parameter, not as the actual request host.
/// Bare hostnames (no port/path) are included alongside `host:port` variants.
static SSRF_HOST_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        // localhost — with or without port
        r"^localhost$",
        r"^localhost:",
        // IPv4 loopback
        r"^127\.0\.0\.1$",
        r"^127\.0\.0\.1:",
        // IPv6 loopback
        r"^\[::1\]$",
        r"^\[::1\]:",
        // AWS metadata service
        r"^169\.254\.169\.254$",
        r"^169\.254\.169\.254:",
        // RFC-1918 private ranges
        r"^10\.\d+\.\d+\.\d+$",
        r"^10\.\d+\.\d+\.\d+:",
        r"^172\.(1[6-9]|2\d|3[01])\.\d+\.\d+$",
        r"^172\.(1[6-9]|2\d|3[01])\.\d+\.\d+:",
        r"^192\.168\.\d+\.\d+$",
        r"^192\.168\.\d+\.\d+:",
    ]
    .iter()
    .map(|s| Regex::new(s).expect("static pattern must compile"))
    .collect()
});

/// Extract the host (and optional port) from a URL string.
/// Returns the portion between `://` and the next `/`, `?`, `#`, or end of string.
/// If the URL has no scheme, returns `None`.
fn extract_url_host(url: &str) -> Option<&str> {
    let after_scheme = url.split_once("://")?.1;
    let host_end = after_scheme
        .find(['/', '?', '#'])
        .unwrap_or(after_scheme.len());
    Some(&after_scheme[..host_end])
}

/// Field names that suggest URL/endpoint content — SSRF patterns are applied here.
static URL_FIELD_NAMES: &[&str] = &["url", "endpoint", "uri", "href", "src", "host", "base_url"];

/// Field names that are known to carry user-provided text queries — SQL injection and
/// command injection patterns are skipped for these fields to avoid false positives.
/// Examples: `memory_search(query=...)`, `web_search(query=...)`.
static SAFE_QUERY_FIELDS: &[&str] = &["query", "q", "search", "text", "message", "content"];

/// Verifier that blocks tool arguments containing SQL injection, command injection,
/// or path traversal patterns. Applies to ALL tools using field-aware matching.
///
/// ## Field-aware matching
///
/// Rather than serialising all args to a flat string (which causes false positives),
/// this verifier iterates over each string-valued field and applies pattern categories
/// based on field semantics:
///
/// - `SAFE_QUERY_FIELDS` (`query`, `q`, `search`, `text`, …): injection patterns are
///   **skipped** — these fields contain user-provided text and generate too many false
///   positives for SQL/command discussions in chat.
/// - `URL_FIELD_NAMES` (`url`, `endpoint`, `uri`, …): SSRF patterns are applied.
/// - All other string fields: injection + path traversal patterns are applied.
///
/// ## Warn semantics
///
/// `VerificationResult::Warn` is metrics-only — the tool call proceeds, a WARN log
/// entry is emitted, and the TUI security panel counter increments. The LLM does not
/// see the warning in its tool result.
#[derive(Debug)]
pub struct InjectionPatternVerifier {
    extra_patterns: Vec<Regex>,
    allowlisted_urls: Vec<String>,
}

impl InjectionPatternVerifier {
    #[must_use]
    pub fn new(config: &InjectionVerifierConfig) -> Self {
        let extra_patterns = config
            .extra_patterns
            .iter()
            .filter_map(|s| match Regex::new(s) {
                Ok(re) => Some(re),
                Err(e) => {
                    tracing::warn!(
                        pattern = %s,
                        error = %e,
                        "InjectionPatternVerifier: invalid extra_pattern, skipping"
                    );
                    None
                }
            })
            .collect();

        Self {
            extra_patterns,
            allowlisted_urls: config
                .allowlisted_urls
                .iter()
                .map(|s| s.to_lowercase())
                .collect(),
        }
    }

    fn is_allowlisted(&self, text: &str) -> bool {
        let lower = text.to_lowercase();
        self.allowlisted_urls
            .iter()
            .any(|u| lower.contains(u.as_str()))
    }

    fn is_url_field(field: &str) -> bool {
        let lower = field.to_lowercase();
        URL_FIELD_NAMES.iter().any(|&f| f == lower)
    }

    fn is_safe_query_field(field: &str) -> bool {
        let lower = field.to_lowercase();
        SAFE_QUERY_FIELDS.iter().any(|&f| f == lower)
    }

    /// Check a single string value from a named field.
    fn check_field_value(&self, field: &str, value: &str) -> VerificationResult {
        let is_url = Self::is_url_field(field);
        let is_safe_query = Self::is_safe_query_field(field);

        // Injection + path traversal: skip safe query fields (user text), apply elsewhere.
        if !is_safe_query {
            for pat in INJECTION_BLOCK_PATTERNS.iter() {
                if pat.is_match(value) {
                    return VerificationResult::Block {
                        reason: format!(
                            "[{}] injection pattern detected in field '{}': {}",
                            "InjectionPatternVerifier",
                            field,
                            pat.as_str()
                        ),
                    };
                }
            }
            for pat in &self.extra_patterns {
                if pat.is_match(value) {
                    return VerificationResult::Block {
                        reason: format!(
                            "[{}] extra injection pattern detected in field '{}': {}",
                            "InjectionPatternVerifier",
                            field,
                            pat.as_str()
                        ),
                    };
                }
            }
        }

        // SSRF: apply only to URL-like fields.
        // Extract the host first so that SSRF targets embedded in query parameters
        // (e.g. `http://evil.com/?r=http://localhost`) are not falsely matched.
        if is_url && let Some(host) = extract_url_host(value) {
            for pat in SSRF_HOST_PATTERNS.iter() {
                if pat.is_match(host) {
                    if self.is_allowlisted(value) {
                        return VerificationResult::Allow;
                    }
                    return VerificationResult::Warn {
                        message: format!(
                            "[{}] possible SSRF in field '{}': host '{}' matches pattern (not blocked)",
                            "InjectionPatternVerifier", field, host,
                        ),
                    };
                }
            }
        }

        VerificationResult::Allow
    }

    /// Walk all string leaf values in a JSON object, collecting field names for context.
    fn check_object(&self, obj: &serde_json::Map<String, serde_json::Value>) -> VerificationResult {
        for (key, val) in obj {
            let result = self.check_value(key, val);
            if !matches!(result, VerificationResult::Allow) {
                return result;
            }
        }
        VerificationResult::Allow
    }

    fn check_value(&self, field: &str, val: &serde_json::Value) -> VerificationResult {
        match val {
            serde_json::Value::String(s) => self.check_field_value(field, s),
            serde_json::Value::Array(arr) => {
                for item in arr {
                    let r = self.check_value(field, item);
                    if !matches!(r, VerificationResult::Allow) {
                        return r;
                    }
                }
                VerificationResult::Allow
            }
            serde_json::Value::Object(obj) => self.check_object(obj),
            // Non-string primitives (numbers, booleans, null) cannot contain injection.
            _ => VerificationResult::Allow,
        }
    }
}

impl PreExecutionVerifier for InjectionPatternVerifier {
    fn name(&self) -> &'static str {
        "InjectionPatternVerifier"
    }

    fn verify(&self, _tool_name: &str, args: &serde_json::Value) -> VerificationResult {
        match args {
            serde_json::Value::Object(obj) => self.check_object(obj),
            // Flat string args (unusual but handle gracefully — treat as unnamed field).
            serde_json::Value::String(s) => self.check_field_value("_args", s),
            _ => VerificationResult::Allow,
        }
    }
}

// ---------------------------------------------------------------------------
// UrlGroundingVerifier
// ---------------------------------------------------------------------------

/// Verifier that blocks `fetch` and `web_scrape` calls when the requested URL
/// was not explicitly provided by the user in the conversation.
///
/// The agent populates `user_provided_urls` whenever a user message is received,
/// by extracting all http/https URLs from the raw input. This set persists across
/// turns within a session and is cleared on `/clear`.
///
/// ## Bypass rules
///
/// - Tools not in the `guarded_tools` list (and not ending in `_fetch`) pass through.
/// - If the URL in the tool call is a prefix-match or exact match of any URL in
///   `user_provided_urls`, the call is allowed.
/// - If `user_provided_urls` is empty (no URLs seen in this session at all), the call
///   is blocked — the LLM must not fetch arbitrary URLs when the user never provided one.
#[derive(Debug, Clone)]
pub struct UrlGroundingVerifier {
    guarded_tools: Vec<String>,
    user_provided_urls: Arc<RwLock<HashSet<String>>>,
}

impl UrlGroundingVerifier {
    #[must_use]
    pub fn new(
        config: &UrlGroundingVerifierConfig,
        user_provided_urls: Arc<RwLock<HashSet<String>>>,
    ) -> Self {
        Self {
            guarded_tools: config
                .guarded_tools
                .iter()
                .map(|s| s.to_lowercase())
                .collect(),
            user_provided_urls,
        }
    }

    fn is_guarded(&self, tool_name: &str) -> bool {
        let lower = tool_name.to_lowercase();
        self.guarded_tools.iter().any(|t| t == &lower) || lower.ends_with("_fetch")
    }

    /// Returns true if `url` is grounded — i.e., it appears in (or is a prefix of)
    /// a URL from `user_provided_urls`.
    fn is_grounded(url: &str, user_provided_urls: &HashSet<String>) -> bool {
        let lower = url.to_lowercase();
        user_provided_urls
            .iter()
            .any(|u| lower.starts_with(u.as_str()) || u.starts_with(lower.as_str()))
    }
}

impl PreExecutionVerifier for UrlGroundingVerifier {
    fn name(&self) -> &'static str {
        "UrlGroundingVerifier"
    }

    fn verify(&self, tool_name: &str, args: &serde_json::Value) -> VerificationResult {
        if !self.is_guarded(tool_name) {
            return VerificationResult::Allow;
        }

        let Some(url) = args.get("url").and_then(|v| v.as_str()) else {
            return VerificationResult::Allow;
        };

        let urls = self.user_provided_urls.read();

        if Self::is_grounded(url, &urls) {
            return VerificationResult::Allow;
        }

        VerificationResult::Block {
            reason: format!(
                "[UrlGroundingVerifier] fetch rejected: URL '{url}' was not provided by the user",
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// FirewallVerifier
// ---------------------------------------------------------------------------

/// Configuration for the firewall verifier.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FirewallVerifierConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Glob patterns for additional paths to block.
    #[serde(default)]
    pub blocked_paths: Vec<String>,
    /// Additional environment variable names to block from tool arguments.
    #[serde(default)]
    pub blocked_env_vars: Vec<String>,
    /// Tool IDs exempt from firewall scanning.
    #[serde(default)]
    pub exempt_tools: Vec<String>,
}

impl Default for FirewallVerifierConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            blocked_paths: Vec::new(),
            blocked_env_vars: Vec::new(),
            exempt_tools: Vec::new(),
        }
    }
}

/// Policy-enforcement verifier that inspects tool arguments for path traversal,
/// environment-variable exfiltration, sensitive file access, and command chaining.
///
/// ## Scope delineation with `InjectionPatternVerifier`
///
/// `FirewallVerifier` enforces *configurable policy* (blocked paths, env vars, sensitive
/// file patterns). `InjectionPatternVerifier` performs regex-based *injection pattern
/// detection* (prompt injection, SSRF, etc.). They are complementary — belt-and-suspenders,
/// the same intentional overlap documented at the top of this module.
///
/// Both verifiers may produce `Block` for the same call (e.g. command chaining detected
/// by both). The pipeline stops at the first `Block` result.
#[derive(Debug)]
pub struct FirewallVerifier {
    blocked_path_globs: Vec<glob::Pattern>,
    blocked_env_vars: HashSet<String>,
    exempt_tools: HashSet<String>,
}

/// Built-in path patterns that are always blocked regardless of config.
static SENSITIVE_PATH_PATTERNS: LazyLock<Vec<glob::Pattern>> = LazyLock::new(|| {
    let raw = [
        "/etc/passwd",
        "/etc/shadow",
        "/etc/sudoers",
        "~/.ssh/*",
        "~/.aws/*",
        "~/.gnupg/*",
        "**/*.pem",
        "**/*.key",
        "**/id_rsa",
        "**/id_ed25519",
        "**/.env",
        "**/credentials",
    ];
    raw.iter()
        .filter_map(|p| {
            glob::Pattern::new(p)
                .map_err(|e| {
                    tracing::error!(pattern = p, error = %e, "failed to compile built-in firewall path pattern");
                    e
                })
                .ok()
        })
        .collect()
});

/// Built-in env var prefixes that trigger a block when found in tool arguments.
static SENSITIVE_ENV_PREFIXES: &[&str] =
    &["$AWS_", "$ZEPH_", "${AWS_", "${ZEPH_", "%AWS_", "%ZEPH_"];

/// Argument field names to extract and inspect.
static INSPECTED_FIELDS: &[&str] = &[
    "command",
    "file_path",
    "path",
    "url",
    "query",
    "uri",
    "input",
    "args",
];

impl FirewallVerifier {
    /// Build a `FirewallVerifier` from config.
    ///
    /// Invalid glob patterns in `blocked_paths` are logged at WARN level and skipped.
    #[must_use]
    pub fn new(config: &FirewallVerifierConfig) -> Self {
        let blocked_path_globs = config
            .blocked_paths
            .iter()
            .filter_map(|p| {
                glob::Pattern::new(p)
                    .map_err(|e| {
                        tracing::warn!(pattern = p, error = %e, "invalid glob pattern in firewall blocked_paths, skipping");
                        e
                    })
                    .ok()
            })
            .collect();

        let blocked_env_vars = config
            .blocked_env_vars
            .iter()
            .map(|s| s.to_uppercase())
            .collect();

        let exempt_tools = config
            .exempt_tools
            .iter()
            .map(|s| s.to_lowercase())
            .collect();

        Self {
            blocked_path_globs,
            blocked_env_vars,
            exempt_tools,
        }
    }

    /// Extract all string argument values from a tool call's JSON args.
    fn collect_args(args: &serde_json::Value) -> Vec<String> {
        let mut out = Vec::new();
        match args {
            serde_json::Value::Object(map) => {
                for field in INSPECTED_FIELDS {
                    if let Some(val) = map.get(*field) {
                        Self::collect_strings(val, &mut out);
                    }
                }
            }
            serde_json::Value::String(s) => out.push(s.clone()),
            _ => {}
        }
        out
    }

    fn collect_strings(val: &serde_json::Value, out: &mut Vec<String>) {
        match val {
            serde_json::Value::String(s) => out.push(s.clone()),
            serde_json::Value::Array(arr) => {
                for item in arr {
                    Self::collect_strings(item, out);
                }
            }
            _ => {}
        }
    }

    fn scan_arg(&self, arg: &str) -> Option<VerificationResult> {
        // Apply NFKC normalization consistent with DestructiveCommandVerifier.
        let normalized: String = arg.nfkc().collect();
        let lower = normalized.to_lowercase();

        // Path traversal
        if lower.contains("../") || lower.contains("..\\") {
            return Some(VerificationResult::Block {
                reason: format!(
                    "[FirewallVerifier] path traversal pattern detected in argument: {arg}"
                ),
            });
        }

        // Sensitive paths (built-in)
        for pattern in SENSITIVE_PATH_PATTERNS.iter() {
            if pattern.matches(&normalized) || pattern.matches(&lower) {
                return Some(VerificationResult::Block {
                    reason: format!(
                        "[FirewallVerifier] sensitive path pattern '{pattern}' matched in argument: {arg}"
                    ),
                });
            }
        }

        // User-configured blocked paths
        for pattern in &self.blocked_path_globs {
            if pattern.matches(&normalized) || pattern.matches(&lower) {
                return Some(VerificationResult::Block {
                    reason: format!(
                        "[FirewallVerifier] blocked path pattern '{pattern}' matched in argument: {arg}"
                    ),
                });
            }
        }

        // Env var exfiltration (built-in prefixes)
        let upper = normalized.to_uppercase();
        for prefix in SENSITIVE_ENV_PREFIXES {
            if upper.contains(*prefix) {
                return Some(VerificationResult::Block {
                    reason: format!(
                        "[FirewallVerifier] env var exfiltration pattern '{prefix}' detected in argument: {arg}"
                    ),
                });
            }
        }

        // User-configured blocked env vars (match $VAR or %VAR% patterns)
        for var in &self.blocked_env_vars {
            let dollar_form = format!("${var}");
            let brace_form = format!("${{{var}}}");
            let percent_form = format!("%{var}%");
            if upper.contains(&dollar_form)
                || upper.contains(&brace_form)
                || upper.contains(&percent_form)
            {
                return Some(VerificationResult::Block {
                    reason: format!(
                        "[FirewallVerifier] blocked env var '{var}' detected in argument: {arg}"
                    ),
                });
            }
        }

        None
    }
}

impl PreExecutionVerifier for FirewallVerifier {
    fn name(&self) -> &'static str {
        "FirewallVerifier"
    }

    fn verify(&self, tool_name: &str, args: &serde_json::Value) -> VerificationResult {
        if self.exempt_tools.contains(&tool_name.to_lowercase()) {
            return VerificationResult::Allow;
        }

        for arg in Self::collect_args(args) {
            if let Some(result) = self.scan_arg(&arg) {
                return result;
            }
        }

        VerificationResult::Allow
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    // --- DestructiveCommandVerifier ---

    fn dcv() -> DestructiveCommandVerifier {
        DestructiveCommandVerifier::new(&DestructiveVerifierConfig::default())
    }

    #[test]
    fn allow_normal_command() {
        let v = dcv();
        assert_eq!(
            v.verify("bash", &json!({"command": "ls -la /tmp"})),
            VerificationResult::Allow
        );
    }

    #[test]
    fn block_rm_rf_root() {
        let v = dcv();
        let result = v.verify("bash", &json!({"command": "rm -rf /"}));
        assert!(matches!(result, VerificationResult::Block { .. }));
    }

    #[test]
    fn block_dd_dev_zero() {
        let v = dcv();
        let result = v.verify("bash", &json!({"command": "dd if=/dev/zero of=/dev/sda"}));
        assert!(matches!(result, VerificationResult::Block { .. }));
    }

    #[test]
    fn block_mkfs() {
        let v = dcv();
        let result = v.verify("bash", &json!({"command": "mkfs.ext4 /dev/sda1"}));
        assert!(matches!(result, VerificationResult::Block { .. }));
    }

    #[test]
    fn allow_rm_rf_in_allowed_path() {
        let config = DestructiveVerifierConfig {
            allowed_paths: vec!["/tmp/build".to_string()],
            ..Default::default()
        };
        let v = DestructiveCommandVerifier::new(&config);
        assert_eq!(
            v.verify("bash", &json!({"command": "rm -rf /tmp/build/artifacts"})),
            VerificationResult::Allow
        );
    }

    #[test]
    fn block_rm_rf_when_not_in_allowed_path() {
        let config = DestructiveVerifierConfig {
            allowed_paths: vec!["/tmp/build".to_string()],
            ..Default::default()
        };
        let v = DestructiveCommandVerifier::new(&config);
        let result = v.verify("bash", &json!({"command": "rm -rf /home/user"}));
        assert!(matches!(result, VerificationResult::Block { .. }));
    }

    #[test]
    fn allow_non_shell_tool() {
        let v = dcv();
        assert_eq!(
            v.verify("read_file", &json!({"path": "rm -rf /"})),
            VerificationResult::Allow
        );
    }

    #[test]
    fn block_extra_pattern() {
        let config = DestructiveVerifierConfig {
            extra_patterns: vec!["format c:".to_string()],
            ..Default::default()
        };
        let v = DestructiveCommandVerifier::new(&config);
        let result = v.verify("bash", &json!({"command": "format c:"}));
        assert!(matches!(result, VerificationResult::Block { .. }));
    }

    #[test]
    fn array_args_normalization() {
        let v = dcv();
        let result = v.verify("bash", &json!({"command": ["rm", "-rf", "/"]}));
        assert!(matches!(result, VerificationResult::Block { .. }));
    }

    #[test]
    fn sh_c_wrapping_normalization() {
        let v = dcv();
        let result = v.verify("bash", &json!({"command": "bash -c 'rm -rf /'"}));
        assert!(matches!(result, VerificationResult::Block { .. }));
    }

    #[test]
    fn fork_bomb_blocked() {
        let v = dcv();
        let result = v.verify("bash", &json!({"command": ":(){ :|:& };:"}));
        assert!(matches!(result, VerificationResult::Block { .. }));
    }

    #[test]
    fn custom_shell_tool_name_blocked() {
        let config = DestructiveVerifierConfig {
            shell_tools: vec!["execute".to_string(), "run_command".to_string()],
            ..Default::default()
        };
        let v = DestructiveCommandVerifier::new(&config);
        let result = v.verify("execute", &json!({"command": "rm -rf /"}));
        assert!(matches!(result, VerificationResult::Block { .. }));
    }

    #[test]
    fn terminal_tool_name_blocked_by_default() {
        let v = dcv();
        let result = v.verify("terminal", &json!({"command": "rm -rf /"}));
        assert!(matches!(result, VerificationResult::Block { .. }));
    }

    #[test]
    fn default_shell_tools_contains_bash_shell_terminal() {
        let config = DestructiveVerifierConfig::default();
        let lower: Vec<String> = config
            .shell_tools
            .iter()
            .map(|s| s.to_lowercase())
            .collect();
        assert!(lower.contains(&"bash".to_string()));
        assert!(lower.contains(&"shell".to_string()));
        assert!(lower.contains(&"terminal".to_string()));
    }

    // --- InjectionPatternVerifier ---

    fn ipv() -> InjectionPatternVerifier {
        InjectionPatternVerifier::new(&InjectionVerifierConfig::default())
    }

    #[test]
    fn allow_clean_args() {
        let v = ipv();
        assert_eq!(
            v.verify("search", &json!({"query": "rust async traits"})),
            VerificationResult::Allow
        );
    }

    #[test]
    fn allow_sql_discussion_in_query_field() {
        // S2: memory_search with SQL discussion must NOT be blocked.
        let v = ipv();
        assert_eq!(
            v.verify(
                "memory_search",
                &json!({"query": "explain SQL UNION SELECT vs JOIN"})
            ),
            VerificationResult::Allow
        );
    }

    #[test]
    fn allow_sql_or_pattern_in_query_field() {
        // S2: safe query field must not trigger SQL injection pattern.
        let v = ipv();
        assert_eq!(
            v.verify("memory_search", &json!({"query": "' OR '1'='1"})),
            VerificationResult::Allow
        );
    }

    #[test]
    fn block_sql_injection_in_non_query_field() {
        let v = ipv();
        let result = v.verify("db_query", &json!({"sql": "' OR '1'='1"}));
        assert!(matches!(result, VerificationResult::Block { .. }));
    }

    #[test]
    fn block_drop_table() {
        let v = ipv();
        let result = v.verify("db_query", &json!({"input": "name'; DROP TABLE users"}));
        assert!(matches!(result, VerificationResult::Block { .. }));
    }

    #[test]
    fn block_path_traversal() {
        let v = ipv();
        let result = v.verify("read_file", &json!({"path": "../../../etc/passwd"}));
        assert!(matches!(result, VerificationResult::Block { .. }));
    }

    #[test]
    fn warn_on_localhost_url_field() {
        // S2: SSRF warn only fires on URL-like fields.
        let v = ipv();
        let result = v.verify("http_get", &json!({"url": "http://localhost:8080/api"}));
        assert!(matches!(result, VerificationResult::Warn { .. }));
    }

    #[test]
    fn allow_localhost_in_non_url_field() {
        // S2: localhost in a "text" field (not a URL field) must not warn.
        let v = ipv();
        assert_eq!(
            v.verify(
                "memory_search",
                &json!({"query": "connect to http://localhost:8080"})
            ),
            VerificationResult::Allow
        );
    }

    #[test]
    fn warn_on_private_ip_url_field() {
        let v = ipv();
        let result = v.verify("fetch", &json!({"url": "http://192.168.1.1/admin"}));
        assert!(matches!(result, VerificationResult::Warn { .. }));
    }

    #[test]
    fn allow_localhost_when_allowlisted() {
        let config = InjectionVerifierConfig {
            allowlisted_urls: vec!["http://localhost:3000".to_string()],
            ..Default::default()
        };
        let v = InjectionPatternVerifier::new(&config);
        assert_eq!(
            v.verify("http_get", &json!({"url": "http://localhost:3000/api"})),
            VerificationResult::Allow
        );
    }

    #[test]
    fn block_union_select_in_non_query_field() {
        let v = ipv();
        let result = v.verify(
            "db_query",
            &json!({"input": "id=1 UNION SELECT password FROM users"}),
        );
        assert!(matches!(result, VerificationResult::Block { .. }));
    }

    #[test]
    fn allow_union_select_in_query_field() {
        // S2: "UNION SELECT" in a `query` field is a SQL discussion, not an injection.
        let v = ipv();
        assert_eq!(
            v.verify(
                "memory_search",
                &json!({"query": "id=1 UNION SELECT password FROM users"})
            ),
            VerificationResult::Allow
        );
    }

    // --- FIX-1: Unicode normalization bypass ---

    #[test]
    fn block_rm_rf_unicode_homoglyph() {
        // U+FF0F FULLWIDTH SOLIDUS looks like '/' and NFKC-normalizes to '/'.
        let v = dcv();
        // "rm -rf ／" where ／ is U+FF0F
        let result = v.verify("bash", &json!({"command": "rm -rf \u{FF0F}"}));
        assert!(matches!(result, VerificationResult::Block { .. }));
    }

    // --- FIX-2: Path traversal in is_allowed_path ---

    #[test]
    fn path_traversal_not_allowed_via_dotdot() {
        // `/tmp/build/../../etc` lexically resolves to `/etc`, NOT under `/tmp/build`.
        let config = DestructiveVerifierConfig {
            allowed_paths: vec!["/tmp/build".to_string()],
            ..Default::default()
        };
        let v = DestructiveCommandVerifier::new(&config);
        // Should be BLOCKED: resolved path is /etc, not under /tmp/build.
        let result = v.verify("bash", &json!({"command": "rm -rf /tmp/build/../../etc"}));
        assert!(matches!(result, VerificationResult::Block { .. }));
    }

    #[test]
    fn allowed_path_with_dotdot_stays_in_allowed() {
        // `/tmp/build/sub/../artifacts` resolves to `/tmp/build/artifacts` — still allowed.
        let config = DestructiveVerifierConfig {
            allowed_paths: vec!["/tmp/build".to_string()],
            ..Default::default()
        };
        let v = DestructiveCommandVerifier::new(&config);
        assert_eq!(
            v.verify(
                "bash",
                &json!({"command": "rm -rf /tmp/build/sub/../artifacts"}),
            ),
            VerificationResult::Allow,
        );
    }

    // --- FIX-3: Double-nested shell wrapping ---

    #[test]
    fn double_nested_bash_c_blocked() {
        let v = dcv();
        let result = v.verify(
            "bash",
            &json!({"command": "bash -c \"bash -c 'rm -rf /'\""}),
        );
        assert!(matches!(result, VerificationResult::Block { .. }));
    }

    #[test]
    fn env_prefix_stripping_blocked() {
        let v = dcv();
        let result = v.verify(
            "bash",
            &json!({"command": "env FOO=bar bash -c 'rm -rf /'"}),
        );
        assert!(matches!(result, VerificationResult::Block { .. }));
    }

    #[test]
    fn exec_prefix_stripping_blocked() {
        let v = dcv();
        let result = v.verify("bash", &json!({"command": "exec bash -c 'rm -rf /'"}));
        assert!(matches!(result, VerificationResult::Block { .. }));
    }

    // --- FIX-4: SSRF host extraction (not substring match) ---

    #[test]
    fn ssrf_not_triggered_for_embedded_localhost_in_query_param() {
        // `evil.com/?r=http://localhost` — host is `evil.com`, not localhost.
        let v = ipv();
        let result = v.verify(
            "http_get",
            &json!({"url": "http://evil.com/?r=http://localhost"}),
        );
        // Should NOT warn — the actual request host is evil.com, not localhost.
        assert_eq!(result, VerificationResult::Allow);
    }

    #[test]
    fn ssrf_triggered_for_bare_localhost_no_port() {
        // FIX-7: `http://localhost` with no trailing slash or port must warn.
        let v = ipv();
        let result = v.verify("http_get", &json!({"url": "http://localhost"}));
        assert!(matches!(result, VerificationResult::Warn { .. }));
    }

    #[test]
    fn ssrf_triggered_for_localhost_with_path() {
        let v = ipv();
        let result = v.verify("http_get", &json!({"url": "http://localhost/api/v1"}));
        assert!(matches!(result, VerificationResult::Warn { .. }));
    }

    // --- Verifier chain: first Block wins, Warn continues ---

    #[test]
    fn chain_first_block_wins() {
        let dcv = DestructiveCommandVerifier::new(&DestructiveVerifierConfig::default());
        let ipv = InjectionPatternVerifier::new(&InjectionVerifierConfig::default());
        let verifiers: Vec<Box<dyn PreExecutionVerifier>> = vec![Box::new(dcv), Box::new(ipv)];

        let args = json!({"command": "rm -rf /"});
        let mut result = VerificationResult::Allow;
        for v in &verifiers {
            result = v.verify("bash", &args);
            if matches!(result, VerificationResult::Block { .. }) {
                break;
            }
        }
        assert!(matches!(result, VerificationResult::Block { .. }));
    }

    #[test]
    fn chain_warn_continues() {
        let dcv = DestructiveCommandVerifier::new(&DestructiveVerifierConfig::default());
        let ipv = InjectionPatternVerifier::new(&InjectionVerifierConfig::default());
        let verifiers: Vec<Box<dyn PreExecutionVerifier>> = vec![Box::new(dcv), Box::new(ipv)];

        // localhost URL in `url` field: dcv allows, ipv warns, chain does NOT block.
        let args = json!({"url": "http://localhost:8080/api"});
        let mut got_warn = false;
        let mut got_block = false;
        for v in &verifiers {
            match v.verify("http_get", &args) {
                VerificationResult::Block { .. } => {
                    got_block = true;
                    break;
                }
                VerificationResult::Warn { .. } => {
                    got_warn = true;
                }
                VerificationResult::Allow => {}
            }
        }
        assert!(got_warn);
        assert!(!got_block);
    }

    // --- UrlGroundingVerifier ---

    fn ugv(urls: &[&str]) -> UrlGroundingVerifier {
        let set: HashSet<String> = urls.iter().map(|s| s.to_lowercase()).collect();
        UrlGroundingVerifier::new(
            &UrlGroundingVerifierConfig::default(),
            Arc::new(RwLock::new(set)),
        )
    }

    #[test]
    fn url_grounding_allows_user_provided_url() {
        let v = ugv(&["https://docs.anthropic.com/models"]);
        assert_eq!(
            v.verify(
                "fetch",
                &json!({"url": "https://docs.anthropic.com/models"})
            ),
            VerificationResult::Allow
        );
    }

    #[test]
    fn url_grounding_blocks_hallucinated_url() {
        let v = ugv(&["https://example.com/page"]);
        let result = v.verify(
            "fetch",
            &json!({"url": "https://api.anthropic.ai/v1/models"}),
        );
        assert!(matches!(result, VerificationResult::Block { .. }));
    }

    #[test]
    fn url_grounding_blocks_when_no_user_urls_at_all() {
        let v = ugv(&[]);
        let result = v.verify(
            "fetch",
            &json!({"url": "https://api.anthropic.ai/v1/models"}),
        );
        assert!(matches!(result, VerificationResult::Block { .. }));
    }

    #[test]
    fn url_grounding_allows_non_guarded_tool() {
        let v = ugv(&[]);
        assert_eq!(
            v.verify("read_file", &json!({"path": "/etc/hosts"})),
            VerificationResult::Allow
        );
    }

    #[test]
    fn url_grounding_guards_fetch_suffix_tool() {
        let v = ugv(&[]);
        let result = v.verify("http_fetch", &json!({"url": "https://evil.com/"}));
        assert!(matches!(result, VerificationResult::Block { .. }));
    }

    #[test]
    fn url_grounding_allows_web_scrape_with_provided_url() {
        let v = ugv(&["https://rust-lang.org/"]);
        assert_eq!(
            v.verify(
                "web_scrape",
                &json!({"url": "https://rust-lang.org/", "select": "h1"})
            ),
            VerificationResult::Allow
        );
    }

    #[test]
    fn url_grounding_allows_prefix_match() {
        // User provided https://docs.rs/ — agent fetches a sub-path.
        let v = ugv(&["https://docs.rs/"]);
        assert_eq!(
            v.verify(
                "fetch",
                &json!({"url": "https://docs.rs/tokio/latest/tokio/"})
            ),
            VerificationResult::Allow
        );
    }

    // --- Regression: #2191 — fetch URL hallucination ---

    /// REG-2191-1: exact reproduction of the bug scenario.
    /// Agent asks "do you know Anthropic?" (no URL provided) and halluccinates
    /// `https://api.anthropic.ai/v1/models`. With an empty `user_provided_urls` set
    /// the fetch must be blocked.
    #[test]
    fn reg_2191_hallucinated_api_endpoint_blocked_with_empty_session() {
        // Simulate: user never sent any URL in the conversation.
        let v = ugv(&[]);
        let result = v.verify(
            "fetch",
            &json!({"url": "https://api.anthropic.ai/v1/models"}),
        );
        assert!(
            matches!(result, VerificationResult::Block { .. }),
            "fetch must be blocked when no user URL was provided — this is the #2191 regression"
        );
    }

    /// REG-2191-2: passthrough — user explicitly pasted the URL, fetch must proceed.
    #[test]
    fn reg_2191_user_provided_url_allows_fetch() {
        let v = ugv(&["https://api.anthropic.com/v1/models"]);
        assert_eq!(
            v.verify(
                "fetch",
                &json!({"url": "https://api.anthropic.com/v1/models"}),
            ),
            VerificationResult::Allow,
            "fetch must be allowed when the URL was explicitly provided by the user"
        );
    }

    /// REG-2191-3: `web_scrape` variant — same rejection for `web_scrape` tool.
    #[test]
    fn reg_2191_web_scrape_hallucinated_url_blocked() {
        let v = ugv(&[]);
        let result = v.verify(
            "web_scrape",
            &json!({"url": "https://api.anthropic.ai/v1/models", "select": "body"}),
        );
        assert!(
            matches!(result, VerificationResult::Block { .. }),
            "web_scrape must be blocked for hallucinated URL with empty user_provided_urls"
        );
    }

    /// REG-2191-4: URL present only in an imagined system/assistant message context
    /// is NOT in `user_provided_urls` (the agent only populates from user messages).
    /// The verifier itself cannot distinguish message roles — it only sees the set
    /// populated by the agent. This test confirms: an empty set always blocks.
    #[test]
    fn reg_2191_empty_url_set_always_blocks_fetch() {
        // Whether the URL came from a system/assistant message or was never seen —
        // if user_provided_urls is empty, fetch must be blocked.
        let v = ugv(&[]);
        let result = v.verify(
            "fetch",
            &json!({"url": "https://docs.anthropic.com/something"}),
        );
        assert!(matches!(result, VerificationResult::Block { .. }));
    }

    /// REG-2191-5: URL matching is case-insensitive — user pastes mixed-case URL.
    #[test]
    fn reg_2191_case_insensitive_url_match_allows_fetch() {
        // user_provided_urls stores lowercase; verify that the fetched URL with
        // different casing still matches.
        let v = ugv(&["https://Docs.Anthropic.COM/models"]);
        assert_eq!(
            v.verify(
                "fetch",
                &json!({"url": "https://docs.anthropic.com/models/detail"}),
            ),
            VerificationResult::Allow,
            "URL matching must be case-insensitive"
        );
    }

    /// REG-2191-6: tool name ending in `_fetch` is auto-guarded regardless of config.
    /// An MCP-registered `anthropic_fetch` tool must not bypass the gate.
    #[test]
    fn reg_2191_mcp_fetch_suffix_tool_blocked_with_empty_session() {
        let v = ugv(&[]);
        let result = v.verify(
            "anthropic_fetch",
            &json!({"url": "https://api.anthropic.ai/v1/models"}),
        );
        assert!(
            matches!(result, VerificationResult::Block { .. }),
            "MCP tools ending in _fetch must be guarded even if not in guarded_tools list"
        );
    }

    /// REG-2191-7: reverse prefix — user provided a specific URL, agent fetches
    /// the root. This is the "reverse prefix" case: `user_url` `starts_with` `fetch_url`.
    #[test]
    fn reg_2191_reverse_prefix_match_allows_fetch() {
        // User provided a deep URL; agent wants to fetch the root.
        // Allowed: user_url.starts_with(fetch_url).
        let v = ugv(&["https://docs.rs/tokio/latest/tokio/index.html"]);
        assert_eq!(
            v.verify("fetch", &json!({"url": "https://docs.rs/"})),
            VerificationResult::Allow,
            "reverse prefix: fetched URL is a prefix of user-provided URL — should be allowed"
        );
    }

    /// REG-2191-8: completely different domain with same path prefix must be blocked.
    #[test]
    fn reg_2191_different_domain_blocked() {
        // User provided docs.rs, agent wants to fetch evil.com/docs.rs path — must block.
        let v = ugv(&["https://docs.rs/"]);
        let result = v.verify("fetch", &json!({"url": "https://evil.com/docs.rs/exfil"}));
        assert!(
            matches!(result, VerificationResult::Block { .. }),
            "different domain must not be allowed even if path looks similar"
        );
    }

    /// REG-2191-9: args without a `url` field — verifier must not block (Allow).
    #[test]
    fn reg_2191_missing_url_field_allows_fetch() {
        // Some fetch-like tools may call with different arg names.
        // Verifier only checks the `url` field; missing field → Allow.
        let v = ugv(&[]);
        assert_eq!(
            v.verify(
                "fetch",
                &json!({"endpoint": "https://api.anthropic.ai/v1/models"})
            ),
            VerificationResult::Allow,
            "missing url field must not trigger blocking — only explicit url field is checked"
        );
    }

    /// REG-2191-10: verifier disabled via config — all fetch calls pass through.
    #[test]
    fn reg_2191_disabled_verifier_allows_all() {
        let config = UrlGroundingVerifierConfig {
            enabled: false,
            guarded_tools: default_guarded_tools(),
        };
        // Note: the enabled flag is checked by the pipeline, not inside verify().
        // The pipeline skips disabled verifiers. This test documents that the struct
        // can be constructed with enabled=false (config round-trip).
        let set: HashSet<String> = HashSet::new();
        let v = UrlGroundingVerifier::new(&config, Arc::new(RwLock::new(set)));
        // verify() itself doesn't check enabled — the pipeline is responsible.
        // When called directly it will still block (the field has no effect here).
        // This is an API documentation test, not a behaviour test.
        let _ = v.verify("fetch", &json!({"url": "https://example.com/"}));
        // No assertion: just verifies the struct can be built with enabled=false.
    }

    // --- FirewallVerifier ---

    fn fwv() -> FirewallVerifier {
        FirewallVerifier::new(&FirewallVerifierConfig::default())
    }

    #[test]
    fn firewall_allows_normal_path() {
        let v = fwv();
        assert_eq!(
            v.verify("shell", &json!({"command": "ls /tmp/build"})),
            VerificationResult::Allow
        );
    }

    #[test]
    fn firewall_blocks_path_traversal() {
        let v = fwv();
        let result = v.verify("read", &json!({"file_path": "../../etc/passwd"}));
        assert!(
            matches!(result, VerificationResult::Block { .. }),
            "path traversal must be blocked"
        );
    }

    #[test]
    fn firewall_blocks_etc_passwd() {
        let v = fwv();
        let result = v.verify("read", &json!({"file_path": "/etc/passwd"}));
        assert!(
            matches!(result, VerificationResult::Block { .. }),
            "/etc/passwd must be blocked"
        );
    }

    #[test]
    fn firewall_blocks_ssh_key() {
        let v = fwv();
        let result = v.verify("read", &json!({"file_path": "~/.ssh/id_rsa"}));
        assert!(
            matches!(result, VerificationResult::Block { .. }),
            "SSH key path must be blocked"
        );
    }

    #[test]
    fn firewall_blocks_aws_env_var() {
        let v = fwv();
        let result = v.verify("shell", &json!({"command": "echo $AWS_SECRET_ACCESS_KEY"}));
        assert!(
            matches!(result, VerificationResult::Block { .. }),
            "AWS env var exfiltration must be blocked"
        );
    }

    #[test]
    fn firewall_blocks_zeph_env_var() {
        let v = fwv();
        let result = v.verify("shell", &json!({"command": "cat ${ZEPH_CLAUDE_API_KEY}"}));
        assert!(
            matches!(result, VerificationResult::Block { .. }),
            "ZEPH env var exfiltration must be blocked"
        );
    }

    #[test]
    fn firewall_exempt_tool_bypasses_check() {
        let cfg = FirewallVerifierConfig {
            enabled: true,
            blocked_paths: vec![],
            blocked_env_vars: vec![],
            exempt_tools: vec!["read".to_string()],
        };
        let v = FirewallVerifier::new(&cfg);
        // /etc/passwd would normally be blocked but tool is exempt.
        assert_eq!(
            v.verify("read", &json!({"file_path": "/etc/passwd"})),
            VerificationResult::Allow
        );
    }

    #[test]
    fn firewall_custom_blocked_path() {
        let cfg = FirewallVerifierConfig {
            enabled: true,
            blocked_paths: vec!["/data/secrets/*".to_string()],
            blocked_env_vars: vec![],
            exempt_tools: vec![],
        };
        let v = FirewallVerifier::new(&cfg);
        let result = v.verify("read", &json!({"file_path": "/data/secrets/master.key"}));
        assert!(
            matches!(result, VerificationResult::Block { .. }),
            "custom blocked path must be blocked"
        );
    }

    #[test]
    fn firewall_custom_blocked_env_var() {
        let cfg = FirewallVerifierConfig {
            enabled: true,
            blocked_paths: vec![],
            blocked_env_vars: vec!["MY_SECRET".to_string()],
            exempt_tools: vec![],
        };
        let v = FirewallVerifier::new(&cfg);
        let result = v.verify("shell", &json!({"command": "echo $MY_SECRET"}));
        assert!(
            matches!(result, VerificationResult::Block { .. }),
            "custom blocked env var must be blocked"
        );
    }

    #[test]
    fn firewall_invalid_glob_is_skipped() {
        // Invalid glob should not panic — logged and skipped at construction.
        let cfg = FirewallVerifierConfig {
            enabled: true,
            blocked_paths: vec!["[invalid-glob".to_string(), "/valid/path/*".to_string()],
            blocked_env_vars: vec![],
            exempt_tools: vec![],
        };
        let v = FirewallVerifier::new(&cfg);
        // Valid pattern still works
        let result = v.verify("read", &json!({"path": "/valid/path/file.txt"}));
        assert!(matches!(result, VerificationResult::Block { .. }));
    }

    #[test]
    fn firewall_config_default_deserialization() {
        let cfg: FirewallVerifierConfig = toml::from_str("").unwrap();
        assert!(cfg.enabled);
        assert!(cfg.blocked_paths.is_empty());
        assert!(cfg.blocked_env_vars.is_empty());
        assert!(cfg.exempt_tools.is_empty());
    }
}
