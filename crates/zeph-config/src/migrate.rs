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
    "tui",
    "agents",
    "experiments",
    "lsp",
    "telemetry",
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

#[allow(clippy::format_push_string, clippy::collapsible_if, clippy::ref_option)]
fn migrate_ollama_provider(
    llm: &toml_edit::Table,
    model: &Option<String>,
    base_url: &Option<String>,
    embedding_model: &Option<String>,
) -> Vec<String> {
    let mut block = "[[llm.providers]]\ntype = \"ollama\"\n".to_owned();
    if let Some(m) = model {
        block.push_str(&format!("model = \"{m}\"\n"));
    }
    if let Some(em) = embedding_model {
        block.push_str(&format!("embedding_model = \"{em}\"\n"));
    }
    if let Some(u) = base_url {
        block.push_str(&format!("base_url = \"{u}\"\n"));
    }
    let _ = llm; // not needed for simple ollama case
    vec![block]
}

#[allow(clippy::format_push_string, clippy::collapsible_if, clippy::ref_option)]
fn migrate_claude_provider(llm: &toml_edit::Table, model: &Option<String>) -> Vec<String> {
    let mut block = "[[llm.providers]]\ntype = \"claude\"\n".to_owned();
    if let Some(cloud) = llm.get("cloud").and_then(toml_edit::Item::as_table) {
        if let Some(m) = cloud.get("model").and_then(toml_edit::Item::as_str) {
            block.push_str(&format!("model = \"{m}\"\n"));
        }
        if let Some(t) = cloud
            .get("max_tokens")
            .and_then(toml_edit::Item::as_integer)
        {
            block.push_str(&format!("max_tokens = {t}\n"));
        }
        if cloud
            .get("server_compaction")
            .and_then(toml_edit::Item::as_bool)
            == Some(true)
        {
            block.push_str("server_compaction = true\n");
        }
        if cloud
            .get("enable_extended_context")
            .and_then(toml_edit::Item::as_bool)
            == Some(true)
        {
            block.push_str("enable_extended_context = true\n");
        }
        if let Some(thinking) = cloud.get("thinking").and_then(toml_edit::Item::as_table) {
            let pairs: Vec<String> = thinking.iter().map(|(k, v)| format!("{k} = {v}")).collect();
            block.push_str(&format!("thinking = {{ {} }}\n", pairs.join(", ")));
        }
    } else if let Some(m) = model {
        block.push_str(&format!("model = \"{m}\"\n"));
    }
    vec![block]
}

#[allow(clippy::format_push_string, clippy::collapsible_if, clippy::ref_option)]
fn migrate_openai_provider(llm: &toml_edit::Table, model: &Option<String>) -> Vec<String> {
    let mut block = "[[llm.providers]]\ntype = \"openai\"\n".to_owned();
    if let Some(openai) = llm.get("openai").and_then(toml_edit::Item::as_table) {
        copy_str_field(openai, "model", &mut block);
        copy_str_field(openai, "base_url", &mut block);
        copy_int_field(openai, "max_tokens", &mut block);
        copy_str_field(openai, "embedding_model", &mut block);
        copy_str_field(openai, "reasoning_effort", &mut block);
    } else if let Some(m) = model {
        block.push_str(&format!("model = \"{m}\"\n"));
    }
    vec![block]
}

#[allow(clippy::format_push_string, clippy::collapsible_if, clippy::ref_option)]
fn migrate_gemini_provider(llm: &toml_edit::Table, model: &Option<String>) -> Vec<String> {
    let mut block = "[[llm.providers]]\ntype = \"gemini\"\n".to_owned();
    if let Some(gemini) = llm.get("gemini").and_then(toml_edit::Item::as_table) {
        copy_str_field(gemini, "model", &mut block);
        copy_int_field(gemini, "max_tokens", &mut block);
        copy_str_field(gemini, "base_url", &mut block);
        copy_str_field(gemini, "embedding_model", &mut block);
        copy_str_field(gemini, "thinking_level", &mut block);
        copy_int_field(gemini, "thinking_budget", &mut block);
        if let Some(v) = gemini
            .get("include_thoughts")
            .and_then(toml_edit::Item::as_bool)
        {
            block.push_str(&format!("include_thoughts = {v}\n"));
        }
    } else if let Some(m) = model {
        block.push_str(&format!("model = \"{m}\"\n"));
    }
    vec![block]
}

#[allow(clippy::format_push_string, clippy::collapsible_if, clippy::ref_option)]
fn migrate_compatible_provider(llm: &toml_edit::Table) -> Vec<String> {
    let mut blocks = Vec::new();
    if let Some(compat_arr) = llm
        .get("compatible")
        .and_then(toml_edit::Item::as_array_of_tables)
    {
        for entry in compat_arr {
            let mut block = "[[llm.providers]]\ntype = \"compatible\"\n".to_owned();
            copy_str_field(entry, "name", &mut block);
            copy_str_field(entry, "base_url", &mut block);
            copy_str_field(entry, "model", &mut block);
            copy_int_field(entry, "max_tokens", &mut block);
            copy_str_field(entry, "embedding_model", &mut block);
            blocks.push(block);
        }
    }
    blocks
}

// Returns (provider_blocks, routing, routes_block)
#[allow(clippy::format_push_string, clippy::collapsible_if, clippy::ref_option)]
fn migrate_orchestrator_provider(
    llm: &toml_edit::Table,
    model: &Option<String>,
    base_url: &Option<String>,
    embedding_model: &Option<String>,
) -> (Vec<String>, Option<String>, Option<String>) {
    let mut blocks = Vec::new();
    let routing = Some("task".to_owned());
    let mut routes_block = None;
    if let Some(orch) = llm.get("orchestrator").and_then(toml_edit::Item::as_table) {
        let default_name = orch
            .get("default")
            .and_then(toml_edit::Item::as_str)
            .unwrap_or("")
            .to_owned();
        let embed_name = orch
            .get("embed")
            .and_then(toml_edit::Item::as_str)
            .unwrap_or("")
            .to_owned();
        if let Some(routes) = orch.get("routes").and_then(toml_edit::Item::as_table) {
            let mut rb = "[llm.routes]\n".to_owned();
            for (key, val) in routes {
                if let Some(arr) = val.as_array() {
                    let items: Vec<String> = arr
                        .iter()
                        .filter_map(toml_edit::Value::as_str)
                        .map(|s| format!("\"{s}\""))
                        .collect();
                    rb.push_str(&format!("{key} = [{}]\n", items.join(", ")));
                }
            }
            routes_block = Some(rb);
        }
        if let Some(providers) = orch.get("providers").and_then(toml_edit::Item::as_table) {
            for (name, pcfg_item) in providers {
                let Some(pcfg) = pcfg_item.as_table() else {
                    continue;
                };
                let ptype = pcfg
                    .get("type")
                    .and_then(toml_edit::Item::as_str)
                    .unwrap_or("ollama");
                let mut block =
                    format!("[[llm.providers]]\nname = \"{name}\"\ntype = \"{ptype}\"\n");
                if name == default_name {
                    block.push_str("default = true\n");
                }
                if name == embed_name {
                    block.push_str("embed = true\n");
                }
                copy_str_field(pcfg, "model", &mut block);
                copy_str_field(pcfg, "base_url", &mut block);
                copy_str_field(pcfg, "embedding_model", &mut block);
                if ptype == "claude" && !pcfg.contains_key("model") {
                    if let Some(cloud) = llm.get("cloud").and_then(toml_edit::Item::as_table) {
                        copy_str_field(cloud, "model", &mut block);
                        copy_int_field(cloud, "max_tokens", &mut block);
                    }
                }
                if ptype == "openai" && !pcfg.contains_key("model") {
                    if let Some(openai) = llm.get("openai").and_then(toml_edit::Item::as_table) {
                        copy_str_field(openai, "model", &mut block);
                        copy_str_field(openai, "base_url", &mut block);
                        copy_int_field(openai, "max_tokens", &mut block);
                        copy_str_field(openai, "embedding_model", &mut block);
                    }
                }
                if ptype == "ollama" && !pcfg.contains_key("base_url") {
                    if let Some(u) = base_url {
                        block.push_str(&format!("base_url = \"{u}\"\n"));
                    }
                }
                if ptype == "ollama" && !pcfg.contains_key("model") {
                    if let Some(m) = model {
                        block.push_str(&format!("model = \"{m}\"\n"));
                    }
                }
                if ptype == "ollama" && !pcfg.contains_key("embedding_model") {
                    if let Some(em) = embedding_model {
                        block.push_str(&format!("embedding_model = \"{em}\"\n"));
                    }
                }
                blocks.push(block);
            }
        }
    }
    (blocks, routing, routes_block)
}

// Returns (provider_blocks, routing)
#[allow(clippy::format_push_string, clippy::collapsible_if, clippy::ref_option)]
fn migrate_router_provider(
    llm: &toml_edit::Table,
    model: &Option<String>,
    base_url: &Option<String>,
    embedding_model: &Option<String>,
) -> (Vec<String>, Option<String>) {
    let mut blocks = Vec::new();
    let mut routing = None;
    if let Some(router) = llm.get("router").and_then(toml_edit::Item::as_table) {
        let strategy = router
            .get("strategy")
            .and_then(toml_edit::Item::as_str)
            .unwrap_or("ema");
        routing = Some(strategy.to_owned());
        if let Some(chain) = router.get("chain").and_then(toml_edit::Item::as_array) {
            for item in chain {
                let name = item.as_str().unwrap_or_default();
                let ptype = infer_provider_type(name, llm);
                let mut block =
                    format!("[[llm.providers]]\nname = \"{name}\"\ntype = \"{ptype}\"\n");
                match ptype {
                    "claude" => {
                        if let Some(cloud) = llm.get("cloud").and_then(toml_edit::Item::as_table) {
                            copy_str_field(cloud, "model", &mut block);
                            copy_int_field(cloud, "max_tokens", &mut block);
                        }
                    }
                    "openai" => {
                        if let Some(openai) = llm.get("openai").and_then(toml_edit::Item::as_table)
                        {
                            copy_str_field(openai, "model", &mut block);
                            copy_str_field(openai, "base_url", &mut block);
                            copy_int_field(openai, "max_tokens", &mut block);
                            copy_str_field(openai, "embedding_model", &mut block);
                        } else {
                            if let Some(m) = model {
                                block.push_str(&format!("model = \"{m}\"\n"));
                            }
                            if let Some(u) = base_url {
                                block.push_str(&format!("base_url = \"{u}\"\n"));
                            }
                        }
                    }
                    "ollama" => {
                        if let Some(m) = model {
                            block.push_str(&format!("model = \"{m}\"\n"));
                        }
                        if let Some(em) = embedding_model {
                            block.push_str(&format!("embedding_model = \"{em}\"\n"));
                        }
                        if let Some(u) = base_url {
                            block.push_str(&format!("base_url = \"{u}\"\n"));
                        }
                    }
                    _ => {
                        if let Some(m) = model {
                            block.push_str(&format!("model = \"{m}\"\n"));
                        }
                    }
                }
                blocks.push(block);
            }
        }
    }
    (blocks, routing)
}

/// Migrate a TOML config string from the old `[llm]` format (with `provider`, `[llm.cloud]`,
/// `[llm.openai]`, `[llm.orchestrator]`, `[llm.router]` sections) to the new
/// `[[llm.providers]]` array format.
///
/// If the config does not contain legacy LLM keys, it is returned unchanged.
/// Creates a `.bak` backup at `backup_path` before writing.
///
/// # Errors
///
/// Returns `MigrateError::Parse` if the input TOML is invalid.
#[allow(
    clippy::too_many_lines,
    clippy::format_push_string,
    clippy::manual_let_else,
    clippy::op_ref,
    clippy::collapsible_if
)]
pub fn migrate_llm_to_providers(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    let doc = toml_src.parse::<toml_edit::DocumentMut>()?;

    // Detect whether this is a legacy-format config.
    let llm = match doc.get("llm").and_then(toml_edit::Item::as_table) {
        Some(t) => t,
        None => {
            // No [llm] section at all — nothing to migrate.
            return Ok(MigrationResult {
                output: toml_src.to_owned(),
                added_count: 0,
                sections_added: Vec::new(),
            });
        }
    };

    let has_provider_field = llm.contains_key("provider");
    let has_cloud = llm.contains_key("cloud");
    let has_openai = llm.contains_key("openai");
    let has_gemini = llm.contains_key("gemini");
    let has_orchestrator = llm.contains_key("orchestrator");
    let has_router = llm.contains_key("router");
    let has_providers = llm.contains_key("providers");

    if !has_provider_field
        && !has_cloud
        && !has_openai
        && !has_orchestrator
        && !has_router
        && !has_gemini
    {
        // Already in new format (or empty).
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            added_count: 0,
            sections_added: Vec::new(),
        });
    }

    if has_providers {
        // Mixed format — refuse to migrate, let the caller handle the error.
        return Err(MigrateError::Parse(
            "cannot migrate: [[llm.providers]] already exists alongside legacy keys"
                .parse::<toml_edit::DocumentMut>()
                .unwrap_err(),
        ));
    }

    // Build new [[llm.providers]] entries from legacy sections.
    let provider_str = llm
        .get("provider")
        .and_then(toml_edit::Item::as_str)
        .unwrap_or("ollama");
    let base_url = llm
        .get("base_url")
        .and_then(toml_edit::Item::as_str)
        .map(str::to_owned);
    let model = llm
        .get("model")
        .and_then(toml_edit::Item::as_str)
        .map(str::to_owned);
    let embedding_model = llm
        .get("embedding_model")
        .and_then(toml_edit::Item::as_str)
        .map(str::to_owned);

    // Collect provider entries as inline TOML strings.
    let mut provider_blocks: Vec<String> = Vec::new();
    let mut routing: Option<String> = None;
    let mut routes_block: Option<String> = None;

    match provider_str {
        "ollama" => {
            provider_blocks.extend(migrate_ollama_provider(
                llm,
                &model,
                &base_url,
                &embedding_model,
            ));
        }
        "claude" => {
            provider_blocks.extend(migrate_claude_provider(llm, &model));
        }
        "openai" => {
            provider_blocks.extend(migrate_openai_provider(llm, &model));
        }
        "gemini" => {
            provider_blocks.extend(migrate_gemini_provider(llm, &model));
        }
        "compatible" => {
            provider_blocks.extend(migrate_compatible_provider(llm));
        }
        "orchestrator" => {
            let (blocks, r, rb) =
                migrate_orchestrator_provider(llm, &model, &base_url, &embedding_model);
            provider_blocks.extend(blocks);
            routing = r;
            routes_block = rb;
        }
        "router" => {
            let (blocks, r) = migrate_router_provider(llm, &model, &base_url, &embedding_model);
            provider_blocks.extend(blocks);
            routing = r;
        }
        other => {
            let mut block = format!("[[llm.providers]]\ntype = \"{other}\"\n");
            if let Some(ref m) = model {
                block.push_str(&format!("model = \"{m}\"\n"));
            }
            provider_blocks.push(block);
        }
    }

    if provider_blocks.is_empty() {
        // Nothing to convert; return as-is.
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            added_count: 0,
            sections_added: Vec::new(),
        });
    }

    // Build the replacement [llm] section.
    let mut new_llm = "[llm]\n".to_owned();
    if let Some(ref r) = routing {
        new_llm.push_str(&format!("routing = \"{r}\"\n"));
    }
    // Carry over cross-cutting LLM settings.
    for key in &[
        "response_cache_enabled",
        "response_cache_ttl_secs",
        "semantic_cache_enabled",
        "semantic_cache_threshold",
        "semantic_cache_max_candidates",
        "summary_model",
        "instruction_file",
    ] {
        if let Some(val) = llm.get(key) {
            if let Some(v) = val.as_value() {
                let raw = value_to_toml_string(v);
                if !raw.is_empty() {
                    new_llm.push_str(&format!("{key} = {raw}\n"));
                }
            }
        }
    }
    new_llm.push('\n');

    if let Some(rb) = routes_block {
        new_llm.push_str(&rb);
        new_llm.push('\n');
    }

    for block in &provider_blocks {
        new_llm.push_str(block);
        new_llm.push('\n');
    }

    // Remove old [llm] section and all its sub-sections from the source,
    // then prepend the new section.
    let output = replace_llm_section(toml_src, &new_llm);

    Ok(MigrationResult {
        output,
        added_count: provider_blocks.len(),
        sections_added: vec!["llm.providers".to_owned()],
    })
}

/// Infer provider type from a name used in router chain.
fn infer_provider_type<'a>(name: &str, llm: &'a toml_edit::Table) -> &'a str {
    match name {
        "claude" => "claude",
        "openai" => "openai",
        "gemini" => "gemini",
        "ollama" => "ollama",
        "candle" => "candle",
        _ => {
            // Check if there's a compatible entry with this name.
            if llm.contains_key("compatible") {
                "compatible"
            } else if llm.contains_key("openai") {
                "openai"
            } else {
                "ollama"
            }
        }
    }
}

fn copy_str_field(table: &toml_edit::Table, key: &str, out: &mut String) {
    use std::fmt::Write as _;
    if let Some(v) = table.get(key).and_then(toml_edit::Item::as_str) {
        let _ = writeln!(out, "{key} = \"{v}\"");
    }
}

fn copy_int_field(table: &toml_edit::Table, key: &str, out: &mut String) {
    use std::fmt::Write as _;
    if let Some(v) = table.get(key).and_then(toml_edit::Item::as_integer) {
        let _ = writeln!(out, "{key} = {v}");
    }
}

/// Replace the entire [llm] section (including all [llm.*] sub-sections and
/// [[llm.*]] array-of-table entries) with `new_llm_section`.
fn replace_llm_section(toml_str: &str, new_llm_section: &str) -> String {
    let mut out = String::new();
    let mut in_llm = false;
    let mut skip_until_next_top = false;

    for line in toml_str.lines() {
        let trimmed = line.trim();

        // Check if this is a top-level section header [something] or [[something]].
        let is_top_section = (trimmed.starts_with('[') && !trimmed.starts_with("[["))
            && trimmed.ends_with(']')
            && !trimmed[1..trimmed.len() - 1].contains('.');
        let is_top_aot = trimmed.starts_with("[[")
            && trimmed.ends_with("]]")
            && !trimmed[2..trimmed.len() - 2].contains('.');
        let is_llm_sub = (trimmed.starts_with("[llm") || trimmed.starts_with("[[llm"))
            && (trimmed.contains(']'));

        if is_llm_sub || (in_llm && !is_top_section && !is_top_aot) {
            in_llm = true;
            skip_until_next_top = true;
            continue;
        }

        if is_top_section || is_top_aot {
            if skip_until_next_top {
                // Emit the new LLM section before the next top-level section.
                out.push_str(new_llm_section);
                skip_until_next_top = false;
            }
            in_llm = false;
        }

        if !skip_until_next_top {
            out.push_str(line);
            out.push('\n');
        }
    }

    // If [llm] was the last section, append now.
    if skip_until_next_top {
        out.push_str(new_llm_section);
    }

    out
}

/// Migrate an old `[llm.stt]` section (with `model` / `base_url` fields) to the new format
/// where those fields live on a `[[llm.providers]]` entry via `stt_model`.
///
/// Transformations:
/// - `[llm.stt].model` → `stt_model` on the matching or new `[[llm.providers]]` entry
/// - `[llm.stt].base_url` → `base_url` on that entry (skipped when already present)
/// - `[llm.stt].provider` is updated to the provider name; the entry is assigned an explicit
///   `name` when it lacked one (W2 guard).
/// - Old `model` and `base_url` keys are stripped from `[llm.stt]`.
///
/// If `[llm.stt]` is absent or already uses the new format (no `model` / `base_url`), the
/// input is returned unchanged.
///
/// # Errors
///
/// Returns `MigrateError::Parse` if the input TOML is invalid.
/// Returns `MigrateError::InvalidStructure` if `[llm.stt].model` is present but the `[llm]`
/// key is absent or not a table, making mutation impossible.
#[allow(clippy::too_many_lines)]
pub fn migrate_stt_to_provider(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    let mut doc = toml_src.parse::<toml_edit::DocumentMut>()?;

    // Extract fields from [llm.stt] if present.
    let stt_model = doc
        .get("llm")
        .and_then(toml_edit::Item::as_table)
        .and_then(|llm| llm.get("stt"))
        .and_then(toml_edit::Item::as_table)
        .and_then(|stt| stt.get("model"))
        .and_then(toml_edit::Item::as_str)
        .map(ToOwned::to_owned);

    let stt_base_url = doc
        .get("llm")
        .and_then(toml_edit::Item::as_table)
        .and_then(|llm| llm.get("stt"))
        .and_then(toml_edit::Item::as_table)
        .and_then(|stt| stt.get("base_url"))
        .and_then(toml_edit::Item::as_str)
        .map(ToOwned::to_owned);

    let stt_provider_hint = doc
        .get("llm")
        .and_then(toml_edit::Item::as_table)
        .and_then(|llm| llm.get("stt"))
        .and_then(toml_edit::Item::as_table)
        .and_then(|stt| stt.get("provider"))
        .and_then(toml_edit::Item::as_str)
        .map(ToOwned::to_owned)
        .unwrap_or_default();

    // Nothing to migrate if [llm.stt] does not exist or already lacks the old fields.
    if stt_model.is_none() && stt_base_url.is_none() {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            added_count: 0,
            sections_added: Vec::new(),
        });
    }

    let stt_model = stt_model.unwrap_or_else(|| "whisper-1".to_owned());

    // Determine the target provider type based on provider hint.
    let target_type = match stt_provider_hint.as_str() {
        "candle-whisper" | "candle" => "candle",
        _ => "openai",
    };

    // Find or create a [[llm.providers]] entry to attach stt_model to.
    // Priority: entry whose effective name matches the hint, else first entry of matching type.
    let providers = doc
        .get("llm")
        .and_then(toml_edit::Item::as_table)
        .and_then(|llm| llm.get("providers"))
        .and_then(toml_edit::Item::as_array_of_tables);

    let matching_idx = providers.and_then(|arr| {
        arr.iter().enumerate().find_map(|(i, t)| {
            let name = t
                .get("name")
                .and_then(toml_edit::Item::as_str)
                .unwrap_or("");
            let ptype = t
                .get("type")
                .and_then(toml_edit::Item::as_str)
                .unwrap_or("");
            // Match by explicit name hint or by type when hint is a legacy backend string.
            let name_match = !stt_provider_hint.is_empty()
                && (name == stt_provider_hint || ptype == stt_provider_hint);
            let type_match = ptype == target_type;
            if name_match || type_match {
                Some(i)
            } else {
                None
            }
        })
    });

    // Determine the final provider name to write into [llm.stt].provider.
    let resolved_provider_name: String;

    if let Some(idx) = matching_idx {
        // Attach stt_model to the existing entry.
        let llm_mut = doc
            .get_mut("llm")
            .and_then(toml_edit::Item::as_table_mut)
            .ok_or(MigrateError::InvalidStructure(
                "[llm] table not accessible for mutation",
            ))?;
        let providers_mut = llm_mut
            .get_mut("providers")
            .and_then(toml_edit::Item::as_array_of_tables_mut)
            .ok_or(MigrateError::InvalidStructure(
                "[[llm.providers]] array not accessible for mutation",
            ))?;
        let entry = providers_mut
            .iter_mut()
            .nth(idx)
            .ok_or(MigrateError::InvalidStructure(
                "[[llm.providers]] entry index out of range during mutation",
            ))?;

        // W2: ensure explicit name.
        let existing_name = entry
            .get("name")
            .and_then(toml_edit::Item::as_str)
            .map(ToOwned::to_owned);
        let entry_name = existing_name.unwrap_or_else(|| {
            let t = entry
                .get("type")
                .and_then(toml_edit::Item::as_str)
                .unwrap_or("openai");
            format!("{t}-stt")
        });
        entry.insert("name", toml_edit::value(entry_name.clone()));
        entry.insert("stt_model", toml_edit::value(stt_model.clone()));
        if stt_base_url.is_some() && entry.get("base_url").is_none() {
            entry.insert(
                "base_url",
                toml_edit::value(stt_base_url.as_deref().unwrap_or_default()),
            );
        }
        resolved_provider_name = entry_name;
    } else {
        // No matching entry — append a new [[llm.providers]] block.
        let new_name = if target_type == "candle" {
            "local-whisper".to_owned()
        } else {
            "openai-stt".to_owned()
        };
        let mut new_entry = toml_edit::Table::new();
        new_entry.insert("name", toml_edit::value(new_name.clone()));
        new_entry.insert("type", toml_edit::value(target_type));
        new_entry.insert("stt_model", toml_edit::value(stt_model.clone()));
        if let Some(ref url) = stt_base_url {
            new_entry.insert("base_url", toml_edit::value(url.clone()));
        }
        // Ensure [[llm.providers]] array exists.
        let llm_mut = doc
            .get_mut("llm")
            .and_then(toml_edit::Item::as_table_mut)
            .ok_or(MigrateError::InvalidStructure(
                "[llm] table not accessible for mutation",
            ))?;
        if let Some(item) = llm_mut.get_mut("providers") {
            if let Some(arr) = item.as_array_of_tables_mut() {
                arr.push(new_entry);
            }
        } else {
            let mut arr = toml_edit::ArrayOfTables::new();
            arr.push(new_entry);
            llm_mut.insert("providers", toml_edit::Item::ArrayOfTables(arr));
        }
        resolved_provider_name = new_name;
    }

    // Update [llm.stt]: set provider name, remove old fields.
    if let Some(stt_table) = doc
        .get_mut("llm")
        .and_then(toml_edit::Item::as_table_mut)
        .and_then(|llm| llm.get_mut("stt"))
        .and_then(toml_edit::Item::as_table_mut)
    {
        stt_table.insert("provider", toml_edit::value(resolved_provider_name.clone()));
        stt_table.remove("model");
        stt_table.remove("base_url");
    }

    Ok(MigrationResult {
        output: doc.to_string(),
        added_count: 1,
        sections_added: vec!["llm.providers.stt_model".to_owned()],
    })
}

/// Migrate `[orchestration] planner_model` to `planner_provider`.
///
/// The namespaces differ: `planner_model` held a raw model name (e.g. `"gpt-4o"`),
/// while `planner_provider` must reference a `[[llm.providers]]` `name` field. A migrated
/// value would cause a silent `warn!` from `build_planner_provider()` when resolution fails,
/// so the old value is commented out and a warning is emitted.
///
/// If `planner_model` is absent, the input is returned unchanged.
///
/// # Errors
///
/// Returns `MigrateError::Parse` if the input TOML is invalid.
pub fn migrate_planner_model_to_provider(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    let mut doc = toml_src.parse::<toml_edit::DocumentMut>()?;

    let old_value = doc
        .get("orchestration")
        .and_then(toml_edit::Item::as_table)
        .and_then(|t| t.get("planner_model"))
        .and_then(toml_edit::Item::as_value)
        .and_then(toml_edit::Value::as_str)
        .map(ToOwned::to_owned);

    let Some(old_model) = old_value else {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            added_count: 0,
            sections_added: Vec::new(),
        });
    };

    // Remove the old key via text substitution to preserve surrounding comments/formatting.
    // We rebuild the section comment in the output rather than using toml_edit mutations,
    // following the same line-oriented approach used elsewhere in this file.
    let commented_out = format!(
        "# planner_provider = \"{old_model}\"  \
         # MIGRATED: was planner_model; update to a [[llm.providers]] name"
    );

    let orch_table = doc
        .get_mut("orchestration")
        .and_then(toml_edit::Item::as_table_mut)
        .ok_or(MigrateError::InvalidStructure(
            "[orchestration] is not a table",
        ))?;
    orch_table.remove("planner_model");
    let decor = orch_table.decor_mut();
    let existing_suffix = decor.suffix().and_then(|s| s.as_str()).unwrap_or("");
    // Append the commented-out entry as a trailing comment on the section.
    let new_suffix = if existing_suffix.trim().is_empty() {
        format!("\n{commented_out}\n")
    } else {
        format!("{existing_suffix}\n{commented_out}\n")
    };
    decor.set_suffix(new_suffix);

    eprintln!(
        "Migration warning: [orchestration].planner_model has been renamed to planner_provider \
         and its value commented out. `planner_provider` must reference a [[llm.providers]] \
         `name` field, not a raw model name. Update or remove the commented line."
    );

    Ok(MigrationResult {
        output: doc.to_string(),
        added_count: 1,
        sections_added: vec!["orchestration.planner_provider".to_owned()],
    })
}

/// Migrate `[[mcp.servers]]` entries to add `trust_level = "trusted"` for any entry
/// that lacks an explicit `trust_level`.
///
/// Before this PR all config-defined servers skipped SSRF validation (equivalent to
/// `trust_level = "trusted"`). Without migration, upgrading to the new default
/// (`Untrusted`) would silently break remote servers on private networks.
///
/// This function adds `trust_level = "trusted"` only to entries that are missing the
/// field, preserving entries that already have it set.
///
/// # Errors
///
/// Returns `MigrateError::Parse` if the TOML cannot be parsed.
pub fn migrate_mcp_trust_levels(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    let mut doc = toml_src.parse::<toml_edit::DocumentMut>()?;
    let mut added = 0usize;

    let Some(mcp) = doc.get_mut("mcp").and_then(toml_edit::Item::as_table_mut) else {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            added_count: 0,
            sections_added: Vec::new(),
        });
    };

    let Some(servers) = mcp
        .get_mut("servers")
        .and_then(toml_edit::Item::as_array_of_tables_mut)
    else {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            added_count: 0,
            sections_added: Vec::new(),
        });
    };

    for entry in servers.iter_mut() {
        if !entry.contains_key("trust_level") {
            entry.insert(
                "trust_level",
                toml_edit::value(toml_edit::Value::from("trusted")),
            );
            added += 1;
        }
    }

    if added > 0 {
        eprintln!(
            "Migration: added trust_level = \"trusted\" to {added} [[mcp.servers]] \
             entr{} (preserving previous SSRF-skip behavior). \
             Review and adjust trust levels as needed.",
            if added == 1 { "y" } else { "ies" }
        );
    }

    Ok(MigrationResult {
        output: doc.to_string(),
        added_count: added,
        sections_added: if added > 0 {
            vec!["mcp.servers.trust_level".to_owned()]
        } else {
            Vec::new()
        },
    })
}

/// Migrate `[agent].max_tool_retries` → `[tools.retry].max_attempts` and
/// `[agent].max_retry_duration_secs` → `[tools.retry].budget_secs`.
///
/// Old fields are preserved (not removed) to avoid breaking configs that rely on them
/// until they are officially deprecated in a future release. The new `[tools.retry]` section
/// is added if missing, populated with the migrated values.
///
/// # Errors
///
/// Returns `MigrateError::Parse` if the TOML is invalid.
pub fn migrate_agent_retry_to_tools_retry(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    let mut doc = toml_src.parse::<toml_edit::DocumentMut>()?;

    let max_retries = doc
        .get("agent")
        .and_then(toml_edit::Item::as_table)
        .and_then(|t| t.get("max_tool_retries"))
        .and_then(toml_edit::Item::as_value)
        .and_then(toml_edit::Value::as_integer)
        .map(i64::cast_unsigned);

    let budget_secs = doc
        .get("agent")
        .and_then(toml_edit::Item::as_table)
        .and_then(|t| t.get("max_retry_duration_secs"))
        .and_then(toml_edit::Item::as_value)
        .and_then(toml_edit::Value::as_integer)
        .map(i64::cast_unsigned);

    if max_retries.is_none() && budget_secs.is_none() {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            added_count: 0,
            sections_added: Vec::new(),
        });
    }

    // Ensure [tools.retry] section exists.
    if !doc.contains_key("tools") {
        doc.insert("tools", toml_edit::Item::Table(toml_edit::Table::new()));
    }
    let tools_table = doc
        .get_mut("tools")
        .and_then(toml_edit::Item::as_table_mut)
        .ok_or(MigrateError::InvalidStructure("[tools] is not a table"))?;

    if !tools_table.contains_key("retry") {
        tools_table.insert("retry", toml_edit::Item::Table(toml_edit::Table::new()));
    }
    let retry_table = tools_table
        .get_mut("retry")
        .and_then(toml_edit::Item::as_table_mut)
        .ok_or(MigrateError::InvalidStructure(
            "[tools.retry] is not a table",
        ))?;

    let mut added_count = 0usize;

    if let Some(retries) = max_retries
        && !retry_table.contains_key("max_attempts")
    {
        retry_table.insert(
            "max_attempts",
            toml_edit::value(i64::try_from(retries).unwrap_or(2)),
        );
        added_count += 1;
    }

    if let Some(secs) = budget_secs
        && !retry_table.contains_key("budget_secs")
    {
        retry_table.insert(
            "budget_secs",
            toml_edit::value(i64::try_from(secs).unwrap_or(30)),
        );
        added_count += 1;
    }

    if added_count > 0 {
        eprintln!(
            "Migration: [agent].max_tool_retries / max_retry_duration_secs migrated to \
             [tools.retry].max_attempts / budget_secs. Old fields preserved for compatibility."
        );
    }

    Ok(MigrationResult {
        output: doc.to_string(),
        added_count,
        sections_added: if added_count > 0 {
            vec!["tools.retry".to_owned()]
        } else {
            Vec::new()
        },
    })
}

/// Add a commented-out `database_url = ""` entry under `[memory]` if absent.
///
/// If the `[memory]` section does not exist it is created. This migration surfaces the
/// `PostgreSQL` URL option for users upgrading from a pre-postgres config file.
///
/// # Errors
///
/// Returns `MigrateError::Parse` if the TOML cannot be parsed.
pub fn migrate_database_url(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    let mut doc = toml_src.parse::<toml_edit::DocumentMut>()?;

    // Ensure [memory] section exists.
    if !doc.contains_key("memory") {
        doc.insert("memory", toml_edit::Item::Table(toml_edit::Table::new()));
    }

    let memory = doc
        .get_mut("memory")
        .and_then(toml_edit::Item::as_table_mut)
        .ok_or(MigrateError::InvalidStructure(
            "[memory] key exists but is not a table",
        ))?;

    if memory.contains_key("database_url") {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            added_count: 0,
            sections_added: Vec::new(),
        });
    }

    // Append as a commented-out line via table suffix decor (same pattern as merge_table_commented).
    let comment = "# PostgreSQL connection URL (used when binary is compiled with --features postgres).\n\
         # Leave empty and store the actual URL in the vault:\n\
         #   zeph vault set ZEPH_DATABASE_URL \"postgres://user:pass@localhost:5432/zeph\"\n\
         # database_url = \"\"\n";
    append_comment_to_table_suffix(memory, comment);

    Ok(MigrationResult {
        output: doc.to_string(),
        added_count: 1,
        sections_added: vec!["memory.database_url".to_owned()],
    })
}

/// No-op migration for `[tools.shell]` transactional fields added in #2414.
///
/// All 5 new fields have `#[serde(default)]` so existing configs parse without changes.
/// This step adds them as commented-out hints in `[tools.shell]` if not already present.
///
/// # Errors
///
/// Returns `MigrateError` if the TOML cannot be parsed or `[tools.shell]` is malformed.
pub fn migrate_shell_transactional(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    let mut doc = toml_src.parse::<toml_edit::DocumentMut>()?;

    let tools_shell_exists = doc
        .get("tools")
        .and_then(toml_edit::Item::as_table)
        .is_some_and(|t| t.contains_key("shell"));
    if !tools_shell_exists {
        // No [tools.shell] section — nothing to annotate; new configs will get defaults.
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            added_count: 0,
            sections_added: Vec::new(),
        });
    }

    let shell = doc
        .get_mut("tools")
        .and_then(toml_edit::Item::as_table_mut)
        .and_then(|t| t.get_mut("shell"))
        .and_then(toml_edit::Item::as_table_mut)
        .ok_or(MigrateError::InvalidStructure(
            "[tools.shell] is not a table",
        ))?;

    if shell.contains_key("transactional") {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            added_count: 0,
            sections_added: Vec::new(),
        });
    }

    let comment = "# Transactional shell: snapshot files before write commands, rollback on failure.\n\
         # transactional = false\n\
         # transaction_scope = []          # glob patterns; empty = all extracted paths\n\
         # auto_rollback = false           # rollback when exit code >= 2\n\
         # auto_rollback_exit_codes = []   # explicit exit codes; overrides >= 2 heuristic\n\
         # snapshot_required = false       # abort if snapshot fails (default: warn and proceed)\n";
    append_comment_to_table_suffix(shell, comment);

    Ok(MigrationResult {
        output: doc.to_string(),
        added_count: 1,
        sections_added: vec!["tools.shell.transactional".to_owned()],
    })
}

/// Migration step: add `budget_hint_enabled` as a commented-out entry under `[agent]` if absent.
///
/// # Errors
///
/// Returns an error if the config cannot be parsed or the `[agent]` section is malformed.
pub fn migrate_agent_budget_hint(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    let mut doc = toml_src.parse::<toml_edit::DocumentMut>()?;

    let agent_exists = doc.contains_key("agent");
    if !agent_exists {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            added_count: 0,
            sections_added: Vec::new(),
        });
    }

    let agent = doc
        .get_mut("agent")
        .and_then(toml_edit::Item::as_table_mut)
        .ok_or(MigrateError::InvalidStructure("[agent] is not a table"))?;

    if agent.contains_key("budget_hint_enabled") {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            added_count: 0,
            sections_added: Vec::new(),
        });
    }

    let comment = "# Inject <budget> XML into the system prompt so the LLM can self-regulate (#2267).\n\
         # budget_hint_enabled = true\n";
    append_comment_to_table_suffix(agent, comment);

    Ok(MigrationResult {
        output: doc.to_string(),
        added_count: 1,
        sections_added: vec!["agent.budget_hint_enabled".to_owned()],
    })
}

/// Add a commented-out `[memory.forgetting]` section if absent (#2397).
///
/// All forgetting fields have `#[serde(default)]` so existing configs parse without changes.
/// This step surfaces the new section for users upgrading from older configs.
///
/// # Errors
///
/// Returns `MigrateError::Parse` if the TOML cannot be parsed.
pub fn migrate_forgetting_config(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    use toml_edit::{Item, Table};

    let mut doc = toml_src.parse::<toml_edit::DocumentMut>()?;

    // If [memory] does not exist, create it so we can check for [memory.forgetting].
    if !doc.contains_key("memory") {
        doc.insert("memory", Item::Table(Table::new()));
    }

    let memory = doc
        .get_mut("memory")
        .and_then(Item::as_table_mut)
        .ok_or(MigrateError::InvalidStructure("[memory] is not a table"))?;

    if memory.contains_key("forgetting") {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            added_count: 0,
            sections_added: Vec::new(),
        });
    }

    let comment = "# SleepGate forgetting sweep (#2397). Disabled by default.\n\
         # [memory.forgetting]\n\
         # enabled = false\n\
         # decay_rate = 0.1                   # per-sweep importance decay\n\
         # forgetting_floor = 0.05            # prune below this score\n\
         # sweep_interval_secs = 7200         # run every 2 hours\n\
         # sweep_batch_size = 500\n\
         # protect_recent_hours = 24\n\
         # protect_min_access_count = 3\n";
    append_comment_to_table_suffix(memory, comment);

    Ok(MigrationResult {
        output: doc.to_string(),
        added_count: 1,
        sections_added: vec!["memory.forgetting".to_owned()],
    })
}

/// Add a commented-out `[memory.compression.predictor]` block if absent (#2460).
///
/// All predictor fields have `#[serde(default)]` so existing configs parse without changes.
///
/// # Errors
///
/// Returns `MigrateError::Parse` if the TOML cannot be parsed.
pub fn migrate_compression_predictor_config(
    toml_src: &str,
) -> Result<MigrationResult, MigrateError> {
    use toml_edit::{Item, Table};

    let mut doc = toml_src.parse::<toml_edit::DocumentMut>()?;

    // Ensure [memory] and [memory.compression] exist.
    if !doc.contains_key("memory") {
        doc.insert("memory", Item::Table(Table::new()));
    }
    let memory = doc
        .get_mut("memory")
        .and_then(Item::as_table_mut)
        .ok_or(MigrateError::InvalidStructure("[memory] is not a table"))?;

    if !memory.contains_key("compression") {
        memory.insert("compression", Item::Table(Table::new()));
    }
    let compression = memory
        .get_mut("compression")
        .and_then(Item::as_table_mut)
        .ok_or(MigrateError::InvalidStructure(
            "[memory.compression] is not a table",
        ))?;

    if compression.contains_key("predictor") {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            added_count: 0,
            sections_added: Vec::new(),
        });
    }

    let comment = "# Performance-floor compression ratio predictor (#2460). Disabled by default.\n\
         # [memory.compression.predictor]\n\
         # enabled = false\n\
         # min_samples = 10                                             # cold-start threshold\n\
         # candidate_ratios = [0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 0.9]\n\
         # retrain_interval = 5\n\
         # max_training_samples = 200\n";
    append_comment_to_table_suffix(compression, comment);

    Ok(MigrationResult {
        output: doc.to_string(),
        added_count: 1,
        sections_added: vec!["memory.compression.predictor".to_owned()],
    })
}

/// Add a commented-out `[memory.microcompact]` block if absent (#2699).
///
/// # Errors
///
/// Returns `MigrateError::Parse` if the TOML cannot be parsed.
pub fn migrate_microcompact_config(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    use toml_edit::{Item, Table};

    let mut doc = toml_src.parse::<toml_edit::DocumentMut>()?;

    if !doc.contains_key("memory") {
        doc.insert("memory", Item::Table(Table::new()));
    }
    let memory = doc
        .get_mut("memory")
        .and_then(Item::as_table_mut)
        .ok_or(MigrateError::InvalidStructure("[memory] is not a table"))?;

    if memory.contains_key("microcompact") {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            added_count: 0,
            sections_added: Vec::new(),
        });
    }

    let comment = "# Time-based microcompact (#2699). Strips stale low-value tool outputs after idle.\n\
         # [memory.microcompact]\n\
         # enabled = false\n\
         # gap_threshold_minutes = 60   # idle gap before clearing stale outputs\n\
         # keep_recent = 3              # always keep this many recent outputs intact\n";
    append_comment_to_table_suffix(memory, comment);

    Ok(MigrationResult {
        output: doc.to_string(),
        added_count: 1,
        sections_added: vec!["memory.microcompact".to_owned()],
    })
}

/// Add a commented-out `[memory.autodream]` block if absent (#2697).
///
/// # Errors
///
/// Returns `MigrateError::Parse` if the TOML cannot be parsed.
pub fn migrate_autodream_config(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    use toml_edit::{Item, Table};

    let mut doc = toml_src.parse::<toml_edit::DocumentMut>()?;

    if !doc.contains_key("memory") {
        doc.insert("memory", Item::Table(Table::new()));
    }
    let memory = doc
        .get_mut("memory")
        .and_then(Item::as_table_mut)
        .ok_or(MigrateError::InvalidStructure("[memory] is not a table"))?;

    if memory.contains_key("autodream") {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            added_count: 0,
            sections_added: Vec::new(),
        });
    }

    let comment = "# autoDream background memory consolidation (#2697). Disabled by default.\n\
         # [memory.autodream]\n\
         # enabled = false\n\
         # min_sessions = 5             # sessions since last consolidation\n\
         # min_hours = 8                # hours since last consolidation\n\
         # consolidation_provider = \"\" # provider name from [[llm.providers]]; empty = primary\n\
         # max_iterations = 5\n";
    append_comment_to_table_suffix(memory, comment);

    Ok(MigrationResult {
        output: doc.to_string(),
        added_count: 1,
        sections_added: vec!["memory.autodream".to_owned()],
    })
}

/// Add a commented-out `[magic_docs]` block if absent (#2702).
///
/// # Errors
///
/// Returns `MigrateError::Parse` if the TOML cannot be parsed.
pub fn migrate_magic_docs_config(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    use toml_edit::{Item, Table};

    let mut doc = toml_src.parse::<toml_edit::DocumentMut>()?;

    if doc.contains_key("magic_docs") {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            added_count: 0,
            sections_added: Vec::new(),
        });
    }

    doc.insert("magic_docs", Item::Table(Table::new()));
    let comment = "# MagicDocs auto-maintained markdown (#2702). Disabled by default.\n\
         # [magic_docs]\n\
         # enabled = false\n\
         # min_turns_between_updates = 10\n\
         # update_provider = \"\"         # provider name from [[llm.providers]]; empty = primary\n\
         # max_iterations = 3\n";
    // Remove the just-inserted empty table and replace with a comment.
    doc.remove("magic_docs");
    // Append as a trailing comment on the document root.
    let raw = doc.to_string();
    let output = format!("{raw}\n{comment}");

    Ok(MigrationResult {
        output,
        added_count: 1,
        sections_added: vec!["magic_docs".to_owned()],
    })
}

/// Add a commented-out `[telemetry]` block if the section is absent (#2846).
///
/// Existing configs that were written before the `telemetry` section was introduced will have
/// the block appended as comments so users can discover and enable it without manual hunting.
///
/// # Errors
///
/// Returns `MigrateError::Parse` if `toml_src` is not valid TOML.
pub fn migrate_telemetry_config(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    let doc = toml_src.parse::<toml_edit::DocumentMut>()?;

    if doc.contains_key("telemetry") || toml_src.contains("# [telemetry]") {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            added_count: 0,
            sections_added: Vec::new(),
        });
    }

    let comment = "\n\
         # Profiling and distributed tracing (requires --features profiling). All\n\
         # instrumentation points are zero-overhead when the feature is absent.\n\
         # [telemetry]\n\
         # enabled = false\n\
         # backend = \"local\"        # \"local\" (Chrome JSON), \"otlp\", or \"pyroscope\"\n\
         # trace_dir = \".local/traces\"\n\
         # include_args = false\n\
         # service_name = \"zeph-agent\"\n\
         # sample_rate = 1.0\n\
         # otel_filter = \"info\"     # base EnvFilter for OTLP layer; noisy-crate exclusions always appended\n";

    let raw = doc.to_string();
    let output = format!("{raw}{comment}");

    Ok(MigrationResult {
        output,
        added_count: 1,
        sections_added: vec!["telemetry".to_owned()],
    })
}

/// Add a commented-out `[agent.supervisor]` block if the sub-table is absent (#2883).
///
/// Appended as comments under `[agent]` so users can discover and tune supervisor limits
/// without manual hunting. Safe to call on configs that already have the section.
///
/// # Errors
///
/// Returns `MigrateError::Parse` if `toml_src` is not valid TOML.
pub fn migrate_supervisor_config(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    // Idempotency: skip if already present (either as real section or commented-out block).
    if toml_src.contains("[agent.supervisor]") || toml_src.contains("# [agent.supervisor]") {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            added_count: 0,
            sections_added: Vec::new(),
        });
    }

    let doc = toml_src.parse::<toml_edit::DocumentMut>()?;

    // Only inject the comment block when an [agent] section is already present so we don't
    // pollute configs that have no [agent] at all.
    if !doc.contains_key("agent") {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            added_count: 0,
            sections_added: Vec::new(),
        });
    }

    let comment = "\n\
         # Background task supervisor tuning (optional — defaults shown, #2883).\n\
         # [agent.supervisor]\n\
         # enrichment_limit = 4\n\
         # telemetry_limit = 8\n\
         # abort_enrichment_on_turn = false\n";

    let raw = doc.to_string();
    let output = format!("{raw}{comment}");

    Ok(MigrationResult {
        output,
        added_count: 1,
        sections_added: vec!["agent.supervisor".to_owned()],
    })
}

/// Add a commented-out `otel_filter` entry under `[telemetry]` if the key is absent (#2997).
///
/// When `[telemetry]` exists but lacks `otel_filter`, appends the key as a comment so users
/// can discover it without manual hunting. Safe to call when the key is already present
/// (real or commented-out).
///
/// # Errors
///
/// Returns `MigrateError::Parse` if `toml_src` is not valid TOML.
pub fn migrate_otel_filter(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    // Idempotency: skip if key already present (real or commented-out).
    if toml_src.contains("otel_filter") {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            added_count: 0,
            sections_added: Vec::new(),
        });
    }

    let mut doc = toml_src.parse::<toml_edit::DocumentMut>()?;

    // Only inject when [telemetry] section exists; otherwise the field will be added
    // by migrate_telemetry_config which already includes it in the commented block.
    if !doc.contains_key("telemetry") {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            added_count: 0,
            sections_added: Vec::new(),
        });
    }

    let telemetry = doc
        .get_mut("telemetry")
        .and_then(toml_edit::Item::as_table_mut)
        .ok_or(MigrateError::InvalidStructure("[telemetry] is not a table"))?;

    // Insert within the [telemetry] section via suffix decor so the comment appears
    // adjacent to its section even when other sections follow.
    let comment = "# Base EnvFilter for the OTLP tracing layer. Noisy-crate exclusions \
        (tonic=warn etc.) are always appended (#2997).\n\
        # otel_filter = \"info\"\n";
    append_comment_to_table_suffix(telemetry, comment);

    Ok(MigrationResult {
        output: doc.to_string(),
        added_count: 1,
        sections_added: vec!["telemetry.otel_filter".to_owned()],
    })
}

/// Adds a commented-out `[tools.egress]` section to configs that predate egress logging (#3058).
///
/// # Errors
///
/// Returns [`MigrateError`] if the TOML source cannot be parsed.
pub fn migrate_egress_config(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    if toml_src.contains("[tools.egress]") || toml_src.contains("tools.egress") {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            added_count: 0,
            sections_added: Vec::new(),
        });
    }

    let comment = "\n# Egress network logging — records outbound HTTP requests to the audit log\n\
        # with per-hop correlation IDs, response metadata, and block reasons (#3058).\n\
        # [tools.egress]\n\
        # enabled = true           # set to false to disable all egress event recording\n\
        # log_blocked = true       # record scheme/domain/SSRF-blocked requests\n\
        # log_response_bytes = true\n\
        # log_hosts_to_tui = true\n";

    let mut output = toml_src.to_owned();
    output.push_str(comment);
    Ok(MigrationResult {
        output,
        added_count: 1,
        sections_added: vec!["tools.egress".to_owned()],
    })
}

/// Adds a commented-out `[security.vigil]` section to configs that predate VIGIL (#3058).
///
/// # Errors
///
/// Returns [`MigrateError`] if the TOML source cannot be parsed.
pub fn migrate_vigil_config(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    if toml_src.contains("[security.vigil]") || toml_src.contains("security.vigil") {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            added_count: 0,
            sections_added: Vec::new(),
        });
    }

    let comment = "\n# VIGIL verify-before-commit intent-anchoring gate (#3058).\n\
        # Runs a regex tripwire on every tool output before it enters LLM context.\n\
        # [security.vigil]\n\
        # enabled = true          # master switch; false bypasses VIGIL entirely\n\
        # strict_mode = false     # true: block (replace with sentinel); false: truncate+annotate\n\
        # sanitize_max_chars = 2048\n\
        # extra_patterns = []     # operator-supplied additional injection patterns (max 64)\n\
        # exempt_tools = [\"memory_search\", \"read_overflow\", \"load_skill\", \"schedule_deferred\"]\n";

    let mut output = toml_src.to_owned();
    output.push_str(comment);
    Ok(MigrationResult {
        output,
        added_count: 1,
        sections_added: vec!["security.vigil".to_owned()],
    })
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
        let user = "[security]\nredact_secrets = true\n";
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

    #[test]
    fn migrate_reference_contains_probe_section() {
        // default.toml must contain the probe section comment block so users can discover it
        // when reading the file directly or after running --migrate-config.
        let reference = include_str!("../config/default.toml");
        assert!(
            reference.contains("[memory.compression.probe]"),
            "default.toml must contain [memory.compression.probe] section comment"
        );
        assert!(
            reference.contains("hard_fail_threshold"),
            "probe section must include hard_fail_threshold default"
        );
    }

    // ─── migrate_llm_to_providers ─────────────────────────────────────────────

    #[test]
    fn migrate_llm_no_llm_section_is_noop() {
        let src = "[agent]\nname = \"Zeph\"\n";
        let result = migrate_llm_to_providers(src).expect("migrate");
        assert_eq!(result.added_count, 0);
        assert_eq!(result.output, src);
    }

    #[test]
    fn migrate_llm_already_new_format_is_noop() {
        let src = r#"
[llm]
[[llm.providers]]
type = "ollama"
model = "qwen3:8b"
"#;
        let result = migrate_llm_to_providers(src).expect("migrate");
        assert_eq!(result.added_count, 0);
    }

    #[test]
    fn migrate_llm_ollama_produces_providers_block() {
        let src = r#"
[llm]
provider = "ollama"
model = "qwen3:8b"
base_url = "http://localhost:11434"
embedding_model = "nomic-embed-text"
"#;
        let result = migrate_llm_to_providers(src).expect("migrate");
        assert!(
            result.output.contains("[[llm.providers]]"),
            "should contain [[llm.providers]]:\n{}",
            result.output
        );
        assert!(
            result.output.contains("type = \"ollama\""),
            "{}",
            result.output
        );
        assert!(
            result.output.contains("model = \"qwen3:8b\""),
            "{}",
            result.output
        );
    }

    #[test]
    fn migrate_llm_claude_produces_providers_block() {
        let src = r#"
[llm]
provider = "claude"

[llm.cloud]
model = "claude-sonnet-4-6"
max_tokens = 8192
server_compaction = true
"#;
        let result = migrate_llm_to_providers(src).expect("migrate");
        assert!(
            result.output.contains("[[llm.providers]]"),
            "{}",
            result.output
        );
        assert!(
            result.output.contains("type = \"claude\""),
            "{}",
            result.output
        );
        assert!(
            result.output.contains("model = \"claude-sonnet-4-6\""),
            "{}",
            result.output
        );
        assert!(
            result.output.contains("server_compaction = true"),
            "{}",
            result.output
        );
    }

    #[test]
    fn migrate_llm_openai_copies_fields() {
        let src = r#"
[llm]
provider = "openai"

[llm.openai]
base_url = "https://api.openai.com/v1"
model = "gpt-4o"
max_tokens = 4096
"#;
        let result = migrate_llm_to_providers(src).expect("migrate");
        assert!(
            result.output.contains("type = \"openai\""),
            "{}",
            result.output
        );
        assert!(
            result
                .output
                .contains("base_url = \"https://api.openai.com/v1\""),
            "{}",
            result.output
        );
    }

    #[test]
    fn migrate_llm_gemini_copies_fields() {
        let src = r#"
[llm]
provider = "gemini"

[llm.gemini]
model = "gemini-2.0-flash"
max_tokens = 8192
base_url = "https://generativelanguage.googleapis.com"
"#;
        let result = migrate_llm_to_providers(src).expect("migrate");
        assert!(
            result.output.contains("type = \"gemini\""),
            "{}",
            result.output
        );
        assert!(
            result.output.contains("model = \"gemini-2.0-flash\""),
            "{}",
            result.output
        );
    }

    #[test]
    fn migrate_llm_compatible_copies_multiple_entries() {
        let src = r#"
[llm]
provider = "compatible"

[[llm.compatible]]
name = "proxy-a"
base_url = "http://proxy-a:8080/v1"
model = "llama3"
max_tokens = 4096

[[llm.compatible]]
name = "proxy-b"
base_url = "http://proxy-b:8080/v1"
model = "mistral"
max_tokens = 2048
"#;
        let result = migrate_llm_to_providers(src).expect("migrate");
        // Both compatible entries should be emitted.
        let count = result.output.matches("[[llm.providers]]").count();
        assert_eq!(
            count, 2,
            "expected 2 [[llm.providers]] blocks:\n{}",
            result.output
        );
        assert!(
            result.output.contains("name = \"proxy-a\""),
            "{}",
            result.output
        );
        assert!(
            result.output.contains("name = \"proxy-b\""),
            "{}",
            result.output
        );
    }

    #[test]
    fn migrate_llm_mixed_format_errors() {
        // Legacy + new format together should produce an error.
        let src = r#"
[llm]
provider = "ollama"

[[llm.providers]]
type = "ollama"
"#;
        assert!(
            migrate_llm_to_providers(src).is_err(),
            "mixed format must return error"
        );
    }

    // ─── migrate_stt_to_provider ──────────────────────────────────────────────

    #[test]
    fn stt_migration_no_stt_section_returns_unchanged() {
        let src = "[llm]\n\n[[llm.providers]]\ntype = \"openai\"\nname = \"quality\"\nmodel = \"gpt-5.4\"\n";
        let result = migrate_stt_to_provider(src).unwrap();
        assert_eq!(result.added_count, 0);
        assert_eq!(result.output, src);
    }

    #[test]
    fn stt_migration_no_model_or_base_url_returns_unchanged() {
        let src = "[llm]\n\n[[llm.providers]]\ntype = \"openai\"\nname = \"quality\"\n\n[llm.stt]\nprovider = \"quality\"\nlanguage = \"en\"\n";
        let result = migrate_stt_to_provider(src).unwrap();
        assert_eq!(result.added_count, 0);
    }

    #[test]
    fn stt_migration_moves_model_to_provider_entry() {
        let src = r#"
[llm]

[[llm.providers]]
type = "openai"
name = "quality"
model = "gpt-5.4"

[llm.stt]
provider = "quality"
model = "gpt-4o-mini-transcribe"
language = "en"
"#;
        let result = migrate_stt_to_provider(src).unwrap();
        assert_eq!(result.added_count, 1);
        // stt_model should appear in providers entry.
        assert!(
            result.output.contains("stt_model"),
            "stt_model must be in output"
        );
        // model should be removed from [llm.stt].
        // The output should parse cleanly.
        let doc: toml_edit::DocumentMut = result.output.parse().unwrap();
        let stt = doc
            .get("llm")
            .and_then(toml_edit::Item::as_table)
            .and_then(|l| l.get("stt"))
            .and_then(toml_edit::Item::as_table)
            .unwrap();
        assert!(
            stt.get("model").is_none(),
            "model must be removed from [llm.stt]"
        );
        assert_eq!(
            stt.get("provider").and_then(toml_edit::Item::as_str),
            Some("quality")
        );
    }

    #[test]
    fn stt_migration_creates_new_provider_when_no_match() {
        let src = r#"
[llm]

[[llm.providers]]
type = "ollama"
name = "local"
model = "qwen3:8b"

[llm.stt]
provider = "whisper"
model = "whisper-1"
base_url = "https://api.openai.com/v1"
language = "en"
"#;
        let result = migrate_stt_to_provider(src).unwrap();
        assert!(
            result.output.contains("openai-stt"),
            "new entry name must be openai-stt"
        );
        assert!(
            result.output.contains("stt_model"),
            "stt_model must be in output"
        );
    }

    #[test]
    fn stt_migration_candle_whisper_creates_candle_entry() {
        let src = r#"
[llm]

[llm.stt]
provider = "candle-whisper"
model = "openai/whisper-tiny"
language = "auto"
"#;
        let result = migrate_stt_to_provider(src).unwrap();
        assert!(
            result.output.contains("local-whisper"),
            "candle entry name must be local-whisper"
        );
        assert!(result.output.contains("candle"), "type must be candle");
    }

    #[test]
    fn stt_migration_w2_assigns_explicit_name() {
        // Provider has no explicit name (type = "openai") — migration must assign one.
        let src = r#"
[llm]

[[llm.providers]]
type = "openai"
model = "gpt-5.4"

[llm.stt]
provider = "openai"
model = "whisper-1"
language = "auto"
"#;
        let result = migrate_stt_to_provider(src).unwrap();
        let doc: toml_edit::DocumentMut = result.output.parse().unwrap();
        let providers = doc
            .get("llm")
            .and_then(toml_edit::Item::as_table)
            .and_then(|l| l.get("providers"))
            .and_then(toml_edit::Item::as_array_of_tables)
            .unwrap();
        let entry = providers
            .iter()
            .find(|t| t.get("stt_model").is_some())
            .unwrap();
        // Must have an explicit `name` field (W2).
        assert!(
            entry.get("name").is_some(),
            "migrated entry must have explicit name"
        );
    }

    #[test]
    fn stt_migration_removes_base_url_from_stt_table() {
        // MEDIUM: verify that base_url is stripped from [llm.stt] after migration.
        let src = r#"
[llm]

[[llm.providers]]
type = "openai"
name = "quality"
model = "gpt-5.4"

[llm.stt]
provider = "quality"
model = "whisper-1"
base_url = "https://api.openai.com/v1"
language = "en"
"#;
        let result = migrate_stt_to_provider(src).unwrap();
        let doc: toml_edit::DocumentMut = result.output.parse().unwrap();
        let stt = doc
            .get("llm")
            .and_then(toml_edit::Item::as_table)
            .and_then(|l| l.get("stt"))
            .and_then(toml_edit::Item::as_table)
            .unwrap();
        assert!(
            stt.get("model").is_none(),
            "model must be removed from [llm.stt]"
        );
        assert!(
            stt.get("base_url").is_none(),
            "base_url must be removed from [llm.stt]"
        );
    }

    #[test]
    fn migrate_planner_model_to_provider_with_field() {
        let input = r#"
[orchestration]
enabled = true
planner_model = "gpt-4o"
max_tasks = 20
"#;
        let result = migrate_planner_model_to_provider(input).expect("migration must succeed");
        assert_eq!(result.added_count, 1, "added_count must be 1");
        assert!(
            !result.output.contains("planner_model = "),
            "planner_model key must be removed from output"
        );
        assert!(
            result.output.contains("# planner_provider"),
            "commented-out planner_provider entry must be present"
        );
        assert!(
            result.output.contains("gpt-4o"),
            "old value must appear in the comment"
        );
        assert!(
            result.output.contains("MIGRATED"),
            "comment must include MIGRATED marker"
        );
    }

    #[test]
    fn migrate_planner_model_to_provider_no_op() {
        let input = r"
[orchestration]
enabled = true
max_tasks = 20
";
        let result = migrate_planner_model_to_provider(input).expect("migration must succeed");
        assert_eq!(
            result.added_count, 0,
            "added_count must be 0 when field is absent"
        );
        assert_eq!(
            result.output, input,
            "output must equal input when nothing to migrate"
        );
    }

    #[test]
    fn migrate_error_invalid_structure_formats_correctly() {
        // HIGH: verify that MigrateError::InvalidStructure exists, matches correctly, and
        // produces a human-readable message. The error path is triggered when the [llm] item
        // is present but cannot be obtained as a mutable table (defensive guard replacing the
        // previous .expect() calls that would have panicked).
        let err = MigrateError::InvalidStructure("test sentinel");
        assert!(
            matches!(err, MigrateError::InvalidStructure(_)),
            "variant must match"
        );
        let msg = err.to_string();
        assert!(
            msg.contains("invalid TOML structure"),
            "error message must mention 'invalid TOML structure', got: {msg}"
        );
        assert!(
            msg.contains("test sentinel"),
            "message must include reason: {msg}"
        );
    }

    // ─── migrate_mcp_trust_levels ─────────────────────────────────────────────

    #[test]
    fn migrate_mcp_trust_levels_adds_trusted_to_entries_without_field() {
        let src = r#"
[mcp]
allowed_commands = ["npx"]

[[mcp.servers]]
id = "srv-a"
command = "npx"
args = ["-y", "some-mcp"]

[[mcp.servers]]
id = "srv-b"
command = "npx"
args = ["-y", "other-mcp"]
"#;
        let result = migrate_mcp_trust_levels(src).expect("migrate");
        assert_eq!(
            result.added_count, 2,
            "both entries must get trust_level added"
        );
        assert!(
            result
                .sections_added
                .contains(&"mcp.servers.trust_level".to_owned()),
            "sections_added must report mcp.servers.trust_level"
        );
        // Both entries must now contain trust_level = "trusted"
        let occurrences = result.output.matches("trust_level = \"trusted\"").count();
        assert_eq!(
            occurrences, 2,
            "each entry must have trust_level = \"trusted\""
        );
    }

    #[test]
    fn migrate_mcp_trust_levels_does_not_overwrite_existing_field() {
        let src = r#"
[[mcp.servers]]
id = "srv-a"
command = "npx"
trust_level = "sandboxed"
tool_allowlist = ["read_file"]

[[mcp.servers]]
id = "srv-b"
command = "npx"
"#;
        let result = migrate_mcp_trust_levels(src).expect("migrate");
        // Only srv-b has no trust_level, so only 1 entry should be updated
        assert_eq!(
            result.added_count, 1,
            "only entry without trust_level gets updated"
        );
        // srv-a's sandboxed value must not be overwritten
        assert!(
            result.output.contains("trust_level = \"sandboxed\""),
            "existing trust_level must not be overwritten"
        );
        // srv-b gets trusted
        assert!(
            result.output.contains("trust_level = \"trusted\""),
            "entry without trust_level must get trusted"
        );
    }

    #[test]
    fn migrate_mcp_trust_levels_no_mcp_section_is_noop() {
        let src = "[agent]\nname = \"Zeph\"\n";
        let result = migrate_mcp_trust_levels(src).expect("migrate");
        assert_eq!(result.added_count, 0);
        assert!(result.sections_added.is_empty());
        assert_eq!(result.output, src);
    }

    #[test]
    fn migrate_mcp_trust_levels_no_servers_is_noop() {
        let src = "[mcp]\nallowed_commands = [\"npx\"]\n";
        let result = migrate_mcp_trust_levels(src).expect("migrate");
        assert_eq!(result.added_count, 0);
        assert!(result.sections_added.is_empty());
        assert_eq!(result.output, src);
    }

    #[test]
    fn migrate_mcp_trust_levels_all_entries_already_have_field_is_noop() {
        let src = r#"
[[mcp.servers]]
id = "srv-a"
trust_level = "trusted"

[[mcp.servers]]
id = "srv-b"
trust_level = "untrusted"
"#;
        let result = migrate_mcp_trust_levels(src).expect("migrate");
        assert_eq!(result.added_count, 0);
        assert!(result.sections_added.is_empty());
    }

    #[test]
    fn migrate_database_url_adds_comment_when_absent() {
        let src = "[memory]\nsqlite_path = \"/tmp/zeph.db\"\n";
        let result = migrate_database_url(src).expect("migrate");
        assert_eq!(result.added_count, 1);
        assert!(
            result
                .sections_added
                .contains(&"memory.database_url".to_owned())
        );
        assert!(result.output.contains("# database_url = \"\""));
    }

    #[test]
    fn migrate_database_url_is_noop_when_present() {
        let src = "[memory]\nsqlite_path = \"/tmp/zeph.db\"\ndatabase_url = \"postgres://localhost/zeph\"\n";
        let result = migrate_database_url(src).expect("migrate");
        assert_eq!(result.added_count, 0);
        assert!(result.sections_added.is_empty());
        assert_eq!(result.output, src);
    }

    #[test]
    fn migrate_database_url_creates_memory_section_when_absent() {
        let src = "[agent]\nname = \"Zeph\"\n";
        let result = migrate_database_url(src).expect("migrate");
        assert_eq!(result.added_count, 1);
        assert!(result.output.contains("# database_url = \"\""));
    }

    // ── migrate_agent_budget_hint tests (#2267) ───────────────────────────────

    #[test]
    fn migrate_agent_budget_hint_adds_comment_to_existing_agent_section() {
        let src = "[agent]\nname = \"Zeph\"\n";
        let result = migrate_agent_budget_hint(src).expect("migrate");
        assert_eq!(result.added_count, 1);
        assert!(result.output.contains("budget_hint_enabled"));
        assert!(
            result
                .sections_added
                .contains(&"agent.budget_hint_enabled".to_owned())
        );
    }

    #[test]
    fn migrate_agent_budget_hint_no_agent_section_is_noop() {
        let src = "[llm]\nmodel = \"gpt-4o\"\n";
        let result = migrate_agent_budget_hint(src).expect("migrate");
        assert_eq!(result.added_count, 0);
        assert_eq!(result.output, src);
    }

    #[test]
    fn migrate_agent_budget_hint_already_present_is_noop() {
        let src = "[agent]\nname = \"Zeph\"\nbudget_hint_enabled = true\n";
        let result = migrate_agent_budget_hint(src).expect("migrate");
        assert_eq!(result.added_count, 0);
        assert_eq!(result.output, src);
    }

    #[test]
    fn migrate_telemetry_config_empty_config_appends_comment_block() {
        let src = "[agent]\nname = \"Zeph\"\n";
        let result = migrate_telemetry_config(src).expect("migrate");
        assert_eq!(result.added_count, 1);
        assert_eq!(result.sections_added, vec!["telemetry"]);
        assert!(
            result.output.contains("# [telemetry]"),
            "expected commented-out [telemetry] block in output"
        );
        assert!(
            result.output.contains("enabled = false"),
            "expected enabled = false in telemetry comment block"
        );
    }

    #[test]
    fn migrate_telemetry_config_existing_section_is_noop() {
        let src = "[agent]\nname = \"Zeph\"\n\n[telemetry]\nenabled = true\n";
        let result = migrate_telemetry_config(src).expect("migrate");
        assert_eq!(result.added_count, 0);
        assert_eq!(result.output, src);
    }

    #[test]
    fn migrate_telemetry_config_existing_comment_is_noop() {
        // Idempotency: if the comment block was already added, don't append again.
        let src = "[agent]\nname = \"Zeph\"\n\n# [telemetry]\n# enabled = false\n";
        let result = migrate_telemetry_config(src).expect("migrate");
        assert_eq!(result.added_count, 0);
        assert_eq!(result.output, src);
    }

    // ── migrate_otel_filter tests (#2997) ─────────────────────────────────────

    #[test]
    fn migrate_otel_filter_already_present_is_noop() {
        // Real key present — must not modify.
        let src = "[telemetry]\nenabled = true\notel_filter = \"debug\"\n";
        let result = migrate_otel_filter(src).expect("migrate");
        assert_eq!(result.added_count, 0);
        assert_eq!(result.output, src);
    }

    #[test]
    fn migrate_otel_filter_commented_key_is_noop() {
        // Commented-out key already present — idempotent.
        let src = "[telemetry]\nenabled = true\n# otel_filter = \"info\"\n";
        let result = migrate_otel_filter(src).expect("migrate");
        assert_eq!(result.added_count, 0);
        assert_eq!(result.output, src);
    }

    #[test]
    fn migrate_otel_filter_no_telemetry_section_is_noop() {
        // [telemetry] absent — must not inject into wrong location.
        let src = "[agent]\nname = \"Zeph\"\n";
        let result = migrate_otel_filter(src).expect("migrate");
        assert_eq!(result.added_count, 0);
        assert_eq!(result.output, src);
        assert!(!result.output.contains("otel_filter"));
    }

    #[test]
    fn migrate_otel_filter_injects_within_telemetry_section() {
        let src = "[telemetry]\nenabled = true\n\n[agent]\nname = \"Zeph\"\n";
        let result = migrate_otel_filter(src).expect("migrate");
        assert_eq!(result.added_count, 1);
        assert_eq!(result.sections_added, vec!["telemetry.otel_filter"]);
        assert!(
            result.output.contains("otel_filter"),
            "otel_filter comment must appear"
        );
        // Comment must appear before [agent] — i.e., within the telemetry section.
        let otel_pos = result
            .output
            .find("otel_filter")
            .expect("otel_filter present");
        let agent_pos = result.output.find("[agent]").expect("[agent] present");
        assert!(
            otel_pos < agent_pos,
            "otel_filter comment should appear before [agent] section"
        );
    }
}
