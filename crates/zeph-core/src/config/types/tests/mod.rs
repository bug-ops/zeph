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
        toml = toml.replace(&actual, &placeholder);
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
    if cfg!(feature = "lsp-context") {
        insta::assert_snapshot!("config_default_snapshot", toml_str);
    } else {
        insta::assert_snapshot!("config_default_snapshot_no_lsp_context", toml_str);
    }
}
