// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Acceptance tests for the `[tools.ask_user]` and `[llm.cache_keepalive]` config sections
//! (specs 090 / 091, FR-002 / FR-010 / FR-011): both default to disabled, the numeric limits
//! are non-zero by construction, and the three defaults TOML files document every key.

use std::num::{NonZeroU8, NonZeroU32, NonZeroU64};

use zeph_config::{AskUserConfig, CacheKeepaliveConfig, Config};

fn merge(base: &mut toml::Table, overlay: toml::Table) {
    for (k, v) in overlay {
        match (base.get_mut(&k), v) {
            (Some(toml::Value::Table(b)), toml::Value::Table(o)) => merge(b, o),
            (_, v) => {
                base.insert(k, v);
            }
        }
    }
}

/// Overlays `overlay` on the serialized default config, since `Config` has required sections.
fn parse(overlay: &str) -> Result<Config, toml::de::Error> {
    let mut base: toml::Table =
        toml::Table::try_from(Config::default()).expect("serialize default");
    merge(&mut base, toml::from_str(overlay)?);
    base.try_into()
}

#[test]
fn ask_user_config_default_is_disabled_with_plan_limits() {
    let cfg = AskUserConfig::default();
    assert!(!cfg.enabled, "ask_user must be opt-in");
    assert_eq!(cfg.timeout_secs, NonZeroU64::new(300).unwrap());
    assert_eq!(cfg.max_questions_per_turn, NonZeroU8::new(3).unwrap());
}

#[test]
fn cache_keepalive_config_default_is_disabled_with_plan_limits() {
    let cfg = CacheKeepaliveConfig::default();
    assert!(!cfg.enabled, "keepalive must be opt-in (spec 091 FR-002)");
    assert!(cfg.keepalive_provider.is_empty());
    assert_eq!(cfg.max_consecutive_pings, NonZeroU32::new(6).unwrap());
}

#[test]
fn config_default_has_both_sections_disabled() {
    let cfg = Config::default();
    assert!(!cfg.tools.ask_user.enabled);
    assert!(!cfg.llm.cache_keepalive.enabled);
}

#[test]
fn config_parse_ask_user_section_reaches_fields() {
    let cfg =
        parse("[tools.ask_user]\nenabled = true\ntimeout_secs = 45\nmax_questions_per_turn = 5\n")
            .expect("valid ask_user section");
    assert!(cfg.tools.ask_user.enabled);
    assert_eq!(cfg.tools.ask_user.timeout_secs.get(), 45);
    assert_eq!(cfg.tools.ask_user.max_questions_per_turn.get(), 5);
}

#[test]
fn config_parse_cache_keepalive_section_reaches_fields() {
    let cfg = parse(
        "[llm.cache_keepalive]\nenabled = true\nkeepalive_provider = \"quality\"\nmax_consecutive_pings = 2\n",
    )
    .expect("valid cache_keepalive section");
    assert!(cfg.llm.cache_keepalive.enabled);
    assert_eq!(
        cfg.llm.cache_keepalive.keepalive_provider.as_str(),
        "quality"
    );
    assert_eq!(cfg.llm.cache_keepalive.max_consecutive_pings.get(), 2);
}

#[test]
fn config_parse_ask_user_zero_timeout_is_rejected() {
    assert!(parse("[tools.ask_user]\ntimeout_secs = 0\n").is_err());
}

#[test]
fn config_parse_ask_user_zero_question_cap_is_rejected() {
    assert!(parse("[tools.ask_user]\nmax_questions_per_turn = 0\n").is_err());
}

#[test]
fn config_parse_cache_keepalive_zero_ping_cap_is_rejected() {
    assert!(parse("[llm.cache_keepalive]\nmax_consecutive_pings = 0\n").is_err());
}

#[test]
fn config_parse_ask_user_rejects_negative_question_cap() {
    assert!(parse("[tools.ask_user]\nmax_questions_per_turn = -1\n").is_err());
}

#[test]
fn default_toml_files_document_every_new_key() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root");
    for rel in [
        "config/default.toml",
        "crates/zeph-core/config/default.toml",
    ] {
        let content = std::fs::read_to_string(root.join(rel))
            .unwrap_or_else(|e| panic!("failed to read {rel}: {e}"));
        for key in [
            "ask_user",
            "max_questions_per_turn",
            "cache_keepalive",
            "keepalive_provider",
            "max_consecutive_pings",
        ] {
            assert!(content.contains(key), "missing `{key}` in {rel}");
        }
    }
}
