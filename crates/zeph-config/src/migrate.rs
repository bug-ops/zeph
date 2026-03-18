// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Config migration: add missing parameters from the canonical reference as commented-out entries.
//!
//! The canonical reference is the checked-in `config/default.toml` file embedded at compile time.
//! Missing sections and keys are added as `# key = default_value` comments so users can discover
//! and enable them without hunting through documentation.

use toml_edit::{Array, DocumentMut, Item, RawString, Table, Value};

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
    "daemon",
    "scheduler",
    "orchestration",
    "security",
    "vault",
    "timeouts",
    "cost",
    "observability",
    "debug",
    "logging",
    "tui",
    "agents",
    "experiments",
    "lsp",
];

/// Error type for migration failures.
#[derive(Debug, thiserror::Error)]
pub enum MigrateError {
    /// Failed to parse the user's config.
    #[error("failed to parse input config: {0}")]
    Parse(#[from] toml_edit::TomlError),
    /// Failed to parse the embedded reference config (should never happen in practice).
    #[error("failed to parse reference config: {0}")]
    Reference(toml_edit::TomlError),
}

/// Result of a migration operation.
#[derive(Debug)]
pub struct MigrationResult {
    /// The migrated TOML document as a string.
    pub output: String,
    /// Number of top-level keys or sub-keys added as comments.
    pub added_count: usize,
    /// Names of top-level sections that were added.
    pub sections_added: Vec<String>,
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
            reference_src: include_str!("../config/default.toml"),
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

        let mut added_count = 0usize;
        let mut sections_added: Vec<String> = Vec::new();

        // Walk the reference top-level keys.
        for (key, ref_item) in reference_doc.as_table() {
            if ref_item.is_table() {
                let ref_table = ref_item.as_table().expect("is_table checked above");
                if user_doc.contains_key(key) {
                    // Section exists — merge missing sub-keys.
                    if let Some(user_table) = user_doc.get_mut(key).and_then(Item::as_table_mut) {
                        added_count += merge_table_commented(user_table, ref_table, key);
                    }
                } else {
                    // Entire section is missing — record for textual append after rendering.
                    // Idempotency: skip if a commented block for this section was already appended.
                    if user_toml.contains(&format!("# [{key}]")) {
                        continue;
                    }
                    let commented = commented_table_block(key, ref_table);
                    if !commented.is_empty() {
                        sections_added.push(key.to_owned());
                    }
                    added_count += 1;
                }
            } else {
                // Top-level scalar/array key.
                if !user_doc.contains_key(key) {
                    let raw = format_commented_item(key, ref_item);
                    if !raw.is_empty() {
                        sections_added.push(format!("__scalar__{key}"));
                        added_count += 1;
                    }
                }
            }
        }

        // Render the user doc as-is first.
        let user_str = user_doc.to_string();

        // Append missing sections as raw commented text at the end.
        let mut output = user_str;
        for key in &sections_added {
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

        // Resolve sections_added to only real section names (not scalars).
        let sections_added_clean: Vec<String> = sections_added
            .into_iter()
            .filter(|k| !k.starts_with("__scalar__"))
            .collect();

        Ok(MigrationResult {
            output,
            added_count,
            sections_added: sections_added_clean,
        })
    }
}

/// Merge missing keys from `ref_table` into `user_table` as commented-out entries.
///
/// Returns the number of keys added.
fn merge_table_commented(user_table: &mut Table, ref_table: &Table, section_key: &str) -> usize {
    let mut count = 0usize;
    for (key, ref_item) in ref_table {
        if ref_item.is_table() {
            if user_table.contains_key(key) {
                let pair = (
                    user_table.get_mut(key).and_then(Item::as_table_mut),
                    ref_item.as_table(),
                );
                if let (Some(user_sub_table), Some(ref_sub_table)) = pair {
                    let sub_key = format!("{section_key}.{key}");
                    count += merge_table_commented(user_sub_table, ref_sub_table, &sub_key);
                }
            } else if let Some(ref_sub_table) = ref_item.as_table() {
                // Sub-table missing from user config — append as commented block.
                let dotted = format!("{section_key}.{key}");
                let marker = format!("# [{dotted}]");
                let existing = user_table
                    .decor()
                    .suffix()
                    .and_then(RawString::as_str)
                    .unwrap_or("");
                if !existing.contains(&marker) {
                    let block = commented_table_block(&dotted, ref_sub_table);
                    if !block.is_empty() {
                        let new_suffix = format!("{existing}\n{block}");
                        user_table.decor_mut().set_suffix(new_suffix);
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
                    append_comment_to_table_suffix(user_table, &comment_line);
                    count += 1;
                }
            }
        }
    }
    count
}

/// Append a comment line to a table's trailing whitespace/decor.
fn append_comment_to_table_suffix(table: &mut Table, comment_line: &str) {
    let existing: String = table
        .decor()
        .suffix()
        .and_then(RawString::as_str)
        .unwrap_or("")
        .to_owned();
    // Only append if this exact comment_line is not already present (idempotency).
    if !existing.contains(comment_line.trim()) {
        let new_suffix = format!("{existing}{comment_line}");
        table.decor_mut().set_suffix(new_suffix);
    }
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

// Helper to create a formatted value (used in tests).
#[cfg(test)]
fn make_formatted_str(s: &str) -> Value {
    use toml_edit::Formatted;
    Value::String(Formatted::new(s.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_config_gets_sections_as_comments() {
        let migrator = ConfigMigrator::new();
        let result = migrator.migrate("").expect("migrate empty");
        // Should have added sections since reference is non-empty.
        assert!(result.added_count > 0 || !result.sections_added.is_empty());
        // Output should mention at least agent section.
        assert!(
            result.output.contains("[agent]") || result.output.contains("# [agent]"),
            "expected agent section in output, got:\n{}",
            result.output
        );
    }

    #[test]
    fn existing_values_not_overwritten() {
        let user = r#"
[agent]
name = "MyAgent"
max_tool_iterations = 5
"#;
        let migrator = ConfigMigrator::new();
        let result = migrator.migrate(user).expect("migrate");
        // Original name preserved.
        assert!(
            result.output.contains("name = \"MyAgent\""),
            "user value should be preserved"
        );
        assert!(
            result.output.contains("max_tool_iterations = 5"),
            "user value should be preserved"
        );
        // Should not appear as commented default.
        assert!(
            !result.output.contains("# max_tool_iterations = 10"),
            "already-set key should not appear as comment"
        );
    }

    #[test]
    fn missing_nested_key_added_as_comment() {
        // User has [memory] but is missing some keys.
        let user = r#"
[memory]
sqlite_path = ".zeph/data/zeph.db"
"#;
        let migrator = ConfigMigrator::new();
        let result = migrator.migrate(user).expect("migrate");
        // history_limit should be added as comment since it's in reference.
        assert!(
            result.output.contains("# history_limit"),
            "missing key should be added as comment, got:\n{}",
            result.output
        );
    }

    #[test]
    fn unknown_user_keys_preserved() {
        let user = r#"
[agent]
name = "Test"
my_custom_key = "preserved"
"#;
        let migrator = ConfigMigrator::new();
        let result = migrator.migrate(user).expect("migrate");
        assert!(
            result.output.contains("my_custom_key = \"preserved\""),
            "custom user keys must not be removed"
        );
    }

    #[test]
    fn idempotent() {
        let migrator = ConfigMigrator::new();
        let first = migrator
            .migrate("[agent]\nname = \"Zeph\"\n")
            .expect("first migrate");
        let second = migrator.migrate(&first.output).expect("second migrate");
        assert_eq!(
            first.output, second.output,
            "idempotent: full output must be identical on second run"
        );
    }

    #[test]
    fn malformed_input_returns_error() {
        let migrator = ConfigMigrator::new();
        let err = migrator
            .migrate("[[invalid toml [[[")
            .expect_err("should error");
        assert!(
            matches!(err, MigrateError::Parse(_)),
            "expected Parse error"
        );
    }

    #[test]
    fn array_of_tables_preserved() {
        let user = r#"
[mcp]
allowed_commands = ["npx"]

[[mcp.servers]]
id = "my-server"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
"#;
        let migrator = ConfigMigrator::new();
        let result = migrator.migrate(user).expect("migrate");
        // User's [[mcp.servers]] entry must survive.
        assert!(
            result.output.contains("[[mcp.servers]]"),
            "array-of-tables entries must be preserved"
        );
        assert!(result.output.contains("id = \"my-server\""));
    }

    #[test]
    fn canonical_ordering_applied() {
        // Put memory before agent intentionally.
        let user = r#"
[memory]
sqlite_path = ".zeph/data/zeph.db"

[agent]
name = "Test"
"#;
        let migrator = ConfigMigrator::new();
        let result = migrator.migrate(user).expect("migrate");
        // agent should appear before memory in canonical order.
        let agent_pos = result.output.find("[agent]");
        let memory_pos = result.output.find("[memory]");
        if let (Some(a), Some(m)) = (agent_pos, memory_pos) {
            assert!(a < m, "agent section should precede memory section");
        }
    }

    #[test]
    fn value_to_toml_string_formats_correctly() {
        use toml_edit::Formatted;

        let s = make_formatted_str("hello");
        assert_eq!(value_to_toml_string(&s), "\"hello\"");

        let i = Value::Integer(Formatted::new(42_i64));
        assert_eq!(value_to_toml_string(&i), "42");

        let b = Value::Boolean(Formatted::new(true));
        assert_eq!(value_to_toml_string(&b), "true");

        let f = Value::Float(Formatted::new(1.0_f64));
        assert_eq!(value_to_toml_string(&f), "1.0");

        let f2 = Value::Float(Formatted::new(157_f64 / 50.0));
        assert_eq!(value_to_toml_string(&f2), "3.14");

        let arr: Array = ["a", "b"].iter().map(|s| make_formatted_str(s)).collect();
        let arr_val = Value::Array(arr);
        assert_eq!(value_to_toml_string(&arr_val), r#"["a", "b"]"#);

        let empty_arr = Value::Array(Array::new());
        assert_eq!(value_to_toml_string(&empty_arr), "[]");
    }

    #[test]
    fn idempotent_full_output_unchanged() {
        // Stronger idempotency: the entire output string must not change on a second pass.
        let migrator = ConfigMigrator::new();
        let first = migrator
            .migrate("[agent]\nname = \"Zeph\"\n")
            .expect("first migrate");
        let second = migrator.migrate(&first.output).expect("second migrate");
        assert_eq!(
            first.output, second.output,
            "full output string must be identical after second migration pass"
        );
    }

    #[test]
    fn full_config_produces_zero_additions() {
        // Migrating the reference config itself should add nothing new.
        let reference = include_str!("../config/default.toml");
        let migrator = ConfigMigrator::new();
        let result = migrator.migrate(reference).expect("migrate reference");
        assert_eq!(
            result.added_count, 0,
            "migrating the canonical reference should add nothing (added_count = {})",
            result.added_count
        );
        assert!(
            result.sections_added.is_empty(),
            "migrating the canonical reference should report no sections_added: {:?}",
            result.sections_added
        );
    }

    #[test]
    fn empty_config_added_count_is_positive() {
        // Stricter variant of empty_config_gets_sections_as_comments.
        let migrator = ConfigMigrator::new();
        let result = migrator.migrate("").expect("migrate empty");
        assert!(
            result.added_count > 0,
            "empty config must report added_count > 0"
        );
    }

    // IMPL-04: verify that [security.guardrail] is injected as commented defaults
    // for a pre-guardrail config that has [security] but no [security.guardrail].
    #[test]
    fn security_without_guardrail_gets_guardrail_commented() {
        let user = r#"
[security]
redact_secrets = true
"#;
        let migrator = ConfigMigrator::new();
        let result = migrator.migrate(user).expect("migrate");
        // The generic diff mechanism must add guardrail keys as commented defaults.
        assert!(
            result.output.contains("guardrail"),
            "migration must add guardrail keys for configs without [security.guardrail]: \
             got:\n{}",
            result.output
        );
    }

    #[test]
    fn migrate_reference_contains_tools_policy() {
        // IMP-NO-MIGRATE-CONFIG: verify that the embedded default.toml (the canonical reference
        // used by ConfigMigrator) contains a [tools.policy] section. This ensures that
        // `zeph --migrate-config` will surface the section to users as a discoverable commented
        // block, even if it cannot be injected as a live sub-table via toml_edit's round-trip.
        let reference = include_str!("../config/default.toml");
        assert!(
            reference.contains("[tools.policy]"),
            "default.toml must contain [tools.policy] section so migrate-config can surface it"
        );
        assert!(
            reference.contains("enabled = false"),
            "tools.policy section must include enabled = false default"
        );
    }
}
