// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use super::*;
use std::assert_matches;

fn ollama_entry() -> ProviderEntry {
    ProviderEntry {
        provider_type: ProviderKind::Ollama,
        name: Some("ollama".into()),
        model: Some("qwen3:8b".into()),
        ..Default::default()
    }
}

fn claude_entry() -> ProviderEntry {
    ProviderEntry {
        provider_type: ProviderKind::Claude,
        name: Some("claude".into()),
        model: Some("claude-sonnet-5".into()),
        max_tokens: Some(8192),
        ..Default::default()
    }
}

// ─── ProviderEntry::validate ─────────────────────────────────────────────

#[test]
fn validate_ollama_valid() {
    assert!(ollama_entry().validate().is_ok());
}

#[test]
fn validate_claude_valid() {
    assert!(claude_entry().validate().is_ok());
}

#[test]
fn validate_compatible_without_name_errors() {
    let entry = ProviderEntry {
        provider_type: ProviderKind::Compatible,
        name: None,
        ..Default::default()
    };
    let err = entry.validate().unwrap_err();
    assert!(
        err.to_string().contains("compatible"),
        "error should mention compatible: {err}"
    );
}

#[test]
fn validate_compatible_with_name_ok() {
    let entry = ProviderEntry {
        provider_type: ProviderKind::Compatible,
        name: Some("my-proxy".into()),
        base_url: Some("http://localhost:8080".into()),
        model: Some("gpt-4o".into()),
        max_tokens: Some(4096),
        ..Default::default()
    };
    assert!(entry.validate().is_ok());
}

#[test]
fn validate_openai_valid() {
    let entry = ProviderEntry {
        provider_type: ProviderKind::OpenAi,
        name: Some("openai".into()),
        model: Some("gpt-4o".into()),
        max_tokens: Some(4096),
        ..Default::default()
    };
    assert!(entry.validate().is_ok());
}

#[test]
fn validate_gemini_valid() {
    let entry = ProviderEntry {
        provider_type: ProviderKind::Gemini,
        name: Some("gemini".into()),
        model: Some("gemini-2.0-flash".into()),
        ..Default::default()
    };
    assert!(entry.validate().is_ok());
}

// ─── validate_pool ───────────────────────────────────────────────────────

#[test]
fn validate_pool_empty_errors() {
    let err = validate_pool(&[]).unwrap_err();
    assert!(err.to_string().contains("at least one"), "{err}");
}

#[test]
fn validate_pool_single_entry_ok() {
    assert!(validate_pool(&[ollama_entry()]).is_ok());
}

#[test]
fn validate_pool_duplicate_names_errors() {
    let a = ollama_entry();
    let b = ollama_entry(); // same effective name "ollama"
    let err = validate_pool(&[a, b]).unwrap_err();
    assert!(err.to_string().contains("duplicate"), "{err}");
}

#[test]
fn validate_pool_multiple_defaults_errors() {
    let mut a = ollama_entry();
    let mut b = claude_entry();
    a.default = true;
    b.default = true;
    let err = validate_pool(&[a, b]).unwrap_err();
    assert!(err.to_string().contains("default"), "{err}");
}

#[test]
fn validate_pool_two_different_providers_ok() {
    assert!(validate_pool(&[ollama_entry(), claude_entry()]).is_ok());
}

#[test]
fn validate_pool_propagates_entry_error() {
    let bad = ProviderEntry {
        provider_type: ProviderKind::Compatible,
        name: None, // invalid: compatible without name
        ..Default::default()
    };
    assert!(validate_pool(&[bad]).is_err());
}

// ─── ProviderEntry::effective_model ──────────────────────────────────────

#[test]
fn effective_model_returns_explicit_when_set() {
    let entry = ProviderEntry {
        provider_type: ProviderKind::Claude,
        model: Some("claude-sonnet-5".into()),
        ..Default::default()
    };
    assert_eq!(entry.effective_model(), "claude-sonnet-5");
}

#[test]
fn effective_model_ollama_default_when_none() {
    let entry = ProviderEntry {
        provider_type: ProviderKind::Ollama,
        model: None,
        ..Default::default()
    };
    assert_eq!(entry.effective_model(), "qwen3:8b");
}

#[test]
fn effective_model_claude_default_when_none() {
    let entry = ProviderEntry {
        provider_type: ProviderKind::Claude,
        model: None,
        ..Default::default()
    };
    assert_eq!(entry.effective_model(), "claude-haiku-4-5-20251001");
}

#[test]
fn effective_model_openai_default_when_none() {
    let entry = ProviderEntry {
        provider_type: ProviderKind::OpenAi,
        model: None,
        ..Default::default()
    };
    assert_eq!(entry.effective_model(), "gpt-4o-mini");
}

#[test]
fn effective_model_gemini_default_when_none() {
    let entry = ProviderEntry {
        provider_type: ProviderKind::Gemini,
        model: None,
        ..Default::default()
    };
    assert_eq!(entry.effective_model(), "gemini-2.0-flash");
}

// ─── LlmConfig::check_legacy_format ──────────────────────────────────────

// Parse a complete TOML snippet that includes the [llm] header.
fn parse_llm(toml: &str) -> LlmConfig {
    #[derive(serde::Deserialize)]
    struct Wrapper {
        llm: LlmConfig,
    }
    toml::from_str::<Wrapper>(toml).unwrap().llm
}

#[test]
fn check_legacy_format_new_format_ok() {
    let cfg = parse_llm(
        r#"
[llm]

[[llm.providers]]
type = "ollama"
model = "qwen3:8b"
"#,
    );
    assert!(cfg.check_legacy_format().is_ok());
}

#[test]
fn check_legacy_format_empty_providers_no_legacy_ok() {
    // No providers, no legacy fields — passes (empty [llm] is acceptable here)
    let cfg = parse_llm("[llm]\n");
    assert!(cfg.check_legacy_format().is_ok());
}

// ─── LlmConfig::effective_* helpers ──────────────────────────────────────

#[test]
fn effective_provider_falls_back_to_ollama_when_no_providers() {
    let cfg = parse_llm("[llm]\n");
    assert_eq!(cfg.effective_provider(), ProviderKind::Ollama);
}

#[test]
fn effective_provider_reads_from_providers_first() {
    let cfg = parse_llm(
        r#"
[llm]

[[llm.providers]]
type = "claude"
model = "claude-sonnet-5"
"#,
    );
    assert_eq!(cfg.effective_provider(), ProviderKind::Claude);
}

#[test]
fn effective_model_reads_from_providers_first() {
    let cfg = parse_llm(
        r#"
[llm]

[[llm.providers]]
type = "ollama"
model = "qwen3:8b"
"#,
    );
    assert_eq!(cfg.effective_model(), "qwen3:8b");
}

#[test]
fn effective_model_skips_embed_only_provider() {
    let cfg = parse_llm(
        r#"
[llm]

[[llm.providers]]
type = "ollama"
model = "gemma4:26b"
embed = true

[[llm.providers]]
type = "openai"
model = "gpt-4o-mini"
"#,
    );
    assert_eq!(cfg.effective_model(), "gpt-4o-mini");
}

#[test]
fn effective_base_url_default_when_absent() {
    let cfg = parse_llm("[llm]\n");
    assert_eq!(cfg.effective_base_url(), "http://localhost:11434");
}

#[test]
fn effective_base_url_from_providers_entry() {
    let cfg = parse_llm(
        r#"
[llm]

[[llm.providers]]
type = "ollama"
base_url = "http://myhost:11434"
"#,
    );
    assert_eq!(cfg.effective_base_url(), "http://myhost:11434");
}

// ─── ComplexityRoutingConfig / LlmRoutingStrategy::Triage TOML parsing ──

#[test]
fn complexity_routing_defaults() {
    let cr = ComplexityRoutingConfig::default();
    assert!(
        cr.bypass_single_provider,
        "bypass_single_provider must default to true"
    );
    assert_eq!(cr.triage_timeout_secs, 5);
    assert_eq!(cr.max_triage_tokens, 50);
    assert!(cr.triage_provider.is_none());
    assert!(cr.tiers.simple.is_none());
}

#[test]
fn complexity_routing_toml_round_trip() {
    let cfg = parse_llm(
        r#"
[llm]
routing = "triage"

[llm.complexity_routing]
triage_provider = "fast"
bypass_single_provider = false
triage_timeout_secs = 10
max_triage_tokens = 100

[llm.complexity_routing.tiers]
simple = "fast"
medium = "medium"
complex = "large"
expert = "opus"
"#,
    );
    assert_matches!(cfg.routing, LlmRoutingStrategy::Triage);
    let cr = cfg
        .complexity_routing
        .expect("complexity_routing must be present");
    assert_eq!(
        cr.triage_provider.as_ref().map(ProviderName::as_str),
        Some("fast")
    );
    assert!(!cr.bypass_single_provider);
    assert_eq!(cr.triage_timeout_secs, 10);
    assert_eq!(cr.max_triage_tokens, 100);
    assert_eq!(cr.tiers.simple.as_deref(), Some("fast"));
    assert_eq!(cr.tiers.medium.as_deref(), Some("medium"));
    assert_eq!(cr.tiers.complex.as_deref(), Some("large"));
    assert_eq!(cr.tiers.expert.as_deref(), Some("opus"));
}

#[test]
fn complexity_routing_partial_tiers_toml() {
    // Only simple + complex configured; medium and expert are None.
    let cfg = parse_llm(
        r#"
[llm]
routing = "triage"

[llm.complexity_routing.tiers]
simple = "haiku"
complex = "sonnet"
"#,
    );
    let cr = cfg
        .complexity_routing
        .expect("complexity_routing must be present");
    assert_eq!(cr.tiers.simple.as_deref(), Some("haiku"));
    assert!(cr.tiers.medium.is_none());
    assert_eq!(cr.tiers.complex.as_deref(), Some("sonnet"));
    assert!(cr.tiers.expert.is_none());
    // Defaults still applied.
    assert!(cr.bypass_single_provider);
    assert_eq!(cr.triage_timeout_secs, 5);
}

#[test]
fn routing_strategy_triage_deserialized() {
    let cfg = parse_llm(
        r#"
[llm]
routing = "triage"
"#,
    );
    assert_matches!(cfg.routing, LlmRoutingStrategy::Triage);
}

// ─── stt_provider_entry ───────────────────────────────────────────────────

#[test]
fn stt_provider_entry_by_name_match() {
    let cfg = parse_llm(
        r#"
[llm]

[[llm.providers]]
type = "openai"
name = "quality"
model = "gpt-5.4"
stt_model = "gpt-4o-mini-transcribe"

[llm.stt]
provider = "quality"
"#,
    );
    let entry = cfg.stt_provider_entry().expect("should find stt provider");
    assert_eq!(entry.effective_name(), "quality");
    assert_eq!(entry.stt_model.as_deref(), Some("gpt-4o-mini-transcribe"));
}

#[test]
fn stt_provider_entry_auto_detect_when_provider_empty() {
    let cfg = parse_llm(
        r#"
[llm]

[[llm.providers]]
type = "openai"
name = "openai-stt"
stt_model = "whisper-1"

[llm.stt]
provider = ""
"#,
    );
    let entry = cfg.stt_provider_entry().expect("should auto-detect");
    assert_eq!(entry.effective_name(), "openai-stt");
}

#[test]
fn stt_provider_entry_auto_detect_no_stt_section() {
    let cfg = parse_llm(
        r#"
[llm]

[[llm.providers]]
type = "openai"
name = "openai-stt"
stt_model = "whisper-1"
"#,
    );
    // No [llm.stt] section — should still find first provider with stt_model.
    let entry = cfg.stt_provider_entry().expect("should auto-detect");
    assert_eq!(entry.effective_name(), "openai-stt");
}

#[test]
fn stt_provider_entry_none_when_no_stt_model() {
    let cfg = parse_llm(
        r#"
[llm]

[[llm.providers]]
type = "openai"
name = "quality"
model = "gpt-5.4"
"#,
    );
    assert!(cfg.stt_provider_entry().is_none());
}

#[test]
fn stt_provider_entry_name_mismatch_falls_back_to_none() {
    // Named provider exists but has no stt_model; another unnamed has stt_model.
    let cfg = parse_llm(
        r#"
[llm]

[[llm.providers]]
type = "openai"
name = "quality"
model = "gpt-5.4"

[[llm.providers]]
type = "openai"
name = "openai-stt"
stt_model = "whisper-1"

[llm.stt]
provider = "quality"
"#,
    );
    // "quality" has no stt_model — returns None for name-based lookup.
    assert!(cfg.stt_provider_entry().is_none());
}

#[test]
fn stt_config_deserializes_new_slim_format() {
    let cfg = parse_llm(
        r#"
[llm]

[[llm.providers]]
type = "openai"
name = "quality"
stt_model = "whisper-1"

[llm.stt]
provider = "quality"
language = "en"
"#,
    );
    let stt = cfg.stt.as_ref().expect("stt section present");
    assert_eq!(stt.provider, "quality");
    assert_eq!(stt.language, "en");
}

#[test]
fn stt_config_default_provider_is_empty() {
    // Verify that W4 fix: default provider is empty (auto-detect), not "whisper".
    assert!(ProviderName::default().is_empty());
}

#[test]
fn validate_stt_missing_provider_ok() {
    let cfg = parse_llm("[llm]\n");
    assert!(cfg.validate_stt().is_ok());
}

#[test]
fn validate_stt_valid_reference() {
    let cfg = parse_llm(
        r#"
[llm]

[[llm.providers]]
type = "openai"
name = "quality"
stt_model = "whisper-1"

[llm.stt]
provider = "quality"
"#,
    );
    assert!(cfg.validate_stt().is_ok());
}

#[test]
fn validate_stt_nonexistent_provider_errors() {
    let cfg = parse_llm(
        r#"
[llm]

[[llm.providers]]
type = "openai"
name = "quality"
model = "gpt-5.4"

[llm.stt]
provider = "nonexistent"
"#,
    );
    assert!(cfg.validate_stt().is_err());
}

#[test]
fn validate_stt_provider_exists_but_no_stt_model_returns_ok_with_warn() {
    // MEDIUM: provider is found but has no stt_model — should return Ok (warn path, not error).
    let cfg = parse_llm(
        r#"
[llm]

[[llm.providers]]
type = "openai"
name = "quality"
model = "gpt-5.4"

[llm.stt]
provider = "quality"
"#,
    );
    // validate_stt must succeed (only a tracing::warn is emitted — not an error).
    assert!(cfg.validate_stt().is_ok());
    // stt_provider_entry must return None because no stt_model is set.
    assert!(
        cfg.stt_provider_entry().is_none(),
        "stt_provider_entry must be None when provider has no stt_model"
    );
}

// ─── BanditConfig::warmup_queries deserialization ─────────────────────────

#[test]
fn bandit_warmup_queries_explicit_value_is_deserialized() {
    let cfg = parse_llm(
        r#"
[llm]

[llm.router]
strategy = "bandit"

[llm.router.bandit]
warmup_queries = 50
"#,
    );
    let bandit = cfg
        .router
        .expect("router section must be present")
        .bandit
        .expect("bandit section must be present");
    assert_eq!(
        bandit.warmup_queries,
        Some(50),
        "warmup_queries = 50 must deserialize to Some(50)"
    );
}

#[test]
fn bandit_warmup_queries_explicit_null_is_none() {
    // Explicitly writing the field as absent: field simply not present is
    // equivalent due to #[serde(default)]. Test that an explicit 0 is Some(0).
    let cfg = parse_llm(
        r#"
[llm]

[llm.router]
strategy = "bandit"

[llm.router.bandit]
warmup_queries = 0
"#,
    );
    let bandit = cfg
        .router
        .expect("router section must be present")
        .bandit
        .expect("bandit section must be present");
    // 0 is a valid explicit value — it means "preserve computed default".
    assert_eq!(
        bandit.warmup_queries,
        Some(0),
        "warmup_queries = 0 must deserialize to Some(0)"
    );
}

#[test]
fn bandit_warmup_queries_missing_field_defaults_to_none() {
    // When warmup_queries is omitted entirely, #[serde(default)] must produce None.
    let cfg = parse_llm(
        r#"
[llm]

[llm.router]
strategy = "bandit"

[llm.router.bandit]
alpha = 1.5
"#,
    );
    let bandit = cfg
        .router
        .expect("router section must be present")
        .bandit
        .expect("bandit section must be present");
    assert_eq!(
        bandit.warmup_queries, None,
        "omitted warmup_queries must default to None"
    );
}

#[test]
fn provider_name_new_and_as_str() {
    let n = ProviderName::new("fast");
    assert_eq!(n.as_str(), "fast");
    assert!(!n.is_empty());
}

#[test]
fn provider_name_default_is_empty() {
    let n = ProviderName::default();
    assert!(n.is_empty());
    assert_eq!(n.as_str(), "");
}

#[test]
fn provider_name_partial_eq_str() {
    let n = ProviderName::new("fast");
    assert_eq!(n, "fast");
    assert_ne!(n, "slow");
}

#[test]
fn provider_name_serde_roundtrip() {
    let n = ProviderName::new("my-provider");
    let json = serde_json::to_string(&n).expect("serialize");
    assert_eq!(json, "\"my-provider\"");
    let back: ProviderName = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, n);
}

#[test]
fn provider_name_serde_empty_roundtrip() {
    let n = ProviderName::default();
    let json = serde_json::to_string(&n).expect("serialize");
    assert_eq!(json, "\"\"");
    let back: ProviderName = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, n);
    assert!(back.is_empty());
}

// ─── GonkaNode / ProviderKind::Gonka ─────────────────────────────────────

fn gonka_entry_with_nodes(nodes: Vec<GonkaNode>) -> ProviderEntry {
    ProviderEntry {
        provider_type: ProviderKind::Gonka,
        name: Some("my-gonka".into()),
        gonka_nodes: nodes,
        ..Default::default()
    }
}

fn valid_gonka_nodes() -> Vec<GonkaNode> {
    vec![
        GonkaNode {
            url: "https://node1.gonka.ai".into(),
            address: "gonka1w508d6qejxtdg4y5r3zarvary0c5xw7k2gsyg6".into(),
            name: Some("node1".into()),
        },
        GonkaNode {
            url: "https://node2.gonka.ai".into(),
            address: "gonka14h0ycu78h88wzldxc7e79vhw5xsde0n85evmum".into(),
            name: Some("node2".into()),
        },
        GonkaNode {
            url: "http://node3.internal".into(),
            address: "gonka1qyqszqgpqyqszqgpqyqszqgpqyqszqgpqyqszqg".into(),
            name: None,
        },
    ]
}

#[test]
fn validate_gonka_valid() {
    let entry = gonka_entry_with_nodes(valid_gonka_nodes());
    assert!(entry.validate().is_ok());
}

#[test]
fn validate_gonka_empty_nodes_errors() {
    let entry = gonka_entry_with_nodes(vec![]);
    let err = entry.validate().unwrap_err();
    assert!(
        err.to_string().contains("gonka_nodes"),
        "error should mention gonka_nodes: {err}"
    );
}

#[test]
fn validate_gonka_node_empty_url_errors() {
    let entry = gonka_entry_with_nodes(vec![GonkaNode {
        url: String::new(),
        address: "gonka1test".into(),
        name: None,
    }]);
    let err = entry.validate().unwrap_err();
    assert!(err.to_string().contains("url"), "{err}");
}

#[test]
fn validate_gonka_node_invalid_scheme_errors() {
    let entry = gonka_entry_with_nodes(vec![GonkaNode {
        url: "ftp://node.gonka.ai".into(),
        address: "gonka1test".into(),
        name: None,
    }]);
    let err = entry.validate().unwrap_err();
    assert!(err.to_string().contains("http"), "{err}");
}

#[test]
fn validate_gonka_without_name_errors() {
    let entry = ProviderEntry {
        provider_type: ProviderKind::Gonka,
        name: None,
        gonka_nodes: valid_gonka_nodes(),
        ..Default::default()
    };
    let err = entry.validate().unwrap_err();
    assert!(err.to_string().contains("gonka"), "{err}");
}

#[test]
fn gonka_toml_round_trip() {
    let toml = r#"
[llm]

[[llm.providers]]
type = "gonka"
name = "my-gonka"
gonka_chain_prefix = "custom-chain"

[[llm.providers.gonka_nodes]]
url = "https://node1.gonka.ai"
address = "gonka1w508d6qejxtdg4y5r3zarvary0c5xw7k2gsyg6"
name = "node1"

[[llm.providers.gonka_nodes]]
url = "https://node2.gonka.ai"
address = "gonka14h0ycu78h88wzldxc7e79vhw5xsde0n85evmum"
name = "node2"

[[llm.providers.gonka_nodes]]
url = "https://node3.gonka.ai"
address = "gonka1qyqszqgpqyqszqgpqyqszqgpqyqszqgpqyqszqg"
"#;
    let cfg = parse_llm(toml);
    assert_eq!(cfg.providers.len(), 1);
    let entry = &cfg.providers[0];
    assert_eq!(entry.provider_type, ProviderKind::Gonka);
    assert_eq!(entry.name.as_deref(), Some("my-gonka"));
    let nodes = &entry.gonka_nodes;
    assert_eq!(nodes.len(), 3);
    assert_eq!(nodes[0].url, "https://node1.gonka.ai");
    assert_eq!(
        nodes[0].address,
        "gonka1w508d6qejxtdg4y5r3zarvary0c5xw7k2gsyg6"
    );
    assert_eq!(nodes[0].name.as_deref(), Some("node1"));
    assert_eq!(nodes[2].name, None);
    assert_eq!(entry.gonka_chain_prefix.as_deref(), Some("custom-chain"));
}

#[test]
fn gonka_default_chain_prefix() {
    let entry = gonka_entry_with_nodes(valid_gonka_nodes());
    assert_eq!(entry.effective_gonka_chain_prefix(), "gonka");
}

#[test]
fn gonka_explicit_chain_prefix() {
    let entry = ProviderEntry {
        provider_type: ProviderKind::Gonka,
        name: Some("my-gonka".into()),
        gonka_nodes: valid_gonka_nodes(),
        gonka_chain_prefix: Some("my-chain".into()),
        ..Default::default()
    };
    assert_eq!(entry.effective_gonka_chain_prefix(), "my-chain");
}

#[test]
fn effective_model_gonka_is_empty() {
    let entry = ProviderEntry {
        provider_type: ProviderKind::Gonka,
        model: None,
        ..Default::default()
    };
    assert_eq!(entry.effective_model(), "");
}

#[test]
fn existing_configs_still_parse() {
    let toml = r#"
[llm]

[[llm.providers]]
type = "ollama"
model = "qwen3:8b"

[[llm.providers]]
type = "claude"
name = "claude"
model = "claude-sonnet-5"
"#;
    let cfg = parse_llm(toml);
    assert_eq!(cfg.providers.len(), 2);
    assert_eq!(cfg.providers[0].provider_type, ProviderKind::Ollama);
    assert_eq!(cfg.providers[1].provider_type, ProviderKind::Claude);
}

// ── ProviderEntry::validate — Cocoon URL and model validation ─────────────

fn cocoon_entry(url: Option<&str>, model: Option<&str>) -> ProviderEntry {
    ProviderEntry {
        provider_type: ProviderKind::Cocoon,
        name: Some("cocoon".into()),
        cocoon_client_url: url.map(str::to_owned),
        model: model.map(str::to_owned),
        ..Default::default()
    }
}

#[test]
fn test_cocoon_url_validation_accepts_http() {
    assert!(
        cocoon_entry(Some("http://localhost:10000"), Some("Qwen/Qwen3-0.6B"))
            .validate()
            .is_ok()
    );
}

#[test]
fn test_cocoon_url_validation_accepts_https_localhost() {
    assert!(
        cocoon_entry(Some("https://localhost:10000"), Some("Qwen/Qwen3-0.6B"))
            .validate()
            .is_ok()
    );
}

#[test]
fn test_cocoon_url_validation_rejects_non_localhost() {
    let err = cocoon_entry(Some("http://192.168.1.10:10000"), Some("Qwen/Qwen3-0.6B"))
        .validate()
        .unwrap_err();
    assert!(
        err.to_string().contains("localhost"),
        "error should mention localhost restriction: {err}"
    );
}

#[test]
fn test_cocoon_url_validation_rejects_non_http_scheme() {
    let err = cocoon_entry(Some("ftp://localhost"), Some("Qwen/Qwen3-0.6B"))
        .validate()
        .unwrap_err();
    assert!(
        err.to_string().contains("ftp"),
        "error should mention the bad scheme: {err}"
    );
}

#[test]
fn test_cocoon_url_validation_rejects_invalid_url() {
    let err = cocoon_entry(Some("not-a-url"), Some("Qwen/Qwen3-0.6B"))
        .validate()
        .unwrap_err();
    assert!(
        err.to_string().contains("not-a-url"),
        "error should mention the bad value: {err}"
    );
}

#[test]
fn test_cocoon_url_none_passes() {
    assert!(
        cocoon_entry(None, Some("Qwen/Qwen3-0.6B"))
            .validate()
            .is_ok()
    );
}

#[test]
fn test_cocoon_model_empty_rejected() {
    let err = cocoon_entry(Some("http://localhost:10000"), Some(""))
        .validate()
        .unwrap_err();
    assert!(
        err.to_string().contains("empty"),
        "error should mention 'empty': {err}"
    );
}

#[test]
fn test_cocoon_model_none_passes() {
    assert!(
        cocoon_entry(Some("http://localhost:10000"), None)
            .validate()
            .is_ok()
    );
}

#[test]
fn validate_cocoon_pricing_negative_prompt_errors() {
    let mut e = cocoon_entry(Some("http://localhost:10000"), Some("Qwen/Qwen3-0.6B"));
    e.cocoon_pricing = Some(CocoonPricing {
        prompt_cents_per_1k: -1.0,
        completion_cents_per_1k: 0.03,
    });
    assert!(e.validate().is_err());
}

#[test]
fn validate_cocoon_pricing_negative_completion_errors() {
    let mut e = cocoon_entry(Some("http://localhost:10000"), Some("Qwen/Qwen3-0.6B"));
    e.cocoon_pricing = Some(CocoonPricing {
        prompt_cents_per_1k: 0.01,
        completion_cents_per_1k: -0.5,
    });
    assert!(e.validate().is_err());
}

#[test]
fn validate_cocoon_pricing_valid_passes() {
    let mut e = cocoon_entry(Some("http://localhost:10000"), Some("Qwen/Qwen3-0.6B"));
    e.cocoon_pricing = Some(CocoonPricing {
        prompt_cents_per_1k: 0.01,
        completion_cents_per_1k: 0.03,
    });
    assert!(e.validate().is_ok());
}

/// Locks in the `f.pad` fix (#6066): `f.write_str` ignores width/fill/align flags, so
/// width-specifier `format!` calls used to render unpadded text. `f.pad` must reproduce
/// the same padding a plain `&str` would get under an identical width specifier.
#[test]
fn provider_kind_display_respects_width() {
    assert_eq!(
        format!("{:<12}", ProviderKind::Claude),
        format!("{:<12}", "claude")
    );
    assert_eq!(
        format!("{:>12}", ProviderKind::Compatible),
        format!("{:>12}", "compatible")
    );
}
