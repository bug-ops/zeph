// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Unit tests for config migration steps and the [`ConfigMigrator`](super::ConfigMigrator).

use super::*;

#[test]
fn migrations_registry_has_all_steps() {
    assert_eq!(
        MIGRATIONS.len(),
        76,
        "MIGRATIONS registry must contain all 76 sequential steps"
    );
    for m in MIGRATIONS.iter() {
        assert!(
            !m.name().is_empty(),
            "each migration must have a non-empty name"
        );
    }
}

#[test]
fn migrations_registry_applies_to_empty_config() {
    let mut toml = String::new();
    for m in MIGRATIONS.iter() {
        toml = m
            .apply(&toml)
            .expect("migration must not fail on empty config")
            .output;
    }
    // After all steps, the output should at minimum be valid TOML (parseable).
    toml.parse::<toml_edit::DocumentMut>()
        .expect("registry output must be valid TOML");
}

#[test]
fn empty_config_gets_sections_as_comments() {
    let migrator = ConfigMigrator::new();
    let result = migrator.migrate("").expect("migrate empty");
    // Should have added sections since reference is non-empty.
    assert!(result.changed_count > 0 || !result.sections_changed.is_empty());
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
    let reference = include_str!("../../config/default.toml");
    let migrator = ConfigMigrator::new();
    let result = migrator.migrate(reference).expect("migrate reference");
    assert_eq!(
        result.changed_count, 0,
        "migrating the canonical reference should add nothing (changed_count = {})",
        result.changed_count
    );
    assert!(
        result.sections_changed.is_empty(),
        "migrating the canonical reference should report no sections_changed: {:?}",
        result.sections_changed
    );
}

#[test]
fn empty_config_changed_count_is_positive() {
    // Stricter variant of empty_config_gets_sections_as_comments.
    let migrator = ConfigMigrator::new();
    let result = migrator.migrate("").expect("migrate empty");
    assert!(
        result.changed_count > 0,
        "empty config must report changed_count > 0"
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
    let reference = include_str!("../../config/default.toml");
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
    let reference = include_str!("../../config/default.toml");
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
    assert_eq!(result.changed_count, 0);
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
    assert_eq!(result.changed_count, 0);
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
model = "claude-sonnet-5"
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
        result.output.contains("model = \"claude-sonnet-5\""),
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
    let src =
        "[llm]\n\n[[llm.providers]]\ntype = \"openai\"\nname = \"quality\"\nmodel = \"gpt-5.4\"\n";
    let result = migrate_stt_to_provider(src).unwrap();
    assert_eq!(result.changed_count, 0);
    assert_eq!(result.output, src);
}

#[test]
fn stt_migration_no_model_or_base_url_returns_unchanged() {
    let src = "[llm]\n\n[[llm.providers]]\ntype = \"openai\"\nname = \"quality\"\n\n[llm.stt]\nprovider = \"quality\"\nlanguage = \"en\"\n";
    let result = migrate_stt_to_provider(src).unwrap();
    assert_eq!(result.changed_count, 0);
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
    assert_eq!(result.changed_count, 1);
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
    assert_eq!(result.changed_count, 1, "changed_count must be 1");
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
        result.changed_count, 0,
        "changed_count must be 0 when field is absent"
    );
    assert_eq!(
        result.output, input,
        "output must equal input when nothing to migrate"
    );
}

#[test]
fn migrate_eval_model_to_provider_with_field() {
    let input = r#"
[experiments]
enabled = true
eval_model = "claude-opus-4"
max_experiments = 20
"#;
    let result = migrate_eval_model_to_provider(input).expect("migration must succeed");
    assert_eq!(result.changed_count, 1, "changed_count must be 1");
    assert!(
        !result.output.contains("eval_model = "),
        "eval_model key must be removed from output"
    );
    assert!(
        result.output.contains("# eval_provider"),
        "commented-out eval_provider entry must be present"
    );
    assert!(
        result.output.contains("claude-opus-4"),
        "old value must appear in the comment"
    );
    assert!(
        result.output.contains("MIGRATED"),
        "comment must include MIGRATED marker"
    );
}

#[test]
fn migrate_eval_model_to_provider_no_op() {
    let input = r"
[experiments]
enabled = true
max_experiments = 20
";
    let result = migrate_eval_model_to_provider(input).expect("migration must succeed");
    assert_eq!(
        result.changed_count, 0,
        "changed_count must be 0 when field is absent"
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
        result.changed_count, 2,
        "both entries must get trust_level added"
    );
    assert!(
        result
            .sections_changed
            .contains(&"mcp.servers.trust_level".to_owned()),
        "sections_changed must report mcp.servers.trust_level"
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
        result.changed_count, 1,
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
    assert_eq!(result.changed_count, 0);
    assert!(result.sections_changed.is_empty());
    assert_eq!(result.output, src);
}

#[test]
fn migrate_mcp_trust_levels_no_servers_is_noop() {
    let src = "[mcp]\nallowed_commands = [\"npx\"]\n";
    let result = migrate_mcp_trust_levels(src).expect("migrate");
    assert_eq!(result.changed_count, 0);
    assert!(result.sections_changed.is_empty());
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
    assert_eq!(result.changed_count, 0);
    assert!(result.sections_changed.is_empty());
}

#[test]
fn migrate_database_url_adds_comment_when_absent() {
    let src = "[memory]\nsqlite_path = \"/tmp/zeph.db\"\n";
    let result = migrate_database_url(src).expect("migrate");
    assert_eq!(result.changed_count, 1);
    assert!(
        result
            .sections_changed
            .contains(&"memory.database_url".to_owned())
    );
    assert!(result.output.contains("# database_url = \"\""));
}

#[test]
fn migrate_database_url_is_noop_when_present() {
    let src =
        "[memory]\nsqlite_path = \"/tmp/zeph.db\"\ndatabase_url = \"postgres://localhost/zeph\"\n";
    let result = migrate_database_url(src).expect("migrate");
    assert_eq!(result.changed_count, 0);
    assert!(result.sections_changed.is_empty());
    assert_eq!(result.output, src);
}

#[test]
fn migrate_database_url_creates_memory_section_when_absent() {
    let src = "[agent]\nname = \"Zeph\"\n";
    let result = migrate_database_url(src).expect("migrate");
    assert_eq!(result.changed_count, 1);
    assert!(result.output.contains("# database_url = \"\""));
}

// ── migrate_agent_budget_hint tests (#2267) ───────────────────────────────

#[test]
fn migrate_agent_budget_hint_adds_comment_to_existing_agent_section() {
    let src = "[agent]\nname = \"Zeph\"\n";
    let result = migrate_agent_budget_hint(src).expect("migrate");
    assert_eq!(result.changed_count, 1);
    assert!(result.output.contains("budget_hint_enabled"));
    assert!(
        result
            .sections_changed
            .contains(&"agent.budget_hint_enabled".to_owned())
    );
}

#[test]
fn migrate_agent_budget_hint_no_agent_section_is_noop() {
    let src = "[llm]\nmodel = \"gpt-4o\"\n";
    let result = migrate_agent_budget_hint(src).expect("migrate");
    assert_eq!(result.changed_count, 0);
    assert_eq!(result.output, src);
}

#[test]
fn migrate_agent_budget_hint_already_present_is_noop() {
    let src = "[agent]\nname = \"Zeph\"\nbudget_hint_enabled = true\n";
    let result = migrate_agent_budget_hint(src).expect("migrate");
    assert_eq!(result.changed_count, 0);
    assert_eq!(result.output, src);
}

#[test]
fn migrate_telemetry_config_empty_config_appends_comment_block() {
    let src = "[agent]\nname = \"Zeph\"\n";
    let result = migrate_telemetry_config(src).expect("migrate");
    assert_eq!(result.changed_count, 1);
    assert_eq!(result.sections_changed, vec!["telemetry"]);
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
    assert_eq!(result.changed_count, 0);
    assert_eq!(result.output, src);
}

#[test]
fn migrate_telemetry_config_existing_comment_is_noop() {
    // Idempotency: if the comment block was already added, don't append again.
    let src = "[agent]\nname = \"Zeph\"\n\n# [telemetry]\n# enabled = false\n";
    let result = migrate_telemetry_config(src).expect("migrate");
    assert_eq!(result.changed_count, 0);
    assert_eq!(result.output, src);
}

// ── migrate_otel_filter tests (#2997) ─────────────────────────────────────

#[test]
fn migrate_otel_filter_already_present_is_noop() {
    // Real key present — must not modify.
    let src = "[telemetry]\nenabled = true\notel_filter = \"debug\"\n";
    let result = migrate_otel_filter(src).expect("migrate");
    assert_eq!(result.changed_count, 0);
    assert_eq!(result.output, src);
}

#[test]
fn migrate_otel_filter_commented_key_is_noop() {
    // Commented-out key already present — idempotent.
    let src = "[telemetry]\nenabled = true\n# otel_filter = \"info\"\n";
    let result = migrate_otel_filter(src).expect("migrate");
    assert_eq!(result.changed_count, 0);
    assert_eq!(result.output, src);
}

#[test]
fn migrate_otel_filter_no_telemetry_section_is_noop() {
    // [telemetry] absent — must not inject into wrong location.
    let src = "[agent]\nname = \"Zeph\"\n";
    let result = migrate_otel_filter(src).expect("migrate");
    assert_eq!(result.changed_count, 0);
    assert_eq!(result.output, src);
    assert!(!result.output.contains("otel_filter"));
}

#[test]
fn migrate_otel_filter_injects_within_telemetry_section() {
    let src = "[telemetry]\nenabled = true\n\n[agent]\nname = \"Zeph\"\n";
    let result = migrate_otel_filter(src).expect("migrate");
    assert_eq!(result.changed_count, 1);
    assert_eq!(result.sections_changed, vec!["telemetry.otel_filter"]);
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

#[test]
fn sandbox_migration_adds_commented_section_when_absent() {
    let src = "[agent]\nname = \"Z\"\n";
    let result = migrate_sandbox_config(src).expect("migrate sandbox");
    assert_eq!(result.changed_count, 1);
    assert!(result.output.contains("# [tools.sandbox]"));
    assert!(result.output.contains("# profile = \"workspace\""));
}

#[test]
fn sandbox_migration_noop_when_section_present() {
    let src = "[tools.sandbox]\nenabled = true\n";
    let result = migrate_sandbox_config(src).expect("migrate sandbox");
    assert_eq!(result.changed_count, 0);
}

#[test]
fn sandbox_migration_noop_when_dotted_key_present() {
    let src = "[tools]\nsandbox = { enabled = true }\n";
    let result = migrate_sandbox_config(src).expect("migrate sandbox");
    assert_eq!(result.changed_count, 0);
}

#[test]
fn sandbox_migration_false_positive_comment_does_not_block() {
    // Comments mentioning tools.sandbox must NOT suppress insertion.
    let src = "# tools.sandbox was planned for #3070\n[agent]\nname = \"Z\"\n";
    let result = migrate_sandbox_config(src).expect("migrate sandbox");
    assert_eq!(result.changed_count, 1);
}

#[test]
fn embedded_default_mentions_tools_sandbox() {
    let default_src = include_str!("../../config/default.toml");
    assert!(
        default_src.contains("tools.sandbox"),
        "embedded default.toml must include tools.sandbox for ConfigMigrator discovery"
    );
}

#[test]
fn sandbox_migration_idempotent_on_own_output() {
    let base = "[agent]\nmodel = \"test\"\n";
    let first = migrate_sandbox_config(base).unwrap();
    assert_eq!(first.changed_count, 1);
    let second = migrate_sandbox_config(&first.output).unwrap();
    assert_eq!(second.changed_count, 0, "second run must not double-append");
    assert_eq!(second.output, first.output);
}

#[test]
fn migrate_agent_budget_hint_idempotent_on_commented_output() {
    let base = "[agent]\nname = \"Zeph\"\n";
    let first = migrate_agent_budget_hint(base).unwrap();
    assert_eq!(first.changed_count, 1);
    let second = migrate_agent_budget_hint(&first.output).unwrap();
    assert_eq!(second.changed_count, 0, "second run must not double-append");
    assert_eq!(second.output, first.output);
}

#[test]
fn migrate_forgetting_config_idempotent_on_commented_output() {
    let base = "[memory]\ndb_path = \"~/.zeph/memory.db\"\n";
    let first = migrate_forgetting_config(base).unwrap();
    assert_eq!(first.changed_count, 1);
    let second = migrate_forgetting_config(&first.output).unwrap();
    assert_eq!(second.changed_count, 0, "second run must not double-append");
    assert_eq!(second.output, first.output);
}

#[test]
fn migrate_microcompact_config_idempotent_on_commented_output() {
    let base = "[memory]\ndb_path = \"~/.zeph/memory.db\"\n";
    let first = migrate_microcompact_config(base).unwrap();
    assert_eq!(first.changed_count, 1);
    let second = migrate_microcompact_config(&first.output).unwrap();
    assert_eq!(second.changed_count, 0, "second run must not double-append");
    assert_eq!(second.output, first.output);
}

#[test]
fn migrate_autodream_config_idempotent_on_commented_output() {
    let base = "[memory]\ndb_path = \"~/.zeph/memory.db\"\n";
    let first = migrate_autodream_config(base).unwrap();
    assert_eq!(first.changed_count, 1);
    let second = migrate_autodream_config(&first.output).unwrap();
    assert_eq!(second.changed_count, 0, "second run must not double-append");
    assert_eq!(second.output, first.output);
}

#[test]
fn migrate_compression_predictor_strips_active_section() {
    let base = "[memory]\ndb_path = \"test\"\n[memory.compression.predictor]\nenabled = false\nmin_samples = 10\n[memory.other]\nfoo = 1\n";
    let result = migrate_compression_predictor_config(base).unwrap();
    assert!(!result.output.contains("[memory.compression.predictor]"));
    assert!(!result.output.contains("min_samples"));
    assert!(result.output.contains("[memory.other]"));
    assert_eq!(result.changed_count, 1);
}

#[test]
fn migrate_compression_predictor_strips_commented_section() {
    let base = "[memory]\ndb_path = \"test\"\n# [memory.compression.predictor]\n# enabled = false\n[memory.other]\nfoo = 1\n";
    let result = migrate_compression_predictor_config(base).unwrap();
    assert!(!result.output.contains("compression.predictor"));
    assert!(result.output.contains("[memory.other]"));
}

#[test]
fn migrate_compression_predictor_idempotent() {
    let base = "[memory]\ndb_path = \"test\"\n[memory.compression.predictor]\nenabled = false\n[memory.other]\nfoo = 1\n";
    let first = migrate_compression_predictor_config(base).unwrap();
    let second = migrate_compression_predictor_config(&first.output).unwrap();
    assert_eq!(second.output, first.output);
    assert_eq!(second.changed_count, 0);
}

#[test]
fn migrate_compression_predictor_noop_when_absent() {
    let base = "[memory]\ndb_path = \"test\"\n";
    let result = migrate_compression_predictor_config(base).unwrap();
    assert_eq!(result.output, base);
    assert_eq!(result.changed_count, 0);
}

#[test]
fn migrate_database_url_idempotent_on_commented_output() {
    let base = "[memory]\ndb_path = \"~/.zeph/memory.db\"\n";
    let first = migrate_database_url(base).unwrap();
    assert_eq!(first.changed_count, 1);
    let second = migrate_database_url(&first.output).unwrap();
    assert_eq!(second.changed_count, 0, "second run must not double-append");
    assert_eq!(second.output, first.output);
}

#[test]
fn migrate_shell_transactional_idempotent_on_commented_output() {
    let base = "[tools]\n[tools.shell]\nallow_list = []\n";
    let first = migrate_shell_transactional(base).unwrap();
    assert_eq!(first.changed_count, 1);
    let second = migrate_shell_transactional(&first.output).unwrap();
    assert_eq!(second.changed_count, 0, "second run must not double-append");
    assert_eq!(second.output, first.output);
}

#[test]
fn migrate_otel_filter_idempotent_on_commented_output() {
    let base = "[telemetry]\nenabled = true\n";
    let first = migrate_otel_filter(base).unwrap();
    assert_eq!(first.changed_count, 1);
    let second = migrate_otel_filter(&first.output).unwrap();
    assert_eq!(second.changed_count, 0, "second run must not double-append");
    assert_eq!(second.output, first.output);
}

#[test]
fn config_migrator_does_not_suppress_duplicate_key_across_sections() {
    let migrator = ConfigMigrator::new();
    let src = "[telemetry]\nenabled = true\n\n[security]\n[security.content_isolation]\n";
    let result = migrator.migrate(src).expect("migrate");
    let sec_body_start = result
        .output
        .find("[security.content_isolation]")
        .unwrap_or(0);
    let sec_body = &result.output[sec_body_start..];
    let next_header = sec_body[1..].find("\n[").map_or(sec_body.len(), |p| p + 1);
    let sec_slice = &sec_body[..next_header];
    assert!(
        sec_slice.contains("# enabled"),
        "[security.content_isolation] body must contain `# enabled` hint; got: {sec_slice:?}"
    );
}

#[test]
fn config_migrator_idempotent_on_realistic_config() {
    let base = r#"
[agent]
name = "Zeph"

[memory]
db_path = "~/.zeph/memory.db"
soft_compaction_threshold = 0.6

[index]
max_chunks = 12

[tools]
[tools.shell]
allow_list = []

[telemetry]
enabled = false

[security]
[security.content_isolation]
enabled = true
"#;
    let migrator = ConfigMigrator::new();
    let first = migrator.migrate(base).expect("first migrate");
    let second = migrator.migrate(&first.output).expect("second migrate");
    assert_eq!(
        second.changed_count, 0,
        "second run of ConfigMigrator::migrate must add 0 entries, got {}",
        second.changed_count
    );
    assert_eq!(
        first.output, second.output,
        "output must be identical on second run"
    );
    for line in first.output.lines() {
        if line.starts_with('[') && !line.starts_with("[[") {
            assert!(
                !line.contains('#'),
                "section header must not have inline comment: {line:?}"
            );
        }
    }
}

#[test]
fn migrate_claude_prompt_cache_ttl_1h_survives() {
    let src = r#"
[llm]
provider = "claude"

[llm.cloud]
model = "claude-sonnet-5"
prompt_cache_ttl = "1h"
"#;
    let result = migrate_llm_to_providers(src).expect("migrate");
    assert!(
        result.output.contains("prompt_cache_ttl = \"1h\""),
        "1h TTL must be preserved in migrated output:\n{}",
        result.output
    );
}

#[test]
fn migrate_claude_prompt_cache_ttl_ephemeral_suppressed() {
    let src = r#"
[llm]
provider = "claude"

[llm.cloud]
model = "claude-sonnet-5"
prompt_cache_ttl = "ephemeral"
"#;
    let result = migrate_llm_to_providers(src).expect("migrate");
    assert!(
        !result.output.contains("prompt_cache_ttl"),
        "ephemeral TTL must be suppressed (M2 idempotency guard):\n{}",
        result.output
    );
}

#[test]
fn migrate_claude_prompt_cache_ttl_1h_idempotent() {
    let src = r#"
[[llm.providers]]
type = "claude"
model = "claude-sonnet-5"
prompt_cache_ttl = "1h"
"#;
    let migrator = ConfigMigrator::new();
    let first = migrator.migrate(src).expect("first migrate");
    let second = migrator.migrate(&first.output).expect("second migrate");
    assert_eq!(
        first.output, second.output,
        "migration must be idempotent when prompt_cache_ttl = \"1h\" already present"
    );
}

// ── migrate_session_recap_config ──────────────────────────────────────────

#[test]
fn migrate_session_recap_adds_block_when_absent() {
    let src = "[agent]\nname = \"Zeph\"\n";
    let result = migrate_session_recap_config(src).expect("migrate");
    assert_eq!(result.changed_count, 1);
    assert!(
        result
            .sections_changed
            .contains(&"session.recap".to_owned())
    );
    assert!(result.output.contains("# [session.recap]"));
    assert!(result.output.contains("on_resume = true"));
}

#[test]
fn migrate_session_recap_idempotent_on_commented_block() {
    let src = "[agent]\nname = \"Zeph\"\n# [session.recap]\n# on_resume = true\n";
    let result = migrate_session_recap_config(src).expect("migrate");
    assert_eq!(result.changed_count, 0);
    assert_eq!(result.output, src);
}

#[test]
fn migrate_session_recap_idempotent_on_active_section() {
    let src = "[agent]\nname = \"Zeph\"\n[session.recap]\non_resume = false\n";
    let result = migrate_session_recap_config(src).expect("migrate");
    assert_eq!(result.changed_count, 0);
    assert_eq!(result.output, src);
}

// ── migrate_mcp_elicitation_config ────────────────────────────────────────

#[test]
fn migrate_mcp_elicitation_adds_keys_when_absent() {
    let src = "[mcp]\nallowed_commands = []\n";
    let result = migrate_mcp_elicitation_config(src).expect("migrate");
    assert_eq!(result.changed_count, 1);
    assert!(
        result
            .sections_changed
            .contains(&"mcp.elicitation".to_owned())
    );
    assert!(result.output.contains("# elicitation_enabled = false"));
    assert!(result.output.contains("# elicitation_timeout = 120"));
}

#[test]
fn migrate_mcp_elicitation_idempotent_when_key_present() {
    let src = "[mcp]\nelicitation_enabled = true\n";
    let result = migrate_mcp_elicitation_config(src).expect("migrate");
    assert_eq!(result.changed_count, 0);
    assert_eq!(result.output, src);
}

#[test]
fn migrate_mcp_elicitation_skips_when_no_mcp_section() {
    let src = "[agent]\nname = \"Zeph\"\n";
    let result = migrate_mcp_elicitation_config(src).expect("migrate");
    assert_eq!(result.changed_count, 0);
    assert_eq!(result.output, src);
}

#[test]
fn migrate_mcp_elicitation_skips_without_trailing_newline() {
    // Edge case: `[mcp]` at EOF with no `\n` — replacen would be a no-op.
    let src = "[mcp]";
    let result = migrate_mcp_elicitation_config(src).expect("migrate");
    assert_eq!(result.changed_count, 0);
    assert_eq!(result.output, src);
}

// ── migrate_quality_config ────────────────────────────────────────────────

#[test]
fn migrate_quality_adds_block_when_absent() {
    let src = "[agent]\nname = \"Zeph\"\n";
    let result = migrate_quality_config(src).expect("migrate");
    assert_eq!(result.changed_count, 1);
    assert!(result.sections_changed.contains(&"quality".to_owned()));
    assert!(result.output.contains("# [quality]"));
    assert!(result.output.contains("self_check = false"));
    assert!(result.output.contains("trigger = \"has_retrieval\""));
}

#[test]
fn migrate_quality_idempotent_on_commented_block() {
    let src = "[agent]\nname = \"Zeph\"\n# [quality]\n# self_check = false\n";
    let result = migrate_quality_config(src).expect("migrate");
    assert_eq!(result.changed_count, 0);
    assert_eq!(result.output, src);
}

#[test]
fn migrate_quality_idempotent_on_active_section() {
    let src = "[agent]\nname = \"Zeph\"\n[quality]\nself_check = true\n";
    let result = migrate_quality_config(src).expect("migrate");
    assert_eq!(result.changed_count, 0);
    assert_eq!(result.output, src);
}

// ── migrate_acp_subagents_config ─────────────────────────────────────────

#[test]
fn migrate_acp_subagents_adds_block_when_absent() {
    let src = "[agent]\nname = \"Zeph\"\n";
    let result = migrate_acp_subagents_config(src).expect("migrate");
    assert_eq!(result.changed_count, 1);
    assert!(
        result
            .sections_changed
            .contains(&"acp.subagents".to_owned())
    );
    assert!(result.output.contains("# [acp.subagents]"));
    assert!(result.output.contains("enabled = false"));
}

#[test]
fn migrate_acp_subagents_idempotent_on_existing_block() {
    let src = "[agent]\nname = \"Zeph\"\n# [acp.subagents]\n# enabled = false\n";
    let result = migrate_acp_subagents_config(src).expect("migrate");
    assert_eq!(result.changed_count, 0);
    assert_eq!(result.output, src);
}

// ── migrate_hooks_permission_denied_config ────────────────────────────────

#[test]
fn migrate_hooks_permission_denied_adds_block_when_absent() {
    let src = "[agent]\nname = \"Zeph\"\n";
    let result = migrate_hooks_permission_denied_config(src).expect("migrate");
    assert_eq!(result.changed_count, 1);
    assert!(
        result
            .sections_changed
            .contains(&"hooks.permission_denied".to_owned())
    );
    assert!(result.output.contains("# [[hooks.permission_denied]]"));
    assert!(result.output.contains("ZEPH_TOOL"));
}

#[test]
fn migrate_hooks_permission_denied_idempotent_on_existing_block() {
    let src = "[agent]\nname = \"Zeph\"\n# [[hooks.permission_denied]]\n# type = \"command\"\n";
    let result = migrate_hooks_permission_denied_config(src).expect("migrate");
    assert_eq!(result.changed_count, 0);
    assert_eq!(result.output, src);
}

// ── migrate_memory_graph_config ───────────────────────────────────────────

#[test]
fn migrate_memory_graph_adds_block_when_absent() {
    let src = "[agent]\nname = \"Zeph\"\n";
    let result = migrate_memory_graph_config(src).expect("migrate");
    assert_eq!(result.changed_count, 1);
    assert!(
        result
            .sections_changed
            .contains(&"memory.graph.retrieval".to_owned())
    );
    assert!(result.output.contains("retrieval_strategy"));
    assert!(result.output.contains("# [memory.graph.beam_search]"));
}

#[test]
fn migrate_memory_graph_idempotent_on_existing_block() {
    let src = "[agent]\nname = \"Zeph\"\n# [memory.graph.beam_search]\n# beam_width = 10\n";
    let result = migrate_memory_graph_config(src).expect("migrate");
    assert_eq!(result.changed_count, 0);
    assert_eq!(result.output, src);
}

// ── migrate_scheduler_daemon_config ──────────────────────────────────────

#[test]
fn migrate_scheduler_daemon_adds_block_when_absent() {
    let src = "[agent]\nname = \"Zeph\"\n";
    let result = migrate_scheduler_daemon_config(src).expect("migrate");
    assert_eq!(result.changed_count, 1);
    assert!(
        result
            .sections_changed
            .contains(&"scheduler.daemon".to_owned())
    );
    assert!(result.output.contains("# [scheduler.daemon]"));
    assert!(result.output.contains("pid_file"));
    assert!(result.output.contains("tick_secs = 60"));
    assert!(result.output.contains("shutdown_grace_secs = 30"));
    assert!(result.output.contains("catch_up = true"));
}

#[test]
fn migrate_scheduler_daemon_idempotent_on_existing_block() {
    let src = "[agent]\nname = \"Zeph\"\n# [scheduler.daemon]\n# tick_secs = 60\n";
    let result = migrate_scheduler_daemon_config(src).expect("migrate");
    assert_eq!(result.changed_count, 0);
    assert_eq!(result.output, src);
}

// ── migrate_memory_retrieval_config ──────────────────────────────────────

#[test]
fn migrate_memory_retrieval_adds_block_when_absent() {
    let src = "[agent]\nname = \"Zeph\"\n";
    let result = migrate_memory_retrieval_config(src).expect("migrate");
    assert_eq!(result.changed_count, 1);
    assert!(
        result
            .sections_changed
            .contains(&"memory.retrieval".to_owned())
    );
    assert!(result.output.contains("# [memory.retrieval]"));
    assert!(result.output.contains("depth = 0"));
    assert!(result.output.contains("context_format"));
}

#[test]
fn migrate_memory_retrieval_idempotent_on_active_section() {
    let src = "[memory.retrieval]\ndepth = 40\n";
    let result = migrate_memory_retrieval_config(src).expect("migrate");
    assert_eq!(result.changed_count, 0);
    assert_eq!(result.output, src);
}

#[test]
fn migrate_memory_retrieval_idempotent_on_commented_section() {
    let src = "[agent]\nname = \"Zeph\"\n# [memory.retrieval]\n# depth = 0\n";
    let result = migrate_memory_retrieval_config(src).expect("migrate");
    assert_eq!(result.changed_count, 0);
    assert_eq!(result.output, src);
}

// ── acp PR4 migration ─────────────────────────────────────────────────────

#[test]
fn migrate_adds_pr4_acp_keys_commented() {
    let migrator = ConfigMigrator::new();
    let input = include_str!("../../tests/fixtures/acp_pr4_v0_19.toml");
    let out = migrator.migrate(input).expect("migrate");
    assert!(
        out.output.contains("# additional_directories = []"),
        "expected commented additional_directories; got:\n{}",
        out.output
    );
    assert!(
        out.output.contains("# auth_methods = [\"agent\"]"),
        "expected commented auth_methods; got:\n{}",
        out.output
    );
    assert!(
        out.output.contains("# message_ids_enabled = true"),
        "expected commented message_ids_enabled; got:\n{}",
        out.output
    );
}

// ── migrate_memory_reasoning_config ──────────────────────────────────────

#[test]
fn migrate_memory_reasoning_adds_block_when_absent() {
    let input = "[agent]\nmodel = \"gpt-4o\"\n";
    let result = migrate_memory_reasoning_config(input).unwrap();
    assert_eq!(result.changed_count, 1);
    assert!(
        result
            .sections_changed
            .contains(&"memory.reasoning".to_owned())
    );
    assert!(result.output.contains("# [memory.reasoning]"));
    assert!(result.output.contains("extraction_timeout_secs = 30"));
    assert!(result.output.contains("max_message_chars = 2000"));
}

#[test]
fn migrate_memory_reasoning_idempotent_on_existing_block() {
    let input = "[agent]\nmodel = \"gpt-4o\"\n# [memory.reasoning]\n# enabled = false\n";
    let result = migrate_memory_reasoning_config(input).unwrap();
    assert_eq!(result.changed_count, 0);
    assert!(result.sections_changed.is_empty());
    assert_eq!(result.output, input);
}

// ── migrate_hooks_turn_complete_config ────────────────────────────────────

#[test]
fn migrate_hooks_turn_complete_adds_block_when_absent() {
    let input = "[agent]\nmodel = \"gpt-4o\"\n";
    let result = migrate_hooks_turn_complete_config(input).unwrap();
    assert_eq!(result.changed_count, 1);
    assert!(
        result
            .sections_changed
            .contains(&"hooks.turn_complete".to_owned())
    );
    assert!(result.output.contains("# [[hooks.turn_complete]]"));
    assert!(result.output.contains("ZEPH_TURN_PREVIEW"));
    assert!(result.output.contains("timeout_secs = 3"));
}

#[test]
fn migrate_hooks_turn_complete_idempotent_on_existing_block() {
    let input =
        "[agent]\nmodel = \"gpt-4o\"\n# [[hooks.turn_complete]]\n# command = \"echo done\"\n";
    let result = migrate_hooks_turn_complete_config(input).unwrap();
    assert_eq!(result.changed_count, 0);
    assert!(result.sections_changed.is_empty());
    assert_eq!(result.output, input);
}

// ── migrate_focus_auto_consolidate_min_window ──────────────────────────────

/// S5: the comment must land inside [agent.focus], not after a subsequent section.
#[test]
fn migrate_focus_auto_consolidate_injects_inside_section() {
    let input = "[agent.focus]\nenabled = true\n\n[other]\nfoo = 1\n";
    let result = migrate_focus_auto_consolidate_min_window(input).unwrap();
    assert_eq!(result.changed_count, 1);
    let comment_pos = result
        .output
        .find("auto_consolidate_min_window")
        .expect("comment must be present");
    let other_pos = result
        .output
        .find("[other]")
        .expect("[other] must be present");
    assert!(
        comment_pos < other_pos,
        "auto_consolidate_min_window comment must appear before [other] section"
    );
}

#[test]
fn migrate_focus_auto_consolidate_idempotent() {
    let input = "[agent.focus]\nenabled = true\nauto_consolidate_min_window = 6\n";
    let result = migrate_focus_auto_consolidate_min_window(input).unwrap();
    assert_eq!(result.changed_count, 0);
    assert_eq!(result.output, input);
}

#[test]
fn migrate_focus_auto_consolidate_noop_when_section_absent() {
    let input = "[agent]\nname = \"zeph\"\n";
    let result = migrate_focus_auto_consolidate_min_window(input).unwrap();
    assert_eq!(result.changed_count, 0);
    assert_eq!(result.output, input);
}

#[test]
fn migrate_focus_auto_consolidate_noop_when_only_commented_section() {
    let input = "[agent]\n# [agent.focus]\n# enabled = false\n";
    let result = migrate_focus_auto_consolidate_min_window(input).unwrap();
    assert_eq!(result.changed_count, 0);
    assert_eq!(result.output, input);
}

// ── Migration registry ────────────────────────────────────────────────────

#[test]
fn registry_has_fifty_entries() {
    assert_eq!(MIGRATIONS.len(), 76);
}

#[test]
fn registry_names_are_unique_and_non_empty() {
    let names: Vec<&str> = MIGRATIONS.iter().map(|m| m.name()).collect();
    for name in &names {
        assert!(!name.is_empty(), "migration name must not be empty");
    }
    let mut deduped = names.clone();
    deduped.sort_unstable();
    deduped.dedup();
    assert_eq!(deduped.len(), names.len(), "migration names must be unique");
}

#[test]
fn registry_is_idempotent_on_empty_input() {
    // Migrations that append comment blocks cannot be idempotent by design:
    // comment text is not parsed as TOML keys, so presence checks always fail.
    const COMMENT_ONLY: &[&str] = &["migrate_magic_docs_config"];

    let mut toml = String::new();
    for m in MIGRATIONS.iter() {
        let result = m.apply(&toml).expect("registry migration must not fail");
        toml = result.output;
    }
    for m in MIGRATIONS.iter() {
        if COMMENT_ONLY.contains(&m.name()) {
            continue;
        }
        let result = m
            .apply(&toml)
            .expect("registry migration must not fail on second pass");
        assert_eq!(result.changed_count, 0, "{} is not idempotent", m.name());
    }
}

#[test]
fn registry_preserves_order_matches_dispatch() {
    // Names must follow the documented step order (steps 1–76).
    let expected = [
        "migrate_stt_to_provider",
        "migrate_planner_model_to_provider",
        "migrate_mcp_trust_levels",
        "migrate_agent_retry_to_tools_retry",
        "migrate_database_url",
        "migrate_shell_transactional",
        "migrate_agent_budget_hint",
        "migrate_forgetting_config",
        "migrate_compression_predictor_config",
        "migrate_microcompact_config",
        "migrate_autodream_config",
        "migrate_magic_docs_config",
        "migrate_telemetry_config",
        "migrate_supervisor_config",
        "migrate_otel_filter",
        "migrate_egress_config",
        "migrate_vigil_config",
        "migrate_sandbox_config",
        "migrate_sandbox_egress_filter",
        "migrate_orchestration_persistence",
        "migrate_session_recap_config",
        "migrate_mcp_elicitation_config",
        "migrate_quality_config",
        "migrate_acp_subagents_config",
        "migrate_hooks_permission_denied_config",
        "migrate_memory_graph_config",
        "migrate_scheduler_daemon_config",
        "migrate_memory_retrieval_config",
        "migrate_memory_reasoning_config",
        "migrate_memory_reasoning_judge_config",
        "migrate_memory_hebbian_config",
        "migrate_memory_hebbian_consolidation_config",
        "migrate_memory_hebbian_spread_config",
        "migrate_hooks_turn_complete_config",
        "migrate_focus_auto_consolidate_min_window",
        "migrate_session_provider_persistence",
        "migrate_memory_retrieval_query_bias",
        "migrate_memory_persona_config",
        "migrate_qdrant_api_key",
        "migrate_mcp_max_connect_attempts",
        "migrate_goals_config",
        "migrate_tools_compression_config",
        "migrate_orchestrator_provider",
        "migrate_provider_max_concurrent",
        "migrate_gonkagate_to_gonka",
        "migrate_cocoon_provider_notice",
        "migrate_trace_metadata",
        "migrate_five_signal_config",
        "migrate_embed_provider_rename",
        "migrate_mcp_retry_and_tool_timeout",
        "migrate_fidelity_timeout_defaults",
        "migrate_session_persist_provider_overrides",
        "migrate_cocoon_show_balance",
        "migrate_worktree_config",
        "migrate_worktree_git_timeout",
        "migrate_llm_stream_limits",
        "migrate_durable_config",
        "migrate_eval_model_to_provider",
        "migrate_caveman_config",
        "migrate_shell_checkpoints_config",
        "migrate_knowledge_config",
        "migrate_deep_link_config",
        "migrate_memory_graph_recall_include_imported",
        "migrate_policy_provider_and_utility_window",
        "migrate_tui_theme_config",
        "migrate_tui_theme_defaults",
        "migrate_tui_delights",
        "migrate_tui_mouse",
        "migrate_orchestration_asset_sensitivity",
        "migrate_session_persistence_config",
        "migrate_serve_config",
        "migrate_nli_config",
        "migrate_secret_masking_config",
        "migrate_pii_filter_names",
        "migrate_qdrant_timeout_secs",
        "migrate_utility_high_gain_tools",
    ];
    let actual: Vec<&str> = MIGRATIONS.iter().map(|m| m.name()).collect();
    assert_eq!(actual, expected);
}

// ── migrate_trace_metadata tests (#4160) ─────────────────────────────────

#[test]
fn migrate_trace_metadata_noop_when_already_present() {
    let src = "[telemetry]\nenabled = true\n\n[telemetry.trace_metadata]\n\"env\" = \"prod\"\n";
    let result = migrate_trace_metadata(src).unwrap();
    assert_eq!(result.changed_count, 0);
    assert_eq!(result.output, src);
}

#[test]
fn migrate_trace_metadata_noop_when_no_telemetry_section() {
    let src = "[agent]\nmax_turns = 10\n";
    let result = migrate_trace_metadata(src).unwrap();
    assert_eq!(result.changed_count, 0);
    assert_eq!(result.output, src);
}

#[test]
fn migrate_trace_metadata_injects_comment_when_telemetry_present() {
    let src = "[telemetry]\nenabled = true\nservice_name = \"zeph\"\n";
    let result = migrate_trace_metadata(src).unwrap();
    assert_eq!(result.changed_count, 1);
    assert!(result.output.contains("trace_metadata"));
    assert!(
        result
            .sections_changed
            .contains(&"telemetry.trace_metadata".to_owned())
    );
    // Idempotent: running again is a no-op.
    let result2 = migrate_trace_metadata(&result.output).unwrap();
    assert_eq!(result2.changed_count, 0);
}

// ── migrate_qdrant_api_key tests (#3543) ─────────────────────────────────

#[test]
fn migrate_qdrant_api_key_adds_comment_when_absent() {
    let src = "[memory]\nqdrant_url = \"http://localhost:6334\"\n";
    let result = migrate_qdrant_api_key(src).expect("migrate");
    assert_eq!(result.changed_count, 1);
    assert!(
        result
            .sections_changed
            .contains(&"memory.qdrant_api_key".to_owned())
    );
    assert!(result.output.contains("# qdrant_api_key = \"\""));
}

#[test]
fn migrate_qdrant_api_key_is_noop_when_present() {
    let src =
        "[memory]\nqdrant_url = \"https://xyz.cloud.qdrant.io\"\nqdrant_api_key = \"secret\"\n";
    let result = migrate_qdrant_api_key(src).expect("migrate");
    assert_eq!(result.changed_count, 0);
    assert!(result.sections_changed.is_empty());
    assert_eq!(result.output, src);
}

#[test]
fn migrate_qdrant_api_key_creates_memory_section_when_absent() {
    let src = "[agent]\nname = \"Zeph\"\n";
    let result = migrate_qdrant_api_key(src).expect("migrate");
    assert_eq!(result.changed_count, 1);
    assert!(result.output.contains("# qdrant_api_key = \"\""));
}

#[test]
fn migrate_qdrant_api_key_idempotent_on_commented_output() {
    let base = "[memory]\nqdrant_url = \"http://localhost:6334\"\n";
    let first = migrate_qdrant_api_key(base).unwrap();
    assert_eq!(first.changed_count, 1);
    let second = migrate_qdrant_api_key(&first.output).unwrap();
    assert_eq!(second.changed_count, 0, "second run must not double-append");
    assert_eq!(second.output, first.output);
}

// ── migrate_qdrant_timeout_secs tests ─────────────────────────────────────

#[test]
fn migrate_qdrant_timeout_secs_adds_comment_when_absent() {
    let src = "[memory]\nqdrant_url = \"http://localhost:6334\"\n";
    let result = migrate_qdrant_timeout_secs(src).expect("migrate");
    assert_eq!(result.changed_count, 1);
    assert!(
        result
            .sections_changed
            .contains(&"memory.qdrant_timeout_secs".to_owned())
    );
    assert!(result.output.contains("# qdrant_timeout_secs = 10"));
}

#[test]
fn migrate_qdrant_timeout_secs_is_noop_when_present() {
    let src = "[memory]\nqdrant_url = \"http://localhost:6334\"\nqdrant_timeout_secs = 5\n";
    let result = migrate_qdrant_timeout_secs(src).expect("migrate");
    assert_eq!(result.changed_count, 0);
    assert!(result.sections_changed.is_empty());
    assert_eq!(result.output, src);
}

#[test]
fn migrate_qdrant_timeout_secs_creates_memory_section_when_absent() {
    let src = "[agent]\nname = \"Zeph\"\n";
    let result = migrate_qdrant_timeout_secs(src).expect("migrate");
    assert_eq!(result.changed_count, 1);
    assert!(result.output.contains("# qdrant_timeout_secs = 10"));
}

#[test]
fn migrate_qdrant_timeout_secs_idempotent_on_commented_output() {
    let base = "[memory]\nqdrant_url = \"http://localhost:6334\"\n";
    let first = migrate_qdrant_timeout_secs(base).unwrap();
    assert_eq!(first.changed_count, 1);
    let second = migrate_qdrant_timeout_secs(&first.output).unwrap();
    assert_eq!(second.changed_count, 0, "second run must not double-append");
    assert_eq!(second.output, first.output);
}

// ── migrate_utility_high_gain_tools tests (#5659) ─────────────────────────

#[test]
fn migrate_utility_high_gain_tools_adds_comment_when_absent() {
    let src = "[tools.utility]\nenabled = false\nthreshold = 0.1\n";
    let result = migrate_utility_high_gain_tools(src).expect("migrate");
    assert_eq!(result.changed_count, 1);
    assert!(
        result
            .sections_changed
            .contains(&"tools.utility.high_gain_tools".to_owned())
    );
    assert!(result.output.contains("# high_gain_tools = []"));
}

#[test]
fn migrate_utility_high_gain_tools_is_noop_when_present() {
    let src = "[tools.utility]\nhigh_gain_tools = [\"github_create_issue\"]\n";
    let result = migrate_utility_high_gain_tools(src).expect("migrate");
    assert_eq!(result.changed_count, 0);
    assert!(result.sections_changed.is_empty());
    assert_eq!(result.output, src);
}

#[test]
fn migrate_utility_high_gain_tools_appends_when_tools_utility_section_absent() {
    // Unlike migrate_qdrant_timeout_secs (step 75), this migration never parses the TOML
    // with toml_edit — it only string-matches "high_gain_tools" and appends a fully
    // commented-out block. Confirm this holds even when [tools.utility] is missing
    // entirely, and that the output remains valid TOML.
    let src = "[agent]\nname = \"Zeph\"\n";
    let result = migrate_utility_high_gain_tools(src).expect("migrate");
    assert_eq!(result.changed_count, 1);
    assert!(result.output.contains("# high_gain_tools = []"));
    result
        .output
        .parse::<toml_edit::DocumentMut>()
        .expect("migrated output must remain valid TOML");
}

#[test]
fn migrate_utility_high_gain_tools_idempotent_on_commented_output() {
    let base = "[tools.utility]\nenabled = false\n";
    let first = migrate_utility_high_gain_tools(base).unwrap();
    assert_eq!(first.changed_count, 1);
    let second = migrate_utility_high_gain_tools(&first.output).unwrap();
    assert_eq!(second.changed_count, 0, "second run must not double-append");
    assert_eq!(second.output, first.output);
}

#[test]
fn migrate_mcp_max_connect_attempts_adds_comment_when_absent() {
    let src = "[mcp]\nallowed_commands = []\n";
    let result = migrate_mcp_max_connect_attempts(src).expect("migrate");
    assert_eq!(result.changed_count, 1);
    assert!(
        result.output.contains("max_connect_attempts"),
        "output must mention max_connect_attempts"
    );
}

#[test]
fn migrate_mcp_max_connect_attempts_idempotent_when_present() {
    let src = "[mcp]\n# max_connect_attempts = 3\nallowed_commands = []\n";
    let result = migrate_mcp_max_connect_attempts(src).expect("migrate");
    assert_eq!(
        result.changed_count, 0,
        "must not modify already-present key"
    );
    assert_eq!(result.output, src);
}

#[test]
fn migrate_mcp_max_connect_attempts_skips_when_no_mcp_section() {
    let src = "[agent]\nname = \"Zeph\"\n";
    let result = migrate_mcp_max_connect_attempts(src).expect("migrate");
    assert_eq!(result.changed_count, 0);
    assert_eq!(result.output, src);
}

// ── Step 50 — mcp startup_retry_backoff_ms and tool_timeout_secs ──────────────────────────────

#[test]
fn migrate_mcp_retry_and_tool_timeout_adds_both_keys_when_absent() {
    let src = "[mcp]\nallowed_commands = []\n";
    let result = migrate_mcp_retry_and_tool_timeout(src).expect("migrate");
    assert_eq!(result.changed_count, 1);
    assert!(
        result.output.contains("startup_retry_backoff_ms"),
        "output must include startup_retry_backoff_ms"
    );
    assert!(
        result.output.contains("tool_timeout_secs"),
        "output must include tool_timeout_secs"
    );
}

#[test]
fn migrate_mcp_retry_and_tool_timeout_idempotent_when_both_present() {
    let src = "[mcp]\n# startup_retry_backoff_ms = 1000\n# tool_timeout_secs = 60\n";
    let result = migrate_mcp_retry_and_tool_timeout(src).expect("migrate");
    assert_eq!(result.changed_count, 0);
    assert_eq!(result.output, src);
}

#[test]
fn migrate_mcp_retry_and_tool_timeout_skips_when_no_mcp_section() {
    let src = "[agent]\nname = \"Zeph\"\n";
    let result = migrate_mcp_retry_and_tool_timeout(src).expect("migrate");
    assert_eq!(result.changed_count, 0);
    assert_eq!(result.output, src);
}

// ── Step 43 — orchestrator_provider ──────────────────────────────────────────────────────────

#[test]
fn step43_adds_orchestrator_provider_comment_when_absent() {
    let src = "[orchestration]\nenabled = true\n";
    let result = migrate_orchestration_orchestrator_provider(src).expect("migrate");
    assert_eq!(result.changed_count, 1);
    assert!(
        result.output.contains("orchestrator_provider"),
        "migration must inject orchestrator_provider hint"
    );
}

#[test]
fn step43_noop_when_orchestrator_provider_already_present() {
    let src = "[orchestration]\nenabled = true\norchestrator_provider = \"\"\n";
    let result = migrate_orchestration_orchestrator_provider(src).expect("migrate");
    assert_eq!(
        result.changed_count, 0,
        "must not modify already-present key"
    );
    assert_eq!(result.output, src);
}

// ── Step 44 — max_concurrent per-provider ────────────────────────────────────────────────────

#[test]
fn step44_adds_max_concurrent_comment_when_providers_present() {
    let src = "[[llm.providers]]\nname = \"quality\"\ntype = \"openai\"\n";
    let result = migrate_provider_max_concurrent(src).expect("migrate");
    assert_eq!(result.changed_count, 1);
    assert!(
        result.output.contains("max_concurrent"),
        "migration must inject max_concurrent hint"
    );
}

#[test]
fn step44_noop_when_max_concurrent_already_present() {
    let src = "[[llm.providers]]\nname = \"quality\"\nmax_concurrent = 4\n";
    let result = migrate_provider_max_concurrent(src).expect("migrate");
    assert_eq!(
        result.changed_count, 0,
        "must not modify already-present key"
    );
    assert_eq!(result.output, src);
}

#[test]
fn step44_noop_when_no_providers_section() {
    let src = "[agent]\nname = \"Zeph\"\n";
    let result = migrate_provider_max_concurrent(src).expect("migrate");
    assert_eq!(result.changed_count, 0);
    assert_eq!(result.output, src);
}

// ── Step 45 — migrate_gonkagate_to_gonka ─────────────────────────────────

#[test]
fn step45_adds_advisory_comment_when_gonkagate_present() {
    let src = "[[llm.providers]]\ntype = \"compatible\"\nname = \"gonkagate\"\n";
    let result = migrate_gonkagate_to_gonka(src);
    assert!(result.changed_count > 0, "must detect gonkagate entry");
    assert!(
        result.output.contains("[migration] GonkaGate detected"),
        "advisory comment must be added"
    );
    // Comment must appear before the [[llm.providers]] table header, not inside it.
    let comment_pos = result
        .output
        .find("[migration] GonkaGate detected")
        .unwrap();
    let header_pos = result.output.find("[[llm.providers]]").unwrap();
    assert!(
        comment_pos < header_pos,
        "advisory comment must precede the [[llm.providers]] header"
    );
}

#[test]
fn step45_noop_when_no_gonkagate() {
    let src = "[[llm.providers]]\ntype = \"openai\"\nname = \"quality\"\n";
    let result = migrate_gonkagate_to_gonka(src);
    assert_eq!(result.changed_count, 0);
    assert_eq!(result.output, src);
}

#[test]
fn step45_does_not_double_insert_comment() {
    let src = "[[llm.providers]]\ntype = \"compatible\"\nname = \"gonkagate\"\n";
    let first = migrate_gonkagate_to_gonka(src);
    let second = migrate_gonkagate_to_gonka(&first.output);
    // Second run must not add another comment line.
    assert_eq!(second.changed_count, 0, "idempotent on second run");
}

// ── Step 46 — Cocoon provider notice ──────────────────────────────────────

#[test]
fn migrate_cocoon_noop_empty_config() {
    let src = "";
    let result = migrate_cocoon_provider_notice(src).unwrap();
    assert_eq!(result.changed_count, 0);
    assert_eq!(result.output, src);
}

#[test]
fn migrate_cocoon_noop_existing_config() {
    let src = "[agent]\nname = \"zeph\"\n\n[[llm.providers]]\ntype = \"ollama\"\n";
    let result = migrate_cocoon_provider_notice(src).unwrap();
    assert_eq!(result.changed_count, 0);
    assert_eq!(result.output, src);
}

#[test]
fn migrate_cocoon_idempotent() {
    let src = "[[llm.providers]]\ntype = \"cocoon\"\nname = \"tee\"\n";
    let first = migrate_cocoon_provider_notice(src).unwrap();
    let second = migrate_cocoon_provider_notice(&first.output).unwrap();
    assert_eq!(second.output, first.output);
    assert_eq!(second.changed_count, 0);
}

// ── migrate_five_signal_config tests (#4374) ─────────────────────────────

#[test]
fn migrate_five_signal_config_noop_when_already_present() {
    let src = "[memory]\nenabled = true\n\n[memory.five_signal]\nenabled = false\n";
    let result = migrate_five_signal_config(src).unwrap();
    assert_eq!(result.changed_count, 0);
    assert_eq!(result.output, src);
}

#[test]
fn migrate_five_signal_config_noop_when_no_memory_section() {
    let src = "[agent]\nmax_turns = 10\n";
    let result = migrate_five_signal_config(src).unwrap();
    assert_eq!(result.changed_count, 0);
    assert_eq!(result.output, src);
}

#[test]
fn migrate_five_signal_config_injects_comment_when_memory_present() {
    let src = "[memory]\nenabled = true\n";
    let result = migrate_five_signal_config(src).unwrap();
    assert_eq!(result.changed_count, 1);
    assert!(result.output.contains("five_signal"));
    assert!(
        result
            .sections_changed
            .contains(&"memory.five_signal".to_owned())
    );
}

#[test]
fn migrate_five_signal_config_idempotent_on_commented_output() {
    let base = "[memory]\nenabled = true\n";
    let first = migrate_five_signal_config(base).unwrap();
    let second = migrate_five_signal_config(&first.output).unwrap();
    assert_eq!(second.output, first.output);
    assert_eq!(second.changed_count, 0);
}

// ── migrate_embed_provider_rename tests (#4480) ───────────────────────────

#[test]
fn migrate_embed_provider_rename_renames_all_four_keys() {
    let src = "\
[memory.semantic]\n\
embed_provider = \"ollama-embed\"\n\
\n\
[index]\n\
embed_provider = \"ollama-embed\"\n\
\n\
[llm.coe]\n\
embed_provider = \"\"\n\
\n\
[learning]\n\
trace_extraction_embed_provider = \"embed-fast\"\n";
    let result = migrate_embed_provider_rename(src).unwrap();
    assert_eq!(result.changed_count, 4);
    assert!(
        result
            .output
            .contains("embedding_provider = \"ollama-embed\"")
    );
    assert!(
        result
            .output
            .contains("trace_extraction_embedding_provider = \"embed-fast\"")
    );
    assert!(!result.output.contains("trace_extraction_embed_provider ="));
    assert!(!result.output.contains("\nembed_provider ="));
}

#[test]
fn migrate_embed_provider_rename_idempotent_on_own_output() {
    let src = "\
[memory.semantic]\n\
embed_provider = \"ollama-embed\"\n\
\n\
[learning]\n\
trace_extraction_embed_provider = \"embed-fast\"\n";
    let first = migrate_embed_provider_rename(src).unwrap();
    assert_eq!(first.changed_count, 2);
    let second = migrate_embed_provider_rename(&first.output).unwrap();
    assert_eq!(second.changed_count, 0, "second run must be a no-op");
    assert_eq!(second.output, first.output);
}

#[test]
fn migrate_embed_provider_rename_noop_when_no_old_keys() {
    let src = "\
[memory.semantic]\n\
embedding_provider = \"ollama-embed\"\n\
\n\
[learning]\n\
trace_extraction_embedding_provider = \"embed-fast\"\n";
    let result = migrate_embed_provider_rename(src).unwrap();
    assert_eq!(result.changed_count, 0);
    assert_eq!(result.output, src);
}

#[test]
fn migrate_embed_provider_rename_preserves_commented_lines() {
    // Lines starting with `#` must not be renamed — `trimmed.starts_with("embed_provider")`
    // is false when the line starts with `#`.
    let src = "# embed_provider = \"old-key\"  # this is a comment\n\
trace_extraction_embed_provider = \"live\"\n";
    let result = migrate_embed_provider_rename(src).unwrap();
    // Only the uncommented key is renamed.
    assert_eq!(result.changed_count, 1);
    assert!(result.output.contains("# embed_provider = \"old-key\""));
    assert!(
        result
            .output
            .contains("trace_extraction_embedding_provider = \"live\"")
    );
}

// ── migrate_fidelity_timeout_defaults tests (#4645, #4651) ───────────────

#[test]
fn migrate_fidelity_timeout_defaults_adds_both_comments_when_absent() {
    let src = "[memory.fidelity]\nenabled = true\n";
    let result = migrate_fidelity_timeout_defaults(src).expect("migrate");
    assert_eq!(result.changed_count, 1);
    assert!(result.output.contains("embed_timeout_secs"));
    assert!(result.output.contains("compress_timeout_secs"));
    assert!(
        result
            .sections_changed
            .contains(&"memory.fidelity".to_owned())
    );
}

#[test]
fn migrate_fidelity_timeout_defaults_idempotent_when_both_present() {
    let src = "[memory.fidelity]\nembed_timeout_secs = 30\ncompress_timeout_secs = 30\n";
    let result = migrate_fidelity_timeout_defaults(src).expect("migrate");
    assert_eq!(result.changed_count, 0);
}

#[test]
fn migrate_fidelity_timeout_defaults_skips_when_no_fidelity_section() {
    let src = "[agent]\nname = \"test\"\n";
    let result = migrate_fidelity_timeout_defaults(src).expect("migrate");
    assert_eq!(result.changed_count, 0);
    assert_eq!(result.output, src);
}

#[test]
fn migrate_fidelity_timeout_defaults_adds_only_missing_key() {
    let src = "[memory.fidelity]\nenabled = true\nembed_timeout_secs = 60\n";
    let result = migrate_fidelity_timeout_defaults(src).expect("migrate");
    assert_eq!(result.changed_count, 1);
    assert!(result.output.contains("compress_timeout_secs"));
    // embed_timeout_secs already present as a real key, must not be duplicated
    assert_eq!(
        result.output.matches("embed_timeout_secs").count(),
        1,
        "embed_timeout_secs must appear exactly once"
    );
}

// ── Step 53 — cocoon.show_balance advisory notice (#4649) ────────────────────────────────────

#[test]
fn migrate_cocoon_show_balance_adds_section_when_absent() {
    let src = "[agent]\nname = \"Zeph\"\n";
    let result = migrate_cocoon_show_balance(src).expect("migrate");
    assert_eq!(result.changed_count, 1);
    assert!(
        result.output.contains("show_balance"),
        "output must mention show_balance"
    );
    assert!(
        result.output.contains("[cocoon]"),
        "output must contain [cocoon] section"
    );
}

#[test]
fn migrate_cocoon_show_balance_idempotent_when_key_present() {
    let src = "[cocoon]\n# show_balance = true\n";
    let result = migrate_cocoon_show_balance(src).expect("migrate");
    assert_eq!(
        result.changed_count, 0,
        "must not modify config that already has show_balance"
    );
    assert_eq!(result.output, src);
}

#[test]
fn migrate_cocoon_show_balance_idempotent_when_active_key_present() {
    let src = "[cocoon]\nshow_balance = false\n";
    let result = migrate_cocoon_show_balance(src).expect("migrate");
    assert_eq!(result.changed_count, 0);
    assert_eq!(result.output, src);
}

// ── migrate_worktree_config tests (#4679) ────────────────────────────────

#[test]
fn step_54_inserts_worktree_section_on_fresh_config() {
    let input = "[agent]\nmax_turns = 10\n";
    let result = migrate_worktree_config(input).unwrap();
    assert_eq!(result.changed_count, 1);
    assert!(
        result.output.contains("[worktree]"),
        "should insert [worktree] section"
    );
    assert!(
        result.output.contains("# enabled = false"),
        "should include default fields"
    );
}

#[test]
fn step_54_is_idempotent_when_worktree_present() {
    let input = "[worktree]\nenabled = true\n";
    let result = migrate_worktree_config(input).unwrap();
    assert_eq!(result.changed_count, 0);
    assert_eq!(
        result.output.matches("[worktree]").count(),
        1,
        "should not duplicate [worktree]"
    );
}

#[test]
fn step_54_is_idempotent_when_worktree_commented() {
    // A `# [worktree]` line (commented-out header) counts as present — conservative.
    let input = "# [worktree]\n[agent]\nmax_turns = 10\n";
    let result = migrate_worktree_config(input).unwrap();
    assert_eq!(
        result.changed_count, 0,
        "commented [worktree] counts as present"
    );
}

#[test]
fn step_54_does_not_skip_when_worktree_in_value() {
    // Regression: `[worktree]` inside a string value must NOT suppress the migration.
    let input = "[agent]\ndescription = \"uses [worktree] isolation\"\n";
    let result = migrate_worktree_config(input).unwrap();
    assert_eq!(
        result.changed_count, 1,
        "[worktree] in a value must not suppress migration"
    );
    assert!(
        result.output.contains("# [worktree]"),
        "output should contain the inserted worktree comment block"
    );
}

// ── migrate_durable_config tests (spec-064, #4949) ───────────────────────

#[test]
fn step_57_inserts_durable_section_on_fresh_config() {
    let input = "[agent]\nmax_turns = 10\n";
    let result = migrate_durable_config(input).unwrap();
    assert_eq!(result.changed_count, 1);
    assert!(
        result.output.contains("# [durable]"),
        "should insert commented [durable] section"
    );
    assert!(
        result.output.contains("# [durable.retention]"),
        "should include the retention sub-table"
    );
    assert!(
        result.output.contains("# enabled = false"),
        "durable migration is default-off"
    );
}

#[test]
fn step_57_is_idempotent_when_durable_present() {
    let input = "[durable]\nenabled = true\n";
    let result = migrate_durable_config(input).unwrap();
    assert_eq!(result.changed_count, 0);
    assert_eq!(
        result.output.matches("[durable]").count(),
        1,
        "should not duplicate [durable]"
    );
}

#[test]
fn step_57_is_idempotent_across_repeated_runs() {
    let input = "[agent]\nmax_turns = 10\n";
    let once = migrate_durable_config(input).unwrap();
    let twice = migrate_durable_config(&once.output).unwrap();
    assert_eq!(
        twice.changed_count, 0,
        "second run must not re-insert the commented block"
    );
    assert_eq!(
        twice.output.matches("# [durable]").count(),
        1,
        "running twice must not duplicate the durable block"
    );
}

#[test]
fn step_57_does_not_skip_when_durable_in_value() {
    let input = "[agent]\ndescription = \"the [durable] layer\"\n";
    let result = migrate_durable_config(input).unwrap();
    assert_eq!(
        result.changed_count, 1,
        "[durable] in a value must not suppress migration"
    );
}

// ── migrate_worktree_git_timeout tests (#4704) ───────────────────────────

#[test]
fn step_55_inserts_git_timeout_comment_when_worktree_present() {
    let input = "[worktree]\nenabled = true\n";
    let result = migrate_worktree_git_timeout(input).unwrap();
    assert_eq!(result.changed_count, 1);
    assert!(
        result.output.contains("git_timeout_secs"),
        "should insert git_timeout_secs comment"
    );
    assert_eq!(result.sections_changed, vec!["worktree"]);
}

#[test]
fn step_55_is_noop_when_no_worktree_section() {
    let input = "[agent]\nmax_turns = 10\n";
    let result = migrate_worktree_git_timeout(input).unwrap();
    assert_eq!(result.changed_count, 0);
    assert_eq!(result.output, input);
}

#[test]
fn step_55_is_idempotent_when_git_timeout_already_present() {
    let input = "[worktree]\ngit_timeout_secs = 60\n";
    let result = migrate_worktree_git_timeout(input).unwrap();
    assert_eq!(result.changed_count, 0);
    assert_eq!(result.output, input);
}

#[test]
fn step_55_is_idempotent_when_git_timeout_commented() {
    let input = "[worktree]\n# git_timeout_secs = 30\n";
    let result = migrate_worktree_git_timeout(input).unwrap();
    assert_eq!(result.changed_count, 0);
}

#[test]
fn step_55_handles_crlf_line_endings() {
    let input = "[worktree]\r\nenabled = true\r\n";
    let result = migrate_worktree_git_timeout(input).unwrap();
    assert_eq!(result.changed_count, 1);
    assert!(
        result.output.contains("git_timeout_secs"),
        "CRLF input should still receive the git_timeout_secs comment"
    );
}

// ── section_header_present tests (#4804) ────────────────────────────────

#[test]
fn section_header_present_exact_match() {
    assert!(section_header_present(
        "[worktree]\nenabled = true\n",
        "worktree"
    ));
}

#[test]
fn section_header_present_inline_comment() {
    assert!(section_header_present(
        "[worktree] # some comment\nenabled = true\n",
        "worktree"
    ));
}

#[test]
fn section_header_present_subtable_implies_parent() {
    assert!(section_header_present(
        "[worktree.git]\ntimeout = 30\n",
        "worktree"
    ));
}

#[test]
fn section_header_present_commented_header_returns_false() {
    assert!(!section_header_present(
        "# [worktree]\nenabled = true\n",
        "worktree"
    ));
}

#[test]
fn section_header_present_no_match() {
    assert!(!section_header_present(
        "[agent]\nmax_turns = 10\n",
        "worktree"
    ));
}

#[test]
fn section_header_present_does_not_match_value_containing_header() {
    // A TOML value like `path = "[worktree]"` must not trigger the guard.
    assert!(!section_header_present(
        "[agent]\npath = \"[worktree]\"\n",
        "worktree"
    ));
}

// ── step_55 regression tests for C1/S1/S2 ────────────────────────────────

#[test]
fn step_55_subtable_only_is_noop() {
    // C1 regression: `[worktree.git]` makes section_header_present return true,
    // but the regex cannot inject a comment after the subtable header —
    // the function must report changed_count=0.
    let input = "[worktree.git]\ntimeout = 30\n";
    let result = migrate_worktree_git_timeout(input).unwrap();
    assert_eq!(
        result.changed_count, 0,
        "subtable-only header must be a no-op"
    );
    assert_eq!(result.output, input);
}

#[test]
fn step_55_inline_comment_header_gets_comment_injected() {
    // S1: `[worktree] # remark` is an active header — git_timeout_secs must be injected.
    let input = "[worktree] # remark\nenabled = true\n";
    let result = migrate_worktree_git_timeout(input).unwrap();
    assert_eq!(result.changed_count, 1);
    assert!(
        result.output.contains("git_timeout_secs"),
        "inline-comment header must still receive the git_timeout_secs comment"
    );
    assert!(
        result.output.contains("[worktree] # remark"),
        "original header line must be preserved"
    );
}

#[test]
fn step_55_value_substring_is_noop() {
    // S2: a value containing `[worktree]` must not be mistaken for the section header.
    let input = "[agent]\npath = \"[worktree]\"\n";
    let result = migrate_worktree_git_timeout(input).unwrap();
    assert_eq!(
        result.changed_count, 0,
        "value substring must not trigger replacement"
    );
    assert_eq!(result.output, input);
}

// ── Step 59 — migrate_caveman_config (#5027) ─────────────────────────────────────────────────

#[test]
fn migrate_caveman_config_appends_block_when_absent() {
    let src = "[agent]\nname = \"Zeph\"\n";
    let result = migrate_caveman_config(src).expect("migrate");
    assert_eq!(result.changed_count, 1);
    assert!(
        result.sections_changed.contains(&"caveman".to_owned()),
        "sections_changed must include 'caveman'"
    );
    assert!(
        result.output.contains("# [caveman]"),
        "output must contain commented-out [caveman] block"
    );
    assert!(
        result.output.contains("default_on"),
        "output must include the default_on key hint"
    );
}

#[test]
fn migrate_caveman_config_noop_when_caveman_section_present() {
    let src = "[agent]\nname = \"Zeph\"\n\n[caveman]\ndefault_on = false\n";
    let result = migrate_caveman_config(src).expect("migrate");
    assert_eq!(
        result.changed_count, 0,
        "must not modify config already containing [caveman]"
    );
    assert_eq!(result.output, src);
}

#[test]
fn migrate_caveman_config_noop_when_commented_block_present() {
    let src = "[agent]\nname = \"Zeph\"\n\n# [caveman]\n# default_on = false\n";
    let result = migrate_caveman_config(src).expect("migrate");
    assert_eq!(
        result.changed_count, 0,
        "must be idempotent when commented block already present"
    );
    assert_eq!(result.output, src);
}

// ── migrate_knowledge_config tests (step 61, spec-067, #5017) ─────────────────────────────

#[test]
fn step_61_adds_knowledge_block_when_absent() {
    let src = "[agent]\nname = \"Zeph\"\n";
    let result = migrate_knowledge_config(src).unwrap();
    assert_eq!(result.changed_count, 1);
    assert!(result.output.contains("# [knowledge]"));
}

#[test]
fn step_61_noop_when_knowledge_section_present() {
    let src = "[agent]\nname = \"Zeph\"\n[knowledge]\nmax_documents = 10\n";
    let result = migrate_knowledge_config(src).unwrap();
    assert_eq!(result.changed_count, 0);
    assert_eq!(result.output, src);
}

#[test]
fn step_61_noop_when_commented_knowledge_section_present() {
    let src = "[agent]\nname = \"Zeph\"\n# [knowledge]\n# max_documents = 0\n";
    let result = migrate_knowledge_config(src).unwrap();
    assert_eq!(result.changed_count, 0);
}

#[test]
fn step_61_idempotent_on_own_output() {
    let src = "[agent]\nname = \"Zeph\"\n";
    let first = migrate_knowledge_config(src).unwrap();
    let second = migrate_knowledge_config(&first.output).unwrap();
    assert_eq!(
        second.changed_count, 0,
        "step_61 must be idempotent on its own output"
    );
}

// ── Step 62 — migrate_deep_link_config (#5011) ────────────────────────────────

#[test]
fn migrate_deep_link_adds_advisory_comment_when_absent() {
    let src = "[agent]\nname = \"Zeph\"\n";
    let result = migrate_deep_link_config(src).expect("migrate");
    assert_eq!(result.changed_count, 1);
    assert!(
        result.sections_changed.contains(&"deep_link".to_owned()),
        "sections_changed should include 'deep_link'"
    );
    assert!(
        result.output.contains("# [deep_link]"),
        "output should contain commented [deep_link] advisory block"
    );
}

#[test]
fn migrate_deep_link_is_idempotent() {
    let src = "[agent]\nname = \"Zeph\"\n";
    let first = migrate_deep_link_config(src).expect("first migration");
    assert_eq!(first.changed_count, 1);

    let second = migrate_deep_link_config(&first.output).expect("second migration");
    assert_eq!(second.changed_count, 0, "second run must be a no-op");
    assert_eq!(
        second.output, first.output,
        "output must be unchanged on second run"
    );
}

// ── Step 64 — migrate_policy_provider_and_utility_window (#5067) ─────────────

#[test]
fn migrate_policy_provider_and_utility_window_adds_both_when_absent() {
    let src = "[agent]\nname = \"Zeph\"\n";
    let result = migrate_policy_provider_and_utility_window(src).expect("migrate");
    assert_eq!(result.changed_count, 2);
    assert!(
        result.output.contains("# policy_provider"),
        "must add policy_provider comment"
    );
    assert!(
        result.output.contains("# utility_window"),
        "must add utility_window comment"
    );
    assert!(
        result.output.contains("# [tools.policy]"),
        "must reference [tools.policy] section"
    );
    assert!(
        result.output.contains("# [tools.utility]"),
        "must reference [tools.utility] section (not tools.utility_scoring)"
    );
    assert!(
        !result.output.contains("utility_scoring"),
        "must not emit [tools.utility_scoring] — wrong section name"
    );
}

#[test]
fn migrate_policy_provider_and_utility_window_idempotent_when_both_present() {
    let src = "[tools.policy]\npolicy_provider = \"\"\n[tools.utility]\nutility_window = 0\n";
    let result = migrate_policy_provider_and_utility_window(src).expect("migrate");
    assert_eq!(result.changed_count, 0);
    assert_eq!(
        result.output, src,
        "output must be unchanged when both already present"
    );
}

#[test]
fn migrate_policy_provider_and_utility_window_adds_only_missing_field() {
    let src = "[tools.policy]\npolicy_provider = \"\"\n";
    let result = migrate_policy_provider_and_utility_window(src).expect("migrate");
    assert_eq!(result.changed_count, 1);
    assert!(
        result.output.contains("# utility_window"),
        "must add utility_window comment"
    );
    assert_eq!(
        result.output.matches("policy_provider").count(),
        1,
        "must not duplicate policy_provider"
    );
}

#[test]
fn migrate_policy_provider_and_utility_window_round_trips_through_config() {
    use toml::Value;
    let src = "[agent]\nname = \"z\"\n";
    let result = migrate_policy_provider_and_utility_window(src).expect("migrate");
    // Strip comment lines, keep only non-comment TOML lines, and verify it parses.
    let toml_only: String = result
        .output
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n");
    toml::from_str::<Value>(&toml_only).expect("stripped output must be valid TOML");
}

// ── Step 60 — migrate_shell_checkpoints_config (#4990) ───────────────────

#[test]
fn step_60_adds_checkpoints_block_when_absent() {
    let src = "[agent]\nname = \"Zeph\"\n";
    let result = migrate_shell_checkpoints_config(src).expect("migrate");
    assert_eq!(result.changed_count, 1);
    assert!(
        result.sections_changed.contains(&"tools.shell".to_owned()),
        "sections_changed must include 'tools.shell'"
    );
    assert!(
        result.output.contains("checkpoints_enabled"),
        "output must contain checkpoints_enabled"
    );
    assert!(
        result.output.contains("max_checkpoints"),
        "output must contain max_checkpoints"
    );
}

#[test]
fn step_60_noop_when_checkpoints_enabled_present() {
    let src = "[tools.shell]\ncheckpoints_enabled = true\n";
    let result = migrate_shell_checkpoints_config(src).expect("migrate");
    assert_eq!(result.changed_count, 0);
    assert_eq!(result.output, src);
}

#[test]
fn step_60_noop_when_max_checkpoints_present() {
    let src = "[tools.shell]\nmax_checkpoints = 50\n";
    let result = migrate_shell_checkpoints_config(src).expect("migrate");
    assert_eq!(result.changed_count, 0);
    assert_eq!(result.output, src);
}

#[test]
fn step_60_idempotent_on_own_output() {
    let src = "[agent]\nname = \"Zeph\"\n";
    let first = migrate_shell_checkpoints_config(src).expect("migrate");
    assert_eq!(first.changed_count, 1);
    let second = migrate_shell_checkpoints_config(&first.output).expect("second migrate");
    assert_eq!(second.changed_count, 0, "second run must be a no-op");
    assert_eq!(
        second.output, first.output,
        "output must be unchanged on second run"
    );
}

// ── Step 63 — migrate_memory_graph_recall_include_imported (#5015) ────────

#[test]
fn step_63_adds_recall_include_imported_when_absent() {
    let src = "[memory.graph]\nenabled = true\n";
    let result = migrate_memory_graph_recall_include_imported(src).expect("migrate");
    assert_eq!(result.changed_count, 1);
    assert!(
        result
            .sections_changed
            .contains(&"memory.graph.recall_include_imported".to_owned()),
        "sections_changed must include memory.graph.recall_include_imported"
    );
    assert!(
        result.output.contains("recall_include_imported"),
        "output must contain recall_include_imported"
    );
}

#[test]
fn step_63_noop_when_no_memory_graph_section() {
    let src = "[agent]\nname = \"Zeph\"\n";
    let result = migrate_memory_graph_recall_include_imported(src).expect("migrate");
    assert_eq!(result.changed_count, 0);
    assert_eq!(result.output, src);
}

#[test]
fn step_63_noop_when_key_already_in_memory_graph() {
    let src = "[memory.graph]\nenabled = true\nrecall_include_imported = false\n";
    let result = migrate_memory_graph_recall_include_imported(src).expect("migrate");
    assert_eq!(result.changed_count, 0);
    assert_eq!(result.output, src);
}

#[test]
fn step_63_noop_when_key_commented_in_memory_graph() {
    let src = "[memory.graph]\nenabled = true\n# recall_include_imported = true\n";
    let result = migrate_memory_graph_recall_include_imported(src).expect("migrate");
    assert_eq!(result.changed_count, 0);
    assert_eq!(result.output, src);
}

/// Regression for #5109: step 61 adds a `recall_include_imported` comment inside [knowledge].
/// Step 63 must NOT treat that as an idempotency hit — it must still inject into [memory.graph].
#[test]
fn step_63_not_fooled_by_recall_include_imported_in_knowledge_block() {
    // Simulates a config that has been processed by step 61, which adds a [knowledge]
    // advisory containing "recall_include_imported" as a comment line.
    let src = "[memory.graph]\nenabled = true\n\n# [knowledge]\n# recall_include_imported = true\n";
    let result = migrate_memory_graph_recall_include_imported(src).expect("migrate");
    assert_eq!(
        result.changed_count, 1,
        "must inject into [memory.graph] even when recall_include_imported appears in [knowledge]"
    );
    assert!(
        result.output.contains("recall_include_imported"),
        "output must contain the injected key"
    );
}

#[test]
fn step_63_idempotent_on_own_output() {
    let src = "[memory.graph]\nenabled = true\n";
    let first = migrate_memory_graph_recall_include_imported(src).expect("migrate");
    assert_eq!(first.changed_count, 1);
    let second =
        migrate_memory_graph_recall_include_imported(&first.output).expect("second migrate");
    assert_eq!(second.changed_count, 0, "second run must be a no-op");
    assert_eq!(
        second.output, first.output,
        "output must be unchanged on second run"
    );
}

// ── Step 65 — migrate_tui_theme_config (#5087) ────────────────────────────

#[test]
fn step_65_adds_tui_theme_block_when_absent() {
    let src = "[tui]\ntool_density = \"compact\"\n";
    let result = migrate_tui_theme_config(src).expect("migrate");
    assert_eq!(result.changed_count, 1);
    assert!(
        result.sections_changed.contains(&"tui.theme".to_owned()),
        "sections_changed must include tui.theme"
    );
    assert!(
        result.output.contains("tui.theme"),
        "output must reference tui.theme"
    );
}

#[test]
fn step_65_noop_when_tui_theme_section_present() {
    let src = "[tui]\ntool_density = \"compact\"\n\n[tui.theme]\nname = \"zephyr\"\n";
    let result = migrate_tui_theme_config(src).expect("migrate");
    assert_eq!(result.changed_count, 0);
    assert_eq!(result.output, src);
}

#[test]
fn step_65_idempotent_on_own_output() {
    let src = "[tui]\ntool_density = \"compact\"\n";
    let first = migrate_tui_theme_config(src).expect("first migrate");
    assert_eq!(first.changed_count, 1);
    let second = migrate_tui_theme_config(&first.output).expect("second migrate");
    assert_eq!(second.changed_count, 0, "second run must be a no-op");
    assert_eq!(
        second.output, first.output,
        "output unchanged on second run"
    );
}

// ── Step 66 — migrate_tui_theme_defaults (#5091) ──────────────────────────

#[test]
fn step_66_inserts_defaults_when_section_present_but_keys_absent() {
    let src = "[tui]\ntool_density = \"compact\"\n\n[tui.theme]\n";
    let result = migrate_tui_theme_defaults(src).expect("migrate");
    assert_eq!(result.changed_count, 1);
    assert!(
        result.sections_changed.contains(&"tui.theme".to_owned()),
        "sections_changed must include tui.theme"
    );
    assert!(
        result.output.contains("name"),
        "output must contain name key"
    );
    assert!(
        result.output.contains("color_mode"),
        "output must contain color_mode key"
    );
    assert!(
        result.output.contains("zephyr"),
        "name default must be zephyr"
    );
    assert!(
        result.output.contains("auto"),
        "color_mode default must be auto"
    );
}

#[test]
fn step_66_noop_when_section_absent() {
    let src = "[tui]\ntool_density = \"compact\"\n";
    let result = migrate_tui_theme_defaults(src).expect("migrate");
    assert_eq!(result.changed_count, 0);
    assert_eq!(result.output, src);
}

#[test]
fn step_66_noop_when_both_keys_present() {
    let src = "[tui]\ntool_density = \"compact\"\n\n[tui.theme]\nname = \"gruvbox-dark\"\ncolor_mode = \"truecolor\"\n";
    let result = migrate_tui_theme_defaults(src).expect("migrate");
    assert_eq!(result.changed_count, 0);
    assert_eq!(result.output, src);
}

#[test]
fn step_66_idempotent_on_own_output() {
    let src = "[tui]\ntool_density = \"compact\"\n\n[tui.theme]\n";
    let first = migrate_tui_theme_defaults(src).expect("first migrate");
    assert_eq!(first.changed_count, 1);
    let second = migrate_tui_theme_defaults(&first.output).expect("second migrate");
    assert_eq!(second.changed_count, 0, "second run must be a no-op");
    assert_eq!(
        second.output, first.output,
        "output unchanged on second run"
    );
}

#[test]
fn step_66_inserts_only_missing_key_when_one_present() {
    let src = "[tui.theme]\nname = \"classic\"\n";
    let result = migrate_tui_theme_defaults(src).expect("migrate");
    assert_eq!(result.changed_count, 1);
    assert!(
        result.output.contains("color_mode"),
        "color_mode must be inserted"
    );
    // name must not be duplicated
    assert_eq!(
        result.output.matches("name").count(),
        1,
        "name must appear exactly once"
    );
}

// ── Step 67 — migrate_tui_delights (#5104) ────────────────────────────────

#[test]
fn step_67_injects_delights_block_when_tui_present_and_no_delights() {
    let src = "[tui]\nmotion = \"full\"\n";
    let result = migrate_tui_delights(src).expect("migrate");
    assert_eq!(result.changed_count, 1);
    assert!(
        result.output.contains("[tui.delights]"),
        "advisory block must be injected"
    );
    assert!(
        result.output.contains("stream_metrics"),
        "stream_metrics key must be in advisory"
    );
    assert_eq!(result.sections_changed, vec!["tui.delights"]);
}

#[test]
fn step_67_noop_when_tui_delights_already_present() {
    let src = "[tui]\nmotion = \"full\"\n\n[tui.delights]\nstream_metrics = false\n";
    let result = migrate_tui_delights(src).expect("migrate");
    assert_eq!(result.changed_count, 0);
    assert_eq!(result.output, src);
}

#[test]
fn step_67_noop_when_no_tui_section() {
    let src = "[agent]\nname = \"zeph\"\n";
    let result = migrate_tui_delights(src).expect("migrate");
    assert_eq!(result.changed_count, 0);
    assert_eq!(result.output, src);
}

#[test]
fn step_67_idempotent_on_commented_delights() {
    // Simulate a config where the advisory was already injected (commented-out header present).
    let src = "[tui]\nmotion = \"full\"\n\n# [tui.delights]\n# stream_metrics = true\n";
    let result = migrate_tui_delights(src).expect("migrate");
    assert_eq!(
        result.changed_count, 0,
        "must not inject twice when commented-out delights already present"
    );
}

// ── Step 68 — migrate_tui_mouse (#5103) ───────────────────────────────────

#[test]
fn step_68_injects_mouse_comment_when_tui_present_and_no_mouse() {
    let src = "[tui]\nmotion = \"full\"\n";
    let result = migrate_tui_mouse(src).expect("migrate");
    assert_eq!(result.changed_count, 1);
    assert!(
        result.output.contains("mouse"),
        "advisory mouse comment must be injected"
    );
    assert_eq!(result.sections_changed, vec!["tui"]);
}

#[test]
fn step_68_noop_when_no_tui_section() {
    let src = "[agent]\nname = \"zeph\"\n";
    let result = migrate_tui_mouse(src).expect("migrate");
    assert_eq!(result.changed_count, 0);
    assert_eq!(result.output, src);
}

#[test]
fn step_68_noop_when_mouse_already_present() {
    let src = "[tui]\nmouse = false\n";
    let result = migrate_tui_mouse(src).expect("migrate");
    assert_eq!(result.changed_count, 0);
    assert_eq!(result.output, src);
}

#[test]
fn step_68_idempotent_on_own_output() {
    let src = "[tui]\nmotion = \"full\"\n";
    let first = migrate_tui_mouse(src).expect("first migrate");
    assert_eq!(first.changed_count, 1);
    let second = migrate_tui_mouse(&first.output).expect("second migrate");
    assert_eq!(second.changed_count, 0, "second run must be a no-op");
    assert_eq!(
        second.output, first.output,
        "output unchanged on second run"
    );
}

// ── Step 69 — migrate_orchestration_asset_sensitivity (spec-068, #3934) ──

#[test]
fn step_69_injects_asset_sensitivity_when_orchestration_present() {
    let src = "[orchestration]\nenabled = true\n";
    let result = migrate_orchestration_asset_sensitivity(src).expect("migrate");
    assert_eq!(result.changed_count, 1);
    assert!(
        result.output.contains("default_asset_sensitivity"),
        "advisory comment must be injected"
    );
    assert_eq!(
        result.sections_changed,
        vec!["orchestration.default_asset_sensitivity"]
    );
}

#[test]
fn step_69_noop_when_no_orchestration_section() {
    let src = "[agent]\nname = \"zeph\"\n";
    let result = migrate_orchestration_asset_sensitivity(src).expect("migrate");
    assert_eq!(result.changed_count, 0);
    assert_eq!(result.output, src);
}

#[test]
fn step_69_noop_when_key_already_present() {
    let src = "[orchestration]\ndefault_asset_sensitivity = \"public\"\n";
    let result = migrate_orchestration_asset_sensitivity(src).expect("migrate");
    assert_eq!(result.changed_count, 0);
    assert_eq!(result.output, src);
}

#[test]
fn step_69_idempotent_on_own_output() {
    let src = "[orchestration]\nenabled = true\n";
    let first = migrate_orchestration_asset_sensitivity(src).expect("first migrate");
    assert_eq!(first.changed_count, 1);
    let second = migrate_orchestration_asset_sensitivity(&first.output).expect("second migrate");
    assert_eq!(second.changed_count, 0, "second run must be a no-op");
    assert_eq!(
        second.output, first.output,
        "output unchanged on second run"
    );
}

// ── Step 70 — migrate_session_persistence_config (spec-068-session-persistence, #5343) ──

#[test]
fn step_70_injects_session_persistence_keys_when_session_present() {
    let src = "[session]\nprovider_persistence = true\n";
    let result = migrate_session_persistence_config(src).expect("migrate");
    assert_eq!(result.changed_count, 1);
    assert!(result.output.contains("max_event_log_mb"));
    assert!(result.output.contains("[session.condense]"));
    assert_eq!(
        result.sections_changed,
        vec!["session".to_owned(), "session.condense".to_owned()]
    );
}

#[test]
fn step_70_noop_when_no_session_section() {
    let src = "[agent]\nname = \"zeph\"\n";
    let result = migrate_session_persistence_config(src).expect("migrate");
    assert_eq!(result.changed_count, 0);
    assert_eq!(result.output, src);
}

#[test]
fn step_70_noop_when_key_already_present() {
    let src = "[session]\nmax_event_log_mb = 256\n";
    let result = migrate_session_persistence_config(src).expect("migrate");
    assert_eq!(result.changed_count, 0);
    assert_eq!(result.output, src);
}

#[test]
fn step_70_idempotent_on_own_output() {
    let src = "[session]\nprovider_persistence = true\n";
    let first = migrate_session_persistence_config(src).expect("first migrate");
    assert_eq!(first.changed_count, 1);
    let second = migrate_session_persistence_config(&first.output).expect("second migrate");
    assert_eq!(second.changed_count, 0, "second run must be a no-op");
    assert_eq!(
        second.output, first.output,
        "output unchanged on second run"
    );
}

// ── Step 71 — migrate_serve_config (spec-068 §9, #5343) ──

#[test]
fn step_71_adds_serve_block_when_absent() {
    let src = "[agent]\nname = \"zeph\"\n";
    let result = migrate_serve_config(src).expect("migrate");
    assert_eq!(result.changed_count, 1);
    assert!(result.output.contains("# [serve]"));
    assert!(result.output.contains("http_addr"));
    assert_eq!(result.sections_changed, vec!["serve".to_owned()]);
}

#[test]
fn step_71_noop_when_serve_section_already_active() {
    let src = "[serve]\nhttp_addr = \"127.0.0.1:8420\"\n";
    let result = migrate_serve_config(src).expect("migrate");
    assert_eq!(result.changed_count, 0);
    assert_eq!(result.output, src);
}

#[test]
fn step_71_noop_when_serve_comment_already_present() {
    let src = "# [serve]\n# http_addr = \"127.0.0.1:8420\"\n";
    let result = migrate_serve_config(src).expect("migrate");
    assert_eq!(result.changed_count, 0);
    assert_eq!(result.output, src);
}

#[test]
fn step_71_idempotent_on_own_output() {
    let src = "[agent]\nname = \"zeph\"\n";
    let first = migrate_serve_config(src).expect("first migrate");
    assert_eq!(first.changed_count, 1);
    let second = migrate_serve_config(&first.output).expect("second migrate");
    assert_eq!(second.changed_count, 0, "second run must be a no-op");
    assert_eq!(
        second.output, first.output,
        "output unchanged on second run"
    );
}

// ── migrate_nli_config tests (step 72, #5438) ────────────────────────────

#[test]
fn step_72_adds_nli_block_when_absent() {
    let src = "[agent]\nname = \"zeph\"\n";
    let result = migrate_nli_config(src).expect("migrate");
    assert_eq!(result.changed_count, 1);
    assert!(result.output.contains("# [security.content_isolation.nli]"));
    assert!(result.output.contains("# enabled = false"));
    assert_eq!(
        result.sections_changed,
        vec!["security.content_isolation.nli".to_owned()]
    );
}

#[test]
fn step_72_noop_when_nli_section_already_active() {
    let src = "[security.content_isolation.nli]\nenabled = true\n";
    let result = migrate_nli_config(src).expect("migrate");
    assert_eq!(result.changed_count, 0);
    assert_eq!(result.output, src);
}

#[test]
fn step_72_noop_when_nli_comment_already_present() {
    let src = "# [security.content_isolation.nli]\n# enabled = false\n";
    let result = migrate_nli_config(src).expect("migrate");
    assert_eq!(result.changed_count, 0);
    assert_eq!(result.output, src);
}

#[test]
fn step_72_idempotent_on_own_output() {
    let src = "[agent]\nname = \"zeph\"\n";
    let first = migrate_nli_config(src).expect("first migrate");
    assert_eq!(first.changed_count, 1);
    let second = migrate_nli_config(&first.output).expect("second migrate");
    assert_eq!(second.changed_count, 0, "second run must be a no-op");
    assert_eq!(
        second.output, first.output,
        "output unchanged on second run"
    );
}

// ── migrate_secret_masking_config tests (step 73, #5437) ─────────────────

#[test]
fn step_73_adds_secret_masking_block_when_absent() {
    let src = "[agent]\nname = \"zeph\"\n";
    let result = migrate_secret_masking_config(src).expect("migrate");
    assert_eq!(result.changed_count, 1);
    assert!(
        result
            .output
            .contains("# [security.content_isolation.secret_masking]")
    );
    assert!(result.output.contains("# enabled = false"));
    assert_eq!(
        result.sections_changed,
        vec!["security.content_isolation.secret_masking".to_owned()]
    );
}

#[test]
fn step_73_noop_when_secret_masking_section_already_active() {
    let src = "[security.content_isolation.secret_masking]\nenabled = true\n";
    let result = migrate_secret_masking_config(src).expect("migrate");
    assert_eq!(result.changed_count, 0);
    assert_eq!(result.output, src);
}

#[test]
fn step_73_noop_when_secret_masking_comment_already_present() {
    let src = "# [security.content_isolation.secret_masking]\n# enabled = false\n";
    let result = migrate_secret_masking_config(src).expect("migrate");
    assert_eq!(result.changed_count, 0);
    assert_eq!(result.output, src);
}

#[test]
fn step_73_idempotent_on_own_output() {
    let src = "[agent]\nname = \"zeph\"\n";
    let first = migrate_secret_masking_config(src).expect("first migrate");
    assert_eq!(first.changed_count, 1);
    let second = migrate_secret_masking_config(&first.output).expect("second migrate");
    assert_eq!(second.changed_count, 0, "second run must be a no-op");
    assert_eq!(
        second.output, first.output,
        "output unchanged on second run"
    );
}

// ── migrate_pii_filter_names tests (step 74, #5530) ──────────────────────

#[test]
fn step_74_adds_commented_advisory_when_pii_filter_active_and_missing_field() {
    let src = "[security.pii_filter]\nenabled = true\nfilter_email = true\n";
    let result = migrate_pii_filter_names(src).expect("migrate");
    assert_eq!(result.changed_count, 1);
    // Advisory only — must NOT force-activate the noisy heuristic for existing installs (S1).
    assert!(result.output.contains("# filter_names = false"));
    assert!(!result.output.contains("\nfilter_names ="));
    assert!(result.output.contains("enabled = true"));
    assert!(result.output.contains("filter_email = true"));
    assert_eq!(
        result.sections_changed,
        vec!["security.pii_filter".to_owned()]
    );
}

#[test]
fn step_74_noop_when_filter_names_already_present() {
    let src = "[security.pii_filter]\nenabled = true\nfilter_names = false\n";
    let result = migrate_pii_filter_names(src).expect("migrate");
    assert_eq!(result.changed_count, 0);
    assert_eq!(result.output, src);
}

#[test]
fn step_74_noop_when_filter_names_comment_already_present() {
    let src = "[security.pii_filter]\nenabled = true\n# filter_names = false\n";
    let result = migrate_pii_filter_names(src).expect("migrate");
    assert_eq!(result.changed_count, 0);
    assert_eq!(result.output, src);
}

#[test]
fn step_74_noop_when_pii_filter_section_absent() {
    let src = "[agent]\nname = \"zeph\"\n";
    let result = migrate_pii_filter_names(src).expect("migrate");
    assert_eq!(result.changed_count, 0);
    assert_eq!(result.output, src);
}

#[test]
fn step_74_idempotent_on_own_output() {
    let src = "[security.pii_filter]\nenabled = true\n";
    let first = migrate_pii_filter_names(src).expect("first migrate");
    assert_eq!(first.changed_count, 1);
    let second = migrate_pii_filter_names(&first.output).expect("second migrate");
    assert_eq!(second.changed_count, 0, "second run must be a no-op");
    assert_eq!(
        second.output, first.output,
        "output unchanged on second run"
    );
}
