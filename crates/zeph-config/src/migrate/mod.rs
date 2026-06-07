// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Config migration: add missing parameters from the canonical reference as commented-out entries.
//!
//! The canonical reference is the checked-in `config/default.toml` file embedded at compile time.
//! Missing sections and keys are added as `# key = default_value` comments so users can discover
//! and enable them without hunting through documentation.

use regex::Regex;
use toml_edit::{Array, DocumentMut, Item, Table, Value};

// ── Submodules: migration steps grouped by subsystem (#4874) ─────────────────────────────────────
mod features;
mod infra;
mod llm;
mod mcp;
mod memory;
mod session;
mod tools;

pub use features::*;
pub use infra::*;
/// Advisory `GonkaGate` migration is crate-internal (registered via the [`MIGRATIONS`] registry).
pub(crate) use llm::migrate_gonkagate_to_gonka;
pub use llm::*;
pub use mcp::*;
pub use memory::*;
pub use session::*;
pub use tools::*;

/// Returns `true` when `name` is an active (non-commented) TOML section header in `src`.
///
/// Correctly handles:
/// - Exact bare header: `[name]` on its own line.
/// - Inline comment: `[name] # remark` — header is active.
/// - Implicit subtable parent: `[name.foo]` implies `[name]` is active.
/// - Commented header: `# [name]` — returns `false`.
///
/// # Panics
///
/// Never panics in practice — [`regex::escape`] always produces a valid pattern.
#[must_use]
pub fn section_header_present(src: &str, name: &str) -> bool {
    // Escape the name for use in a regex pattern.
    let escaped = regex::escape(name);
    // Matches `[name]` or `[name.anything]`, optionally followed by whitespace/comment.
    // Applied to trimmed lines after filtering out lines starting with `#`.
    let pattern = format!(r"^\[{escaped}(?:\.[^\]]+)?\](?:\s*#.*)?$");
    let re = Regex::new(&pattern).expect("regex::escape always produces a valid pattern");
    src.lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .any(|line| re.is_match(line.trim()))
}

/// Canonical section ordering for top-level keys in the output document.
static CANONICAL_ORDER: &[&str] = &[
    "agent",
    "llm",
    "skills",
    "memory",
    "index",
    "tools",
    "mcp",
    "telegram",
    "discord",
    "slack",
    "a2a",
    "acp",
    "gateway",
    "metrics",
    "daemon",
    "scheduler",
    "orchestration",
    "classifiers",
    "security",
    "vault",
    "timeouts",
    "cost",
    "debug",
    "logging",
    "notifications",
    "tui",
    "agents",
    "experiments",
    "lsp",
    "telemetry",
    "session",
];

/// Error type for migration failures.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum MigrateError {
    /// Failed to parse the user's config.
    #[error("failed to parse input config: {0}")]
    Parse(#[from] toml_edit::TomlError),
    /// Failed to parse the embedded reference config (should never happen in practice).
    #[error("failed to parse reference config: {0}")]
    Reference(toml_edit::TomlError),
    /// The document structure is inconsistent (e.g. `[llm.stt].model` exists but `[llm]` table
    /// cannot be obtained as a mutable table — can happen when `[llm]` is absent or not a table).
    #[error("migration failed: invalid TOML structure — {0}")]
    InvalidStructure(&'static str),
}

/// Result of a migration operation.
#[derive(Debug)]
pub struct MigrationResult {
    /// The migrated TOML document as a string.
    pub output: String,
    /// Number of top-level keys or sub-keys modified (added or removed) during migration.
    pub changed_count: usize,
    /// Names of top-level sections that were modified (added or removed).
    pub sections_changed: Vec<String>,
}

/// Migrates a user config by adding missing parameters as commented-out entries.
///
/// The canonical reference is embedded from `config/default.toml` at compile time.
/// User values are never modified; only missing keys are appended as comments.
pub struct ConfigMigrator {
    reference_src: &'static str,
}

impl Default for ConfigMigrator {
    fn default() -> Self {
        Self::new()
    }
}

impl ConfigMigrator {
    /// Create a new migrator using the embedded canonical reference config.
    #[must_use]
    pub fn new() -> Self {
        Self {
            reference_src: include_str!("../../config/default.toml"),
        }
    }

    /// Migrate `user_toml`: add missing parameters from the reference as commented-out entries.
    ///
    /// # Errors
    ///
    /// Returns `MigrateError::Parse` if the user's TOML is invalid.
    /// Returns `MigrateError::Reference` if the embedded reference TOML cannot be parsed.
    ///
    /// # Panics
    ///
    /// Never panics in practice; `.expect("checked")` is unreachable because `is_table()` is
    /// verified on the same `ref_item` immediately before calling `as_table()`.
    pub fn migrate(&self, user_toml: &str) -> Result<MigrationResult, MigrateError> {
        let reference_doc = self
            .reference_src
            .parse::<DocumentMut>()
            .map_err(MigrateError::Reference)?;
        let mut user_doc = user_toml.parse::<DocumentMut>()?;

        let mut changed_count = 0usize;
        let mut sections_changed: Vec<String> = Vec::new();
        // Collected scalar/sub-table comment lines to insert after rendering.
        // Each entry: (section_key, comment_line).
        let mut pending_comments: Vec<(String, String)> = Vec::new();

        // Walk the reference top-level keys.
        for (key, ref_item) in reference_doc.as_table() {
            if ref_item.is_table() {
                let ref_table = ref_item.as_table().expect("is_table checked above");
                if user_doc.contains_key(key) {
                    // Section exists — merge missing sub-keys.
                    if let Some(user_table) = user_doc.get_mut(key).and_then(Item::as_table_mut) {
                        let (n, comments) =
                            merge_table_commented(user_table, ref_table, key, user_toml);
                        changed_count += n;
                        pending_comments.extend(comments);
                    }
                } else {
                    // Entire section is missing — record for textual append after rendering.
                    // Idempotency: skip if a commented block for this section was already appended.
                    if user_toml.contains(&format!("# [{key}]")) {
                        continue;
                    }
                    let commented = commented_table_block(key, ref_table);
                    if !commented.is_empty() {
                        sections_changed.push(key.to_owned());
                    }
                    changed_count += 1;
                }
            } else {
                // Top-level scalar/array key.
                if !user_doc.contains_key(key) {
                    let raw = format_commented_item(key, ref_item);
                    if !raw.is_empty() {
                        sections_changed.push(format!("__scalar__{key}"));
                        changed_count += 1;
                    }
                }
            }
        }

        // Render the user doc as-is first.
        let user_str = user_doc.to_string();

        // Insert collected scalar/sub-table comment lines via raw text operations.
        // This avoids toml_edit decor roundtrip loss — guards check the rendered string.
        let mut output = user_str;
        for (section_key, comment_line) in &pending_comments {
            if !section_body(&output, section_key).contains(comment_line.trim()) {
                output = insert_after_section(&output, section_key, comment_line);
            }
        }

        // Append missing sections as raw commented text at the end.
        for key in &sections_changed {
            if let Some(scalar_key) = key.strip_prefix("__scalar__") {
                if let Some(ref_item) = reference_doc.get(scalar_key) {
                    let raw = format_commented_item(scalar_key, ref_item);
                    if !raw.is_empty() {
                        output.push('\n');
                        output.push_str(&raw);
                        output.push('\n');
                    }
                }
            } else if let Some(ref_table) = reference_doc.get(key.as_str()).and_then(Item::as_table)
            {
                let block = commented_table_block(key, ref_table);
                if !block.is_empty() {
                    output.push('\n');
                    output.push_str(&block);
                }
            }
        }

        // Reorder top-level sections by canonical order.
        output = reorder_sections(&output, CANONICAL_ORDER);

        // Resolve sections_changed to only real section names (not scalars).
        let sections_changed_clean: Vec<String> = sections_changed
            .into_iter()
            .filter(|k| !k.starts_with("__scalar__"))
            .collect();

        Ok(MigrationResult {
            output,
            changed_count,
            sections_changed: sections_changed_clean,
        })
    }
}

/// Merge missing keys from `ref_table` into `user_table` as commented-out entries.
///
/// Returns `(count, comment_lines)` where `comment_lines` is a list of
/// `(section_key, comment_line)` pairs to be inserted into the rendered output.
/// Using raw-string insertion avoids `toml_edit` decor roundtrip loss.
fn merge_table_commented(
    user_table: &mut Table,
    ref_table: &Table,
    section_key: &str,
    user_toml: &str,
) -> (usize, Vec<(String, String)>) {
    let mut count = 0usize;
    let mut comments: Vec<(String, String)> = Vec::new();
    for (key, ref_item) in ref_table {
        if ref_item.is_table() {
            if user_table.contains_key(key) {
                let pair = (
                    user_table.get_mut(key).and_then(Item::as_table_mut),
                    ref_item.as_table(),
                );
                if let (Some(user_sub_table), Some(ref_sub_table)) = pair {
                    let sub_key = format!("{section_key}.{key}");
                    let (n, c) =
                        merge_table_commented(user_sub_table, ref_sub_table, &sub_key, user_toml);
                    count += n;
                    comments.extend(c);
                }
            } else if let Some(ref_sub_table) = ref_item.as_table() {
                // Sub-table missing from user config — collect as raw commented block.
                let dotted = format!("{section_key}.{key}");
                let marker = format!("# [{dotted}]");
                if !user_toml.contains(&marker) {
                    let block = commented_table_block(&dotted, ref_sub_table);
                    if !block.is_empty() {
                        comments.push((section_key.to_owned(), format!("\n{block}")));
                        count += 1;
                    }
                }
            }
        } else if ref_item.is_array_of_tables() {
            // Never inject array-of-tables entries — they are user-defined.
        } else {
            // Scalar/array value — check if already present (as value or as comment).
            if !user_table.contains_key(key) {
                let raw_value = ref_item
                    .as_value()
                    .map(value_to_toml_string)
                    .unwrap_or_default();
                if !raw_value.is_empty() {
                    let comment_line = format!("# {key} = {raw_value}\n");
                    // Scope the guard to the target section body so that an identical key
                    // name in another section does not suppress this insertion.
                    if !section_body(user_toml, section_key).contains(comment_line.trim()) {
                        comments.push((section_key.to_owned(), comment_line));
                        count += 1;
                    }
                }
            }
        }
    }
    (count, comments)
}

/// Return the body of `[section]` in `doc` — the text between the section header line
/// and the next top-level `[...]` header (or end of document).
///
/// Used to scope idempotency guards to a single section so that a comment present in
/// one section does not suppress insertion into a different section with the same key name.
fn section_body<'a>(doc: &'a str, section: &str) -> &'a str {
    let header = format!("[{section}]");
    let Some(section_start) = doc.find(&header) else {
        return "";
    };
    let body_start = section_start + header.len();
    let body_end = doc[body_start..]
        .find("\n[")
        .map_or(doc.len(), |r| body_start + r);
    &doc[body_start..body_end]
}

/// Insert `text` after the last line belonging to `[section_name]` and before the next
/// top-level `[section]` header (or at the end of the file if no such header follows).
///
/// This is a purely textual operation: it does not parse TOML, making it immune to
/// `toml_edit` decor round-trip loss.
fn insert_after_section(raw: &str, section_name: &str, text: &str) -> String {
    let header = format!("[{section_name}]");
    let Some(section_start) = raw.find(&header) else {
        return format!("{raw}{text}");
    };
    // Find the next top-level section `[...]` after `section_start`.
    let search_from = section_start + header.len();
    // Look for `\n[` which signals a new top-level section.
    let insert_pos = raw[search_from..]
        .find("\n[")
        .map_or(raw.len(), |rel| search_from + rel + 1);
    let mut out = String::with_capacity(raw.len() + text.len());
    out.push_str(&raw[..insert_pos]);
    out.push_str(text);
    out.push_str(&raw[insert_pos..]);
    out
}

/// Format a reference item as a commented TOML line: `# key = value`.
fn format_commented_item(key: &str, item: &Item) -> String {
    if let Some(val) = item.as_value() {
        let raw = value_to_toml_string(val);
        if !raw.is_empty() {
            return format!("# {key} = {raw}\n");
        }
    }
    String::new()
}

/// Render a table as a commented-out TOML block with arbitrary nesting depth.
///
/// `section_name` is the full dotted path (e.g. `security.content_isolation`).
/// Returns an empty string if the table has no renderable content.
fn commented_table_block(section_name: &str, table: &Table) -> String {
    use std::fmt::Write as _;

    let mut lines = format!("# [{section_name}]\n");

    for (key, item) in table {
        if item.is_table() {
            if let Some(sub_table) = item.as_table() {
                let sub_name = format!("{section_name}.{key}");
                let sub_block = commented_table_block(&sub_name, sub_table);
                if !sub_block.is_empty() {
                    lines.push('\n');
                    lines.push_str(&sub_block);
                }
            }
        } else if item.is_array_of_tables() {
            // Skip — user configures these manually (e.g. `[[mcp.servers]]`).
        } else if let Some(val) = item.as_value() {
            let raw = value_to_toml_string(val);
            if !raw.is_empty() {
                let _ = writeln!(lines, "# {key} = {raw}");
            }
        }
    }

    // Return empty if we only wrote the section header with no content.
    if lines.trim() == format!("[{section_name}]") {
        return String::new();
    }
    lines
}

/// Convert a `toml_edit::Value` to its TOML string representation.
fn value_to_toml_string(val: &Value) -> String {
    match val {
        Value::String(s) => {
            let inner = s.value();
            format!("\"{inner}\"")
        }
        Value::Integer(i) => i.value().to_string(),
        Value::Float(f) => {
            let v = f.value();
            // Use representation that round-trips exactly.
            if v.fract() == 0.0 {
                format!("{v:.1}")
            } else {
                format!("{v}")
            }
        }
        Value::Boolean(b) => b.value().to_string(),
        Value::Array(arr) => format_array(arr),
        Value::InlineTable(t) => {
            let pairs: Vec<String> = t
                .iter()
                .map(|(k, v)| format!("{k} = {}", value_to_toml_string(v)))
                .collect();
            format!("{{ {} }}", pairs.join(", "))
        }
        Value::Datetime(dt) => dt.value().to_string(),
    }
}

fn format_array(arr: &Array) -> String {
    if arr.is_empty() {
        return "[]".to_owned();
    }
    let items: Vec<String> = arr.iter().map(value_to_toml_string).collect();
    format!("[{}]", items.join(", "))
}

/// Reorder top-level sections of a TOML document string by the canonical order.
///
/// Sections not in the canonical list are placed at the end, preserving their relative order.
/// This operates on the raw string rather than the parsed document to preserve comments that
/// would otherwise be dropped by `toml_edit`'s round-trip.
fn reorder_sections(toml_str: &str, canonical_order: &[&str]) -> String {
    let sections = split_into_sections(toml_str);
    if sections.is_empty() {
        return toml_str.to_owned();
    }

    // Each entry is (header, content). Empty header = preamble block.
    let preamble_block = sections
        .iter()
        .find(|(h, _)| h.is_empty())
        .map_or("", |(_, c)| c.as_str());

    let section_map: Vec<(&str, &str)> = sections
        .iter()
        .filter(|(h, _)| !h.is_empty())
        .map(|(h, c)| (h.as_str(), c.as_str()))
        .collect();

    let mut out = String::new();
    if !preamble_block.is_empty() {
        out.push_str(preamble_block);
    }

    let mut emitted: Vec<bool> = vec![false; section_map.len()];

    for &canon in canonical_order {
        for (idx, &(header, content)) in section_map.iter().enumerate() {
            let section_name = extract_section_name(header);
            let top_level = section_name
                .split('.')
                .next()
                .unwrap_or("")
                .trim_start_matches('#')
                .trim();
            if top_level == canon && !emitted[idx] {
                out.push_str(content);
                emitted[idx] = true;
            }
        }
    }

    // Append sections not in canonical order.
    for (idx, &(_, content)) in section_map.iter().enumerate() {
        if !emitted[idx] {
            out.push_str(content);
        }
    }

    out
}

/// Extract the section name from a section header line (e.g. `[agent]` → `agent`).
fn extract_section_name(header: &str) -> &str {
    // Strip leading `# ` for commented headers.
    let trimmed = header.trim().trim_start_matches("# ");
    // Strip `[` and `]`.
    if trimmed.starts_with('[') && trimmed.contains(']') {
        let inner = &trimmed[1..];
        if let Some(end) = inner.find(']') {
            return &inner[..end];
        }
    }
    trimmed
}

/// Split a TOML string into `(header_line, full_block)` pairs.
///
/// The first element may have an empty header representing the preamble.
fn split_into_sections(toml_str: &str) -> Vec<(String, String)> {
    let mut sections: Vec<(String, String)> = Vec::new();
    let mut current_header = String::new();
    let mut current_content = String::new();

    for line in toml_str.lines() {
        let trimmed = line.trim();
        if is_top_level_section_header(trimmed) {
            sections.push((current_header.clone(), current_content.clone()));
            trimmed.clone_into(&mut current_header);
            line.clone_into(&mut current_content);
            current_content.push('\n');
        } else {
            current_content.push_str(line);
            current_content.push('\n');
        }
    }

    // Push the last section.
    if !current_header.is_empty() || !current_content.is_empty() {
        sections.push((current_header, current_content));
    }

    sections
}

/// Determine if a line is a real (non-commented) top-level section header.
///
/// Top-level means `[name]` with no dots. Commented headers like `# [name]`
/// are NOT treated as section boundaries — they are migrator-generated hints.
fn is_top_level_section_header(line: &str) -> bool {
    if line.starts_with('[')
        && !line.starts_with("[[")
        && let Some(end) = line.find(']')
    {
        return !line[1..end].contains('.');
    }
    false
}

/// A single idempotent config migration step.
///
/// Each impl wraps one of the free-standing `migrate_*` functions and gives it a stable
/// name used in logs and test assertions. The trait is object-safe so that steps can be
/// stored in a `Vec<Box<dyn Migration + Send + Sync>>`.
///
/// # Contract for implementors
///
/// - `apply` **must** be idempotent: calling it twice on the same source must return the
///   same output as calling it once.
/// - On a no-op (nothing to migrate), `apply` returns a [`MigrationResult`] with
///   `changed_count == 0`.
///
/// # Examples
///
/// ```rust
/// use zeph_config::migrate::{Migration, MIGRATIONS};
///
/// // The registry is ordered chronologically; apply each step in sequence.
/// let mut toml = "[agent]\nname = \"zeph\"\n".to_owned();
/// for m in MIGRATIONS.iter() {
///     toml = m.apply(&toml).expect("migration failed").output;
/// }
/// ```
pub trait Migration: Send + Sync {
    /// Human-readable identifier used in diagnostics and ordering assertions.
    fn name(&self) -> &'static str;

    /// Apply this migration step to `toml_src`.
    ///
    /// # Errors
    ///
    /// Propagates any [`MigrateError`] from the underlying free function.
    fn apply(&self, toml_src: &str) -> Result<MigrationResult, MigrateError>;
}

mod steps;
use steps::{
    MigrateAcpSubagentsConfig, MigrateAgentBudgetHint, MigrateAgentRetryToToolsRetry,
    MigrateAutodreamConfig, MigrateCocoonProviderNotice, MigrateCocoonShowBalance,
    MigrateCompressionPredictorConfig, MigrateDatabaseUrl, MigrateDurableConfig,
    MigrateEgressConfig, MigrateEmbedProviderRename, MigrateEvalModelToProvider,
    MigrateFidelityTimeoutDefaults, MigrateFiveSignalConfig, MigrateFocusAutoConsolidateMinWindow,
    MigrateForgettingConfig, MigrateGoalsConfig, MigrateGonkagateToGonka,
    MigrateHooksPermissionDeniedConfig, MigrateHooksTurnComplete, MigrateLlmStreamLimits,
    MigrateMagicDocsConfig, MigrateMcpElicitationConfig, MigrateMcpMaxConnectAttempts,
    MigrateMcpRetryAndToolTimeout, MigrateMcpTrustLevels, MigrateMemoryGraph, MigrateMemoryHebbian,
    MigrateMemoryHebbianConsolidation, MigrateMemoryHebbianSpread, MigrateMemoryPersonaConfig,
    MigrateMemoryReasoning, MigrateMemoryReasoningJudge, MigrateMemoryRetrieval,
    MigrateMemoryRetrievalQueryBias, MigrateMicrocompactConfig, MigrateOrchestrationPersistence,
    MigrateOrchestratorProvider, MigrateOtelFilter, MigratePlannerModelToProvider,
    MigrateProviderMaxConcurrent, MigrateQdrantApiKey, MigrateQualityConfig, MigrateSandboxConfig,
    MigrateSandboxEgressFilter, MigrateSchedulerDaemon, MigrateSessionPersistProviderOverrides,
    MigrateSessionProviderPersistence, MigrateSessionRecapConfig, MigrateShellTransactional,
    MigrateSttToProvider, MigrateSupervisorConfig, MigrateTelemetryConfig,
    MigrateToolsCompressionConfig, MigrateTraceMetadata, MigrateVigilConfig, MigrateWorktreeConfig,
    MigrateWorktreeGitTimeout,
};

/// Ordered registry of all sequential migration steps (steps 1–58).
///
/// Each entry wraps the corresponding free function and is evaluated lazily at first access.
/// The ordering is chronological; the dispatch loop in `src/commands/migrate.rs` iterates
/// this registry rather than calling free functions individually.
///
/// # Examples
///
/// ```rust
/// use zeph_config::migrate::MIGRATIONS;
///
/// // Every step in the registry has a non-empty name.
/// for m in MIGRATIONS.iter() {
///     assert!(!m.name().is_empty());
/// }
/// ```
pub static MIGRATIONS: std::sync::LazyLock<Vec<Box<dyn Migration + Send + Sync>>> =
    std::sync::LazyLock::new(|| {
        vec![
            // Steps 1–25 (pre-existing migrations)
            Box::new(MigrateSttToProvider) as Box<dyn Migration + Send + Sync>,
            Box::new(MigratePlannerModelToProvider),
            Box::new(MigrateMcpTrustLevels),
            Box::new(MigrateAgentRetryToToolsRetry),
            Box::new(MigrateDatabaseUrl),
            Box::new(MigrateShellTransactional),
            Box::new(MigrateAgentBudgetHint),
            Box::new(MigrateForgettingConfig),
            Box::new(MigrateCompressionPredictorConfig),
            Box::new(MigrateMicrocompactConfig),
            Box::new(MigrateAutodreamConfig),
            Box::new(MigrateMagicDocsConfig),
            Box::new(MigrateTelemetryConfig),
            Box::new(MigrateSupervisorConfig),
            Box::new(MigrateOtelFilter),
            Box::new(MigrateEgressConfig),
            Box::new(MigrateVigilConfig),
            Box::new(MigrateSandboxConfig),
            Box::new(MigrateSandboxEgressFilter),
            Box::new(MigrateOrchestrationPersistence),
            Box::new(MigrateSessionRecapConfig),
            Box::new(MigrateMcpElicitationConfig),
            Box::new(MigrateQualityConfig),
            Box::new(MigrateAcpSubagentsConfig),
            Box::new(MigrateHooksPermissionDeniedConfig),
            // Steps 26–35 (most recent migrations, pre-stable-defaults)
            Box::new(MigrateMemoryGraph),
            Box::new(MigrateSchedulerDaemon),
            Box::new(MigrateMemoryRetrieval),
            Box::new(MigrateMemoryReasoning),
            Box::new(MigrateMemoryReasoningJudge),
            Box::new(MigrateMemoryHebbian),
            Box::new(MigrateMemoryHebbianConsolidation),
            Box::new(MigrateMemoryHebbianSpread),
            Box::new(MigrateHooksTurnComplete),
            Box::new(MigrateFocusAutoConsolidateMinWindow),
            // Steps 36–38 (stable-defaults: flip verified-stable config keys to on)
            Box::new(MigrateSessionProviderPersistence),
            Box::new(MigrateMemoryRetrievalQueryBias),
            Box::new(MigrateMemoryPersonaConfig),
            // Step 39 — optional Qdrant API key (#3543)
            Box::new(MigrateQdrantApiKey),
            // Step 40 — MCP startup auto-retry max_connect_attempts (#3568)
            Box::new(MigrateMcpMaxConnectAttempts),
            // Steps 41–42 — goal lifecycle and TACO compression (#3567, #3306)
            Box::new(MigrateGoalsConfig),
            Box::new(MigrateToolsCompressionConfig),
            // Step 43 — orchestrator_provider for scheduling-tier LLM calls (#3300)
            Box::new(MigrateOrchestratorProvider),
            // Step 44 — max_concurrent per-provider admission control hint (#3299)
            Box::new(MigrateProviderMaxConcurrent),
            // Step 45 — advisory notice for GonkaGate → native Gonka upgrade path (#3613)
            Box::new(MigrateGonkagateToGonka),
            // Step 46 — advisory notice for Cocoon decentralized inference provider (#3671)
            Box::new(MigrateCocoonProviderNotice),
            // Step 47 — telemetry.trace_metadata OTEL resource attributes (#4160)
            Box::new(MigrateTraceMetadata),
            // Step 48 — five-signal SYNAPSE retrieval advisory (#4374)
            Box::new(MigrateFiveSignalConfig),
            // Step 49 — rename embed_provider → embedding_provider (#4480)
            Box::new(MigrateEmbedProviderRename),
            // Step 50 — add mcp startup_retry_backoff_ms and tool_timeout_secs (#4004)
            Box::new(MigrateMcpRetryAndToolTimeout),
            // Step 51 — add embed_timeout_secs and compress_timeout_secs to [memory.fidelity] (#4645, #4651)
            Box::new(MigrateFidelityTimeoutDefaults),
            // Step 52 — add persist_provider_overrides to [session] (#4654)
            Box::new(MigrateSessionPersistProviderOverrides),
            // Step 53 — add [cocoon] show_balance advisory notice (#4649)
            Box::new(MigrateCocoonShowBalance),
            // Step 54 — add [worktree] section with defaults (#4679)
            Box::new(MigrateWorktreeConfig),
            // Step 55 — add git_timeout_secs to [worktree] (#4704)
            Box::new(MigrateWorktreeGitTimeout),
            // Step 56 — add [llm.stream_limits] commented advisory notice (#4750)
            Box::new(MigrateLlmStreamLimits),
            // Step 57 — add [durable] execution-layer section, default-off (spec-064, #4949)
            Box::new(MigrateDurableConfig),
            // Step 58 — rename [experiments] eval_model → eval_provider (#4987)
            Box::new(MigrateEvalModelToProvider),
        ]
    });

// Helper to create a formatted value (used in tests).
#[cfg(test)]
fn make_formatted_str(s: &str) -> Value {
    use toml_edit::Formatted;
    Value::String(Formatted::new(s.to_owned()))
}

#[cfg(test)]
mod tests;
