// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

mod features;
mod memory;

use super::super::*;

fn normalize_runtime_paths_for_snapshot(mut toml: String) -> String {
    for (actual, placeholder) in [
        (default_skills_dir(), "<DEFAULT_SKILLS_DIR>".to_owned()),
        (default_sqlite_path(), "<DEFAULT_SQLITE_PATH>".to_owned()),
        (
            default_debug_dir().to_string_lossy().into_owned(),
            "<DEFAULT_DEBUG_DIR>".to_owned(),
        ),
        (default_log_file_path(), "<DEFAULT_LOG_FILE>".to_owned()),
    ] {
        // TOML serializes Windows backslashes as `\\`; replace the escaped
        // form first so both platforms produce identical snapshots.
        toml = toml.replace(&actual.replace('\\', "\\\\"), &placeholder);
        toml = toml.replace(&actual, &placeholder);
    }

    // TOML may use single quotes for Windows paths containing backslashes.
    // After replacement, normalize single-quoted placeholders to double quotes
    // so snapshots are consistent across platforms.
    for placeholder in [
        "<DEFAULT_SKILLS_DIR>",
        "<DEFAULT_SQLITE_PATH>",
        "<DEFAULT_DEBUG_DIR>",
        "<DEFAULT_LOG_FILE>",
    ] {
        let single = format!("'{placeholder}'");
        let double = format!("\"{placeholder}\"");
        toml = toml.replace(&single, &double);
    }

    toml
}

#[test]
fn config_serialize_roundtrip() {
    let config = Config::default();
    let toml_str = toml::to_string_pretty(&config).expect("serialize");
    let back: Config = toml::from_str(&toml_str).expect("deserialize");
    assert_eq!(back.agent.name, config.agent.name);
    assert_eq!(back.llm.provider, config.llm.provider);
    assert_eq!(back.llm.model, config.llm.model);
    assert_eq!(back.memory.sqlite_path, config.memory.sqlite_path);
    assert_eq!(back.memory.history_limit, config.memory.history_limit);
    assert_eq!(back.vault.backend, config.vault.backend);
    assert_eq!(back.agent.auto_update_check, config.agent.auto_update_check);
}

#[test]
fn config_default_snapshot() {
    let config = Config::default();
    let toml_str =
        normalize_runtime_paths_for_snapshot(toml::to_string_pretty(&config).expect("serialize"));
    let snapshot_name = match (
        cfg!(feature = "lsp-context"),
        cfg!(feature = "policy-enforcer"),
    ) {
        (true, true) => "config_default_snapshot_lsp_policy",
        (true, false) => "config_default_snapshot",
        (false, true) => "config_default_snapshot_no_lsp_policy",
        (false, false) => "config_default_snapshot_no_lsp_context",
    };
    insta::assert_snapshot!(snapshot_name, toml_str);
}
