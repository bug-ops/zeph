// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Llm provider, routing, stt, and embedding config migration steps.
//!
//! Extracted from the former `migrate/mod.rs` monolith (#4874). Shared TOML helpers,
//! the [`Migration`](super::Migration) trait, and the [`MIGRATIONS`](super::MIGRATIONS)
//! registry remain in the parent module.

use super::{MigrateError, MigrationResult, section_header_present, value_to_toml_string};

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
        if let Some(v) = cloud
            .get("prompt_cache_ttl")
            .and_then(toml_edit::Item::as_str)
        {
            if v != "ephemeral" {
                block.push_str(&format!("prompt_cache_ttl = \"{v}\"\n"));
            }
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

// Returns (provider_blocks, routing)
#[allow(clippy::format_push_string, clippy::collapsible_if, clippy::ref_option)]
fn migrate_orchestrator_provider(
    llm: &toml_edit::Table,
    model: &Option<String>,
    base_url: &Option<String>,
    embedding_model: &Option<String>,
) -> (Vec<String>, Option<String>) {
    let mut blocks = Vec::new();
    let routing = None;
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
    (blocks, routing)
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
/// Removes `routing = "task"` and `[llm.routes]` block lines from a raw TOML string.
///
/// Used as a pre-pass before `migrate_llm_to_providers` when the removed variant is detected.
fn strip_task_routing_keys(toml_src: &str) -> String {
    let mut in_routes_block = false;
    let mut out = Vec::new();
    for line in toml_src.lines() {
        let trimmed = line.trim();
        if trimmed == "[llm.routes]" {
            in_routes_block = true;
            continue;
        }
        if in_routes_block {
            // Exit the routes block when we hit the next section header.
            if trimmed.starts_with('[') {
                in_routes_block = false;
            } else {
                continue;
            }
        }
        // Strip bare `routing = "task"` assignment.
        if trimmed.starts_with("routing") && trimmed.contains("\"task\"") {
            continue;
        }
        out.push(line);
    }
    out.join("\n")
}

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
                changed_count: 0,
                sections_changed: Vec::new(),
            });
        }
    };

    // Pre-check: `routing = "task"` was removed as unimplemented (#3248).
    // Detect on the input document before any block transforms.
    if llm.get("routing").and_then(toml_edit::Item::as_str) == Some("task") {
        let routes_count = llm
            .get("routes")
            .and_then(toml_edit::Item::as_table)
            .map_or(0, toml_edit::Table::len);
        let msg = format!(
            "routing = \"task\" is no longer supported and has been removed (#3248). \
             {routes_count} route(s) in [llm.routes] will be dropped. \
             Falling back to default single-provider routing."
        );
        tracing::warn!("{msg}");
        eprintln!("WARNING: {msg}");
        // Strip the removed keys and re-run migration on the cleaned source.
        let cleaned = strip_task_routing_keys(toml_src);
        return migrate_llm_to_providers(&cleaned);
    }

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
            changed_count: 0,
            sections_changed: Vec::new(),
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
            let (blocks, r) =
                migrate_orchestrator_provider(llm, &model, &base_url, &embedding_model);
            provider_blocks.extend(blocks);
            routing = r;
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
            changed_count: 0,
            sections_changed: Vec::new(),
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

    for block in &provider_blocks {
        new_llm.push_str(block);
        new_llm.push('\n');
    }

    // Remove old [llm] section and all its sub-sections from the source,
    // then prepend the new section.
    let output = replace_llm_section(toml_src, &new_llm);

    Ok(MigrationResult {
        output,
        changed_count: provider_blocks.len(),
        sections_changed: vec!["llm.providers".to_owned()],
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

/// Fields extracted from `[llm.stt]` that drive the migration decision.
struct SttFields {
    model: Option<String>,
    base_url: Option<String>,
    provider_hint: String,
}

/// Extract migration-relevant fields from `[llm.stt]` in the parsed document.
fn extract_stt_fields(doc: &toml_edit::DocumentMut) -> SttFields {
    let stt_table = doc
        .get("llm")
        .and_then(toml_edit::Item::as_table)
        .and_then(|llm| llm.get("stt"))
        .and_then(toml_edit::Item::as_table);

    let model = stt_table
        .and_then(|stt| stt.get("model"))
        .and_then(toml_edit::Item::as_str)
        .map(ToOwned::to_owned);

    let base_url = stt_table
        .and_then(|stt| stt.get("base_url"))
        .and_then(toml_edit::Item::as_str)
        .map(ToOwned::to_owned);

    let provider_hint = stt_table
        .and_then(|stt| stt.get("provider"))
        .and_then(toml_edit::Item::as_str)
        .map(ToOwned::to_owned)
        .unwrap_or_default();

    SttFields {
        model,
        base_url,
        provider_hint,
    }
}

/// Find the index of the first `[[llm.providers]]` entry that matches `target_type` or
/// `provider_hint`, giving priority to explicit name/type matches over type-only matches.
fn find_matching_provider_index(
    doc: &toml_edit::DocumentMut,
    target_type: &str,
    provider_hint: &str,
) -> Option<usize> {
    let providers = doc
        .get("llm")
        .and_then(toml_edit::Item::as_table)
        .and_then(|llm| llm.get("providers"))
        .and_then(toml_edit::Item::as_array_of_tables)?;

    providers.iter().enumerate().find_map(|(i, t)| {
        let name = t
            .get("name")
            .and_then(toml_edit::Item::as_str)
            .unwrap_or("");
        let ptype = t
            .get("type")
            .and_then(toml_edit::Item::as_str)
            .unwrap_or("");
        // Match by explicit name hint or by type when hint is a legacy backend string.
        let name_match =
            !provider_hint.is_empty() && (name == provider_hint || ptype == provider_hint);
        let type_match = ptype == target_type;
        if name_match || type_match {
            Some(i)
        } else {
            None
        }
    })
}

/// Attach `stt_model` (and optionally `base_url`) to an existing `[[llm.providers]]` entry
/// at `idx`. Ensures the entry has an explicit `name` (W2 guard) and returns that name.
fn attach_stt_to_existing_provider(
    doc: &mut toml_edit::DocumentMut,
    idx: usize,
    stt_model: &str,
    stt_base_url: Option<&str>,
) -> Result<String, MigrateError> {
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
    entry.insert("stt_model", toml_edit::value(stt_model));
    if let Some(url) = stt_base_url
        && entry.get("base_url").is_none()
    {
        entry.insert("base_url", toml_edit::value(url));
    }
    Ok(entry_name)
}

/// Append a new `[[llm.providers]]` entry carrying `stt_model`, creating the array if absent.
/// Returns the name assigned to the new entry.
fn append_new_stt_provider(
    doc: &mut toml_edit::DocumentMut,
    target_type: &str,
    stt_model: &str,
    stt_base_url: Option<&str>,
) -> Result<String, MigrateError> {
    let new_name = if target_type == "candle" {
        "local-whisper".to_owned()
    } else {
        "openai-stt".to_owned()
    };
    let mut new_entry = toml_edit::Table::new();
    new_entry.insert("name", toml_edit::value(new_name.clone()));
    new_entry.insert("type", toml_edit::value(target_type));
    new_entry.insert("stt_model", toml_edit::value(stt_model));
    if let Some(url) = stt_base_url {
        new_entry.insert("base_url", toml_edit::value(url));
    }
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
    Ok(new_name)
}

/// Update `[llm.stt]`: set `provider` to `resolved_provider_name` and strip `model`/`base_url`.
fn rewrite_stt_section(doc: &mut toml_edit::DocumentMut, resolved_provider_name: &str) {
    if let Some(stt_table) = doc
        .get_mut("llm")
        .and_then(toml_edit::Item::as_table_mut)
        .and_then(|llm| llm.get_mut("stt"))
        .and_then(toml_edit::Item::as_table_mut)
    {
        stt_table.insert("provider", toml_edit::value(resolved_provider_name));
        stt_table.remove("model");
        stt_table.remove("base_url");
    }
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
pub fn migrate_stt_to_provider(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    let mut doc = toml_src.parse::<toml_edit::DocumentMut>()?;
    let stt = extract_stt_fields(&doc);

    // Nothing to migrate if [llm.stt] does not exist or already lacks the old fields.
    if stt.model.is_none() && stt.base_url.is_none() {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let stt_model = stt.model.unwrap_or_else(|| "whisper-1".to_owned());

    // Determine the target provider type based on provider hint.
    let target_type = match stt.provider_hint.as_str() {
        "candle-whisper" | "candle" => "candle",
        _ => "openai",
    };

    let resolved_name = match find_matching_provider_index(&doc, target_type, &stt.provider_hint) {
        Some(idx) => {
            attach_stt_to_existing_provider(&mut doc, idx, &stt_model, stt.base_url.as_deref())?
        }
        None => {
            append_new_stt_provider(&mut doc, target_type, &stt_model, stt.base_url.as_deref())?
        }
    };

    rewrite_stt_section(&mut doc, &resolved_name);

    Ok(MigrationResult {
        output: doc.to_string(),
        changed_count: 1,
        sections_changed: vec!["llm.providers.stt_model".to_owned()],
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
            changed_count: 0,
            sections_changed: Vec::new(),
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
        changed_count: 1,
        sections_changed: vec!["orchestration.planner_provider".to_owned()],
    })
}

/// Add `orchestrator_provider` as a commented-out entry in `[orchestration]` when absent.
///
/// # Errors
///
/// Returns [`MigrateError::Parse`] when `toml_src` is not valid TOML.
pub fn migrate_orchestration_orchestrator_provider(
    toml_src: &str,
) -> Result<MigrationResult, MigrateError> {
    if toml_src.contains("orchestrator_provider") {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let comment = "\n# Provider for scheduling-tier LLM calls (aggregation, predicate, verify fallback).\n\
         # Set to a cheap/fast model to reduce orchestration cost. Empty = primary provider.\n\
         # Add under the orchestration section in your config:\n\
         # orchestrator_provider = \"\"\n";

    Ok(MigrationResult {
        output: format!("{toml_src}{comment}"),
        changed_count: 1,
        sections_changed: vec!["orchestration".to_owned()],
    })
}

/// Add a commented-out `max_concurrent` hint to `[[llm.providers]]` entries when absent.
///
/// `max_concurrent` limits how many orchestrated sub-agent calls may be in-flight to a
/// given provider simultaneously. The field is optional (`None` = unlimited), so existing
/// configs continue to work without changes.
///
/// # Errors
///
/// Returns [`MigrateError::Parse`] when `toml_src` is not valid TOML.
pub fn migrate_provider_max_concurrent(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    if toml_src.contains("max_concurrent") {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    if !toml_src.contains("[[llm.providers]]") {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let comment = "\n# Optional: maximum concurrent sub-agent calls to this provider (admission control).\n\
         # Remove the comment to enable; omit or set to 0 for unlimited.\n\
         # max_concurrent = 4\n";

    Ok(MigrationResult {
        output: format!("{toml_src}{comment}"),
        changed_count: 1,
        sections_changed: vec!["llm.providers".to_owned()],
    })
}

// ── Migration trait and registry ────────────────────────────────────────────────────────────────

/// Step 45: add an advisory comment above `GonkaGate` provider entries pointing users to
/// the native Gonka provider option.
///
/// This is advisory only — no automatic conversion is performed. The comment informs
/// users that a native Gonka provider is now available and links to migration docs.
pub(crate) fn migrate_gonkagate_to_gonka(toml_src: &str) -> MigrationResult {
    // Advisory-only: add a comment before GonkaGate provider entries when found.
    const MARKER: &str = "# [migration] GonkaGate detected: consider migrating to type = \"gonka\"";

    if !toml_src.contains("gonkagate") {
        return MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: vec![],
        };
    }

    let mut changed_count = 0;
    let mut lines: Vec<String> = toml_src.lines().map(str::to_owned).collect();

    // Walk backwards from each line containing "gonkagate" to find the nearest preceding
    // [[llm.providers]] table header and insert the advisory comment before it.
    // We iterate indices in reverse so that inserting at a position does not shift later targets.
    let indices: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.contains("gonkagate"))
        .map(|(i, _)| i)
        .rev()
        .collect();

    for gonka_idx in indices {
        // Find the most recent [[...]] header at or before gonka_idx.
        let header_idx = (0..=gonka_idx)
            .rev()
            .find(|&i| lines[i].starts_with("[["))
            .unwrap_or(gonka_idx);

        // Skip if the comment is already present just before the header.
        let already_marked = header_idx > 0 && lines[header_idx - 1].contains(MARKER);
        if already_marked {
            continue;
        }

        lines.insert(
            header_idx,
            format!("{MARKER} (see docs/guides/gonka-native.md)"),
        );
        changed_count += 1;
    }

    let output = lines.join("\n");
    let output = if toml_src.ends_with('\n') {
        format!("{output}\n")
    } else {
        output
    };

    MigrationResult {
        output,
        changed_count,
        sections_changed: if changed_count > 0 {
            vec!["llm".into()]
        } else {
            vec![]
        },
    }
}

/// Advisory-only no-op step for Cocoon provider introduction.
///
/// Returns the input unchanged. Exists so the migration registry stays sequential
/// and `--migrate-config` informs users that Cocoon is now available.
///
/// # Errors
///
/// This function never returns an error.
pub fn migrate_cocoon_provider_notice(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    Ok(MigrationResult {
        output: toml_src.to_owned(),
        changed_count: 0,
        sections_changed: vec![],
    })
}

/// Rename `embed_provider` → `embedding_provider` in `[memory.semantic]`, `[index]`,
/// `[llm.coe]`, and `trace_extraction_embed_provider` → `trace_extraction_embedding_provider`
/// in `[learning]` (#4480).
///
/// All four keys are renamed in a single pass. Keys that are already using the new name,
/// or that do not appear in the config, are left untouched (idempotent).
///
/// # Errors
///
/// This implementation never returns an error; the `Result` return type
/// satisfies the [`Migration`](super::Migration) trait contract.
pub fn migrate_embed_provider_rename(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    // Fast-path: nothing to do when none of the old names are present.
    let has_old =
        toml_src.contains("embed_provider") || toml_src.contains("trace_extraction_embed_provider");
    if !has_old {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    // Line-oriented rename: replace `embed_provider` with `embedding_provider` and
    // `trace_extraction_embed_provider` with `trace_extraction_embedding_provider`.
    // Only rename standalone key occurrences (key = value lines), not values or comments.
    let mut changed_count = 0usize;
    let mut sections_changed = Vec::new();

    let output = toml_src
        .lines()
        .map(|line| {
            let trimmed = line.trim_start();
            // Rename trace_extraction_embed_provider first (longer prefix must come first)
            if trimmed.starts_with("trace_extraction_embed_provider") {
                let replaced = line.replacen(
                    "trace_extraction_embed_provider",
                    "trace_extraction_embedding_provider",
                    1,
                );
                changed_count += 1;
                if !sections_changed.contains(&"learning".to_owned()) {
                    sections_changed.push("learning".to_owned());
                }
                return replaced;
            }
            if trimmed.starts_with("embed_provider") {
                let replaced = line.replacen("embed_provider", "embedding_provider", 1);
                changed_count += 1;
                return replaced;
            }
            line.to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n");

    // Preserve trailing newline if the original had one.
    let output = if toml_src.ends_with('\n') && !output.ends_with('\n') {
        format!("{output}\n")
    } else {
        output
    };

    Ok(MigrationResult {
        output,
        changed_count,
        sections_changed,
    })
}

/// Add a commented-out `[cocoon]` section with `show_balance` advisory notice (#4649).
///
/// Implements spec §15.2 opt-in redaction: when `show_balance = false`, the TUI
/// status bar renders the TON balance as `*** TON` instead of the real value.
/// The default remains `true` (visible), so existing configs are unaffected.
///
/// Idempotent: skipped if `show_balance` is already present (as an active key or comment).
///
/// # Errors
///
/// Infallible in practice; `Result` matches the migration convention.
pub fn migrate_cocoon_show_balance(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    if toml_src.contains("show_balance") {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let section = "\n[cocoon]\n\
        # show_balance = true  \
        # set to false to redact TON balance in TUI status bar (spec §15.2) (#4649)\n";
    let output = format!("{toml_src}{section}");
    Ok(MigrationResult {
        output,
        changed_count: 1,
        sections_changed: vec!["cocoon".to_owned()],
    })
}

/// Add a commented-out `[llm.stream_limits]` section when `[llm]` is present but the
/// section is absent (#4750).
///
/// All three fields carry compile-time defaults that reproduce pre-existing behavior, so
/// existing deployments that skip the section are unaffected.
///
/// # Errors
///
/// This function is infallible in practice; the `Result` return type matches the
/// migration function convention for use in chained pipelines.
pub fn migrate_llm_stream_limits(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    if section_header_present(toml_src, "llm.stream_limits") || !toml_src.contains("[llm]") {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let comment = "\n# SSE streaming buffer caps (#4750). Defaults match pre-existing behavior.\n\
        # [llm.stream_limits]\n\
        # max_tool_json_bytes  = 4194304   # 4 MiB\n\
        # max_thinking_bytes   = 1048576   # 1 MiB\n\
        # max_compaction_bytes = 32768     # 32 KiB\n";

    let output = format!("{toml_src}{comment}");
    Ok(MigrationResult {
        output,
        changed_count: 1,
        sections_changed: vec!["llm.stream_limits".to_owned()],
    })
}
