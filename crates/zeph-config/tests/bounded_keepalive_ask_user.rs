// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0
#![expect(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "Fixture construction failures should fail the test immediately.; Fixture construction failures should fail the test immediately."
)]

//! Acceptance tests (tests-first) for #7129: `KeepalivePingCap` (1..=50) and `AskUserTimeout`
//! (1..=3600 s) are bounded newtypes enforced at construction and at deserialization.

use zeph_config::{AskUserConfig, AskUserTimeout, CacheKeepaliveConfig, Config, KeepalivePingCap};

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

fn parse(overlay: &str) -> Result<Config, toml::de::Error> {
    let mut base: toml::Table =
        toml::Table::try_from(Config::default()).expect("serialize default");
    merge(&mut base, toml::from_str(overlay)?);
    base.try_into()
}

#[test]
fn keepalive_ping_cap_new_rejects_zero() {
    assert!(KeepalivePingCap::new(0).is_err());
}

#[test]
fn keepalive_ping_cap_new_accepts_bounds_and_get_round_trips() {
    assert_eq!(KeepalivePingCap::new(1).unwrap().get(), 1);
    assert_eq!(KeepalivePingCap::new(50).unwrap().get(), 50);
}

#[test]
fn keepalive_ping_cap_new_rejects_above_max() {
    assert!(KeepalivePingCap::new(51).is_err());
    assert!(KeepalivePingCap::new(u32::MAX).is_err());
}

#[test]
fn ask_user_timeout_new_rejects_zero() {
    assert!(AskUserTimeout::new(0).is_err());
}

#[test]
fn ask_user_timeout_new_accepts_bounds_and_get_round_trips() {
    assert_eq!(AskUserTimeout::new(1).unwrap().get(), 1);
    assert_eq!(AskUserTimeout::new(3600).unwrap().get(), 3600);
}

#[test]
fn ask_user_timeout_new_rejects_above_max() {
    assert!(AskUserTimeout::new(3601).is_err());
    assert!(AskUserTimeout::new(u64::MAX).is_err());
}

#[test]
fn cache_keepalive_config_default_cap_is_six() {
    assert_eq!(
        CacheKeepaliveConfig::default().max_consecutive_pings.get(),
        6
    );
}

#[test]
fn ask_user_config_default_timeout_is_300() {
    assert_eq!(AskUserConfig::default().timeout_secs.get(), 300);
}

#[test]
fn cache_keepalive_toml_max_consecutive_pings_above_max_fails_to_load() {
    assert!(parse("[llm.cache_keepalive]\nmax_consecutive_pings = 51\n").is_err());
}

#[test]
fn cache_keepalive_toml_max_consecutive_pings_zero_fails_to_load() {
    assert!(parse("[llm.cache_keepalive]\nmax_consecutive_pings = 0\n").is_err());
}

#[test]
fn cache_keepalive_toml_max_consecutive_pings_at_max_loads() {
    let cfg = parse("[llm.cache_keepalive]\nmax_consecutive_pings = 50\n").unwrap();
    assert_eq!(cfg.llm.cache_keepalive.max_consecutive_pings.get(), 50);
}

#[test]
fn ask_user_toml_timeout_above_max_fails_to_load() {
    assert!(parse("[tools.ask_user]\ntimeout_secs = 3601\n").is_err());
}

#[test]
fn ask_user_toml_timeout_zero_fails_to_load() {
    assert!(parse("[tools.ask_user]\ntimeout_secs = 0\n").is_err());
}

#[test]
fn ask_user_toml_timeout_at_max_loads() {
    let cfg = parse("[tools.ask_user]\ntimeout_secs = 3600\n").unwrap();
    assert_eq!(cfg.tools.ask_user.timeout_secs.get(), 3600);
}

#[test]
fn bounded_newtypes_serialize_back_to_plain_integers() {
    let cfg = parse(
        "[llm.cache_keepalive]\nmax_consecutive_pings = 7\n[tools.ask_user]\ntimeout_secs = 90\n",
    )
    .unwrap();
    let table = toml::Table::try_from(&cfg).unwrap();
    let ka = table["llm"]["cache_keepalive"]["max_consecutive_pings"].as_integer();
    let au = table["tools"]["ask_user"]["timeout_secs"].as_integer();
    assert_eq!((ka, au), (Some(7), Some(90)));
}
