// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

// std::env::set_var / remove_var are unsafe in Rust 2024 edition; all callers are #[serial].
#![allow(unsafe_code)]
#![allow(clippy::default_trait_access)]

use std::path::{Path, PathBuf};

use super::*;
use crate::bootstrap::mcp::create_mcp_manager;
use zeph_core::config::{Config, ProviderEntry, ProviderKind};
use zeph_llm::claude::ClaudeProvider;
use zeph_llm::ollama::OllamaProvider;

// ── is_qdrant_localhost ───────────────────────────────────────────────────────

#[test]
fn localhost_variants_detected() {
    assert!(is_qdrant_localhost("http://127.0.0.1:6333"));
    assert!(is_qdrant_localhost("http://localhost:6333"));
    assert!(is_qdrant_localhost("http://[::1]:6334"));
    assert!(is_qdrant_localhost("http://0.0.0.0:6333"));
    assert!(is_qdrant_localhost("http://host.docker.internal:6333"));
}

#[test]
fn remote_url_not_localhost() {
    assert!(!is_qdrant_localhost("https://qdrant.example.com"));
    assert!(!is_qdrant_localhost("http://10.0.0.5:6333"));
}

// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn vault_args_defaults_in_test_context() {
    // #5953: an omitted `[vault] backend` must resolve to the age vault, not env.
    let config = Config::load(Path::new("/nonexistent")).unwrap();
    assert_eq!(config.vault.backend, zeph_config::VaultBackend::Age);
    let args = parse_vault_args(&config, None, None, None).unwrap();
    assert_eq!(args.backend, zeph_config::VaultBackend::Age);
    assert!(args.key_path.is_some());
    assert!(args.vault_path.is_some());
}

#[test]
fn vault_args_uses_config_backend_as_fallback() {
    let mut config = Config::load(Path::new("/nonexistent")).unwrap();
    config.vault.backend = zeph_config::VaultBackend::Env;
    let args = parse_vault_args(&config, None, None, None).unwrap();
    assert_eq!(args.backend, zeph_config::VaultBackend::Env);
}

#[test]
#[serial_test::serial]
fn vault_args_env_overrides_config() {
    let mut config = Config::load(Path::new("/nonexistent")).unwrap();
    config.vault.backend = zeph_config::VaultBackend::Env;
    unsafe { std::env::set_var("ZEPH_VAULT_BACKEND", "age") };
    let args = parse_vault_args(&config, None, None, None).unwrap();
    unsafe { std::env::remove_var("ZEPH_VAULT_BACKEND") };
    assert_eq!(args.backend, zeph_config::VaultBackend::Age);
}

#[test]
#[serial_test::serial]
fn vault_args_unknown_cli_backend_errors() {
    // #5954: an unrecognized `--vault` value must fail loudly, never fall back to `env`.
    let config = Config::load(Path::new("/nonexistent")).unwrap();
    let result = parse_vault_args(&config, Some("aeg"), None, None);
    let err = result.expect_err("typo'd backend name must be rejected, not silently downgraded");
    assert!(
        err.contains("aeg"),
        "error must name the invalid input: {err}"
    );
}

#[test]
#[serial_test::serial]
fn vault_args_unknown_env_backend_errors() {
    let config = Config::load(Path::new("/nonexistent")).unwrap();
    unsafe { std::env::set_var("ZEPH_VAULT_BACKEND", "totally-bogus") };
    let result = parse_vault_args(&config, None, None, None);
    unsafe { std::env::remove_var("ZEPH_VAULT_BACKEND") };
    assert!(
        result.is_err(),
        "unrecognized ZEPH_VAULT_BACKEND must be rejected, not silently downgraded to env"
    );
}

#[test]
fn vault_args_struct_construction() {
    let args = VaultArgs {
        backend: zeph_config::VaultBackend::Age,
        key_path: Some("/tmp/key".into()),
        vault_path: Some("/tmp/vault".into()),
    };
    assert_eq!(args.backend, zeph_config::VaultBackend::Age);
    assert_eq!(args.key_path.as_deref(), Some("/tmp/key"));
    assert_eq!(args.vault_path.as_deref(), Some("/tmp/vault"));
}

#[test]
#[serial_test::serial]
fn vault_args_cli_overrides_env_and_config() {
    let mut config = Config::load(Path::new("/nonexistent")).unwrap();
    config.vault.backend = zeph_config::VaultBackend::Env;
    unsafe { std::env::set_var("ZEPH_VAULT_BACKEND", "env") };
    let args = parse_vault_args(
        &config,
        Some("age"),
        Some(Path::new("/cli/key")),
        Some(Path::new("/cli/vault")),
    )
    .unwrap();
    unsafe { std::env::remove_var("ZEPH_VAULT_BACKEND") };
    assert_eq!(args.backend, zeph_config::VaultBackend::Age);
    assert_eq!(args.key_path.as_deref(), Some("/cli/key"));
    assert_eq!(args.vault_path.as_deref(), Some("/cli/vault"));
}

#[test]
#[serial_test::serial]
fn vault_args_env_key_and_path_fallback() {
    let config = Config::load(Path::new("/nonexistent")).unwrap();
    unsafe { std::env::set_var("ZEPH_VAULT_KEY", "/env/key") };
    unsafe { std::env::set_var("ZEPH_VAULT_PATH", "/env/vault") };
    let args = parse_vault_args(&config, None, None, None).unwrap();
    unsafe { std::env::remove_var("ZEPH_VAULT_KEY") };
    unsafe { std::env::remove_var("ZEPH_VAULT_PATH") };
    assert_eq!(args.key_path.as_deref(), Some("/env/key"));
    assert_eq!(args.vault_path.as_deref(), Some("/env/vault"));
}

#[test]
fn resolve_config_path_cli_override() {
    let path = resolve_config_path(Some(Path::new("/custom/config.toml")));
    assert_eq!(path, PathBuf::from("/custom/config.toml"));
}

#[test]
fn resolve_config_path_default() {
    let path = resolve_config_path(None);
    // Without ZEPH_CONFIG env, falls back to default
    if std::env::var("ZEPH_CONFIG").is_err() {
        assert_eq!(path, PathBuf::from("config/default.toml"));
    }
}

#[test]
fn vault_args_struct_env_backend() {
    let args = VaultArgs {
        backend: zeph_config::VaultBackend::Env,
        key_path: None,
        vault_path: None,
    };
    assert_eq!(args.backend, zeph_config::VaultBackend::Env);
    assert!(args.key_path.is_none());
    assert!(args.vault_path.is_none());
}

#[test]
fn create_provider_ollama() {
    let config = Config::load(Path::new("/nonexistent")).unwrap();
    let provider = create_provider(&config).unwrap();
    assert!(matches!(provider, AnyProvider::Ollama(_)));
    assert_eq!(provider.name(), "ollama");
}

#[test]
fn create_provider_claude_without_api_key_errors() {
    let mut config = Config::load(Path::new("/nonexistent")).unwrap();
    config.llm.providers = vec![ProviderEntry {
        provider_type: ProviderKind::Claude,
        model: Some("claude-sonnet-5".into()),
        max_tokens: Some(4096),
        ..ProviderEntry::default()
    }];
    config.secrets.claude_api_key = None;

    let result = create_provider(&config);
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("ZEPH_CLAUDE_API_KEY not found")
    );
}

#[tokio::test]
async fn health_check_ollama_unreachable() {
    let provider = AnyProvider::Ollama(OllamaProvider::new(
        "http://127.0.0.1:1",
        "test".into(),
        "embed".into(),
    ));
    health_check(&provider).await;
}

/// #6377: `build_provider` is the actual wiring point that probes `/api/show` and calls
/// `set_vision_capable` — the per-field unit tests on `OllamaProvider::supports_vision()`
/// and `ModelInfo::supports_vision()` never exercise this glue, only the pieces it calls.
#[tokio::test]
async fn build_provider_ollama_sets_vision_capable_from_api_show() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    use zeph_llm::provider::LlmProvider as _;

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/show"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "capabilities": ["completion", "vision"],
        })))
        .mount(&server)
        .await;

    let mut config = Config::load(Path::new("/nonexistent")).unwrap();
    config.llm.providers = vec![ProviderEntry {
        provider_type: ProviderKind::Ollama,
        base_url: Some(server.uri()),
        model: Some("qwen2.5vl".into()),
        ..ProviderEntry::default()
    }];
    let builder = AppBuilder {
        config,
        config_path: PathBuf::from("/nonexistent/config.toml"),
        vault: Box::new(EnvVaultProvider),
        age_vault: None,
        qdrant_ops: None,
        resolved_overlay: zeph_plugins::ResolvedOverlay::default(),
        secret_registry: None,
    };

    let (provider, _tx, _rx) = builder
        .build_provider()
        .await
        .expect("build_provider must succeed against the mock server");
    assert!(
        provider.supports_vision(),
        "a model whose /api/show capabilities include \"vision\" must be wired through to \
         supports_vision() == true"
    );
}

/// #6377 fail-safe path: when `/api/show` cannot be reached at all, `vision_capable` must
/// stay at its safe default (`false`) rather than the request failure leaving the field
/// unset in some ambiguous state.
#[tokio::test]
async fn build_provider_ollama_vision_capable_false_when_api_show_unreachable() {
    use zeph_llm::provider::LlmProvider as _;

    let mut config = Config::load(Path::new("/nonexistent")).unwrap();
    config.llm.providers = vec![ProviderEntry {
        provider_type: ProviderKind::Ollama,
        base_url: Some("http://127.0.0.1:1".into()),
        model: Some("qwen3:8b".into()),
        ..ProviderEntry::default()
    }];
    let builder = AppBuilder {
        config,
        config_path: PathBuf::from("/nonexistent/config.toml"),
        vault: Box::new(EnvVaultProvider),
        age_vault: None,
        qdrant_ops: None,
        resolved_overlay: zeph_plugins::ResolvedOverlay::default(),
        secret_registry: None,
    };

    let (provider, _tx, _rx) = builder
        .build_provider()
        .await
        .expect("build_provider must not fail just because /api/show is unreachable");
    assert!(
        !provider.supports_vision(),
        "an unreachable /api/show must leave vision_capable at its safe default (false)"
    );
}

#[tokio::test]
async fn health_check_claude_noop() {
    let provider = AnyProvider::Claude(ClaudeProvider::new("key".into(), "model".into(), 1024));
    health_check(&provider).await;
}

#[test]
fn effective_embedding_model_defaults_to_llm() {
    let config = Config::load(Path::new("/nonexistent")).unwrap();
    assert_eq!(config.llm.effective_embedding_model(), "qwen3-embedding");
}

#[test]
fn effective_embedding_model_uses_pool_embed_entry() {
    let mut config = Config::load(Path::new("/nonexistent")).unwrap();
    config.llm.providers = vec![ProviderEntry {
        provider_type: ProviderKind::OpenAi,
        model: Some("gpt-5.2".into()),
        max_tokens: Some(4096),
        embedding_model: Some("text-embedding-3-small".into()),
        embed: true,
        ..ProviderEntry::default()
    }];
    assert_eq!(
        config.llm.effective_embedding_model(),
        "text-embedding-3-small"
    );
}

#[test]
fn effective_embedding_model_falls_back_when_embed_missing() {
    let mut config = Config::load(Path::new("/nonexistent")).unwrap();
    config.llm.providers = vec![ProviderEntry {
        provider_type: ProviderKind::OpenAi,
        model: Some("gpt-5.2".into()),
        max_tokens: Some(4096),
        embedding_model: None,
        ..ProviderEntry::default()
    }];
    assert_eq!(config.llm.effective_embedding_model(), "qwen3-embedding");
}

#[test]
fn create_provider_openai_missing_api_key_errors() {
    let mut config = Config::load(Path::new("/nonexistent")).unwrap();
    config.llm.providers = vec![ProviderEntry {
        provider_type: ProviderKind::OpenAi,
        base_url: Some("https://api.openai.com/v1".into()),
        model: Some("gpt-4o".into()),
        max_tokens: Some(4096),
        embedding_model: None,
        ..ProviderEntry::default()
    }];
    config.secrets.openai_api_key = None;
    let result = create_provider(&config);
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("ZEPH_OPENAI_API_KEY not found")
    );
}

#[cfg(feature = "candle")]
use zeph_config::CandleDevice;
#[cfg(feature = "candle")]
use zeph_core::provider_factory::select_device;

#[cfg(feature = "candle")]
#[test]
fn select_device_cpu_default() {
    let device = select_device(CandleDevice::Cpu).unwrap();
    assert!(matches!(device, zeph_llm::candle_provider::Device::Cpu));
}

#[cfg(all(feature = "candle", not(feature = "metal")))]
#[test]
fn select_device_metal_without_feature_errors() {
    let result = select_device(CandleDevice::Metal);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("metal feature"));
}

#[cfg(all(feature = "candle", not(feature = "cuda")))]
#[test]
fn select_device_cuda_without_feature_errors() {
    let result = select_device(CandleDevice::Cuda);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("cuda feature"));
}

#[cfg(feature = "candle")]
#[test]
fn select_device_auto_fallback() {
    let device = select_device(CandleDevice::Auto).unwrap();
    assert!(matches!(
        device,
        zeph_llm::candle_provider::Device::Cpu
            | zeph_llm::candle_provider::Device::Cuda(_)
            | zeph_llm::candle_provider::Device::Metal(_)
    ));
}

#[cfg(feature = "candle")]
#[test]
fn create_provider_candle_without_config_errors() {
    let mut config = Config::load(Path::new("/nonexistent")).unwrap();
    config.llm.providers = vec![ProviderEntry {
        provider_type: ProviderKind::Candle,
        ..ProviderEntry::default()
    }];
    config.llm.candle = None;
    let result = create_provider(&config);
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("candle provider requires 'candle' section in [[llm.providers]]")
    );
}

#[cfg(feature = "candle")]
#[tokio::test]
async fn health_check_candle_logs_device() {
    use zeph_llm::candle_provider::CandleProvider;

    let source = zeph_llm::candle_provider::loader::ModelSource::HuggingFace {
        repo_id: "test/model".to_string(),
        filename: Some("model.gguf".to_string()),
        sha256: None,
    };
    let template = zeph_llm::candle_provider::template::ChatTemplate::parse_str(
        "{{ bos_token }}{{ messages[0].content }}",
    );
    let gen_config = zeph_llm::candle_provider::generate::GenerationConfig {
        temperature: 0.7,
        top_p: Some(0.9),
        top_k: Some(50),
        max_tokens: 512,
        seed: 42,
        repeat_penalty: 1.1,
        repeat_last_n: 64,
    };
    let device = zeph_llm::candle_provider::Device::Cpu;

    let candle_result = CandleProvider::new(
        &source,
        template,
        gen_config,
        Some("embed/model"),
        None,
        None,
        device,
    );

    if let Ok(candle) = candle_result {
        let provider = AnyProvider::Candle(candle);
        health_check(&provider).await;
    }
}

#[test]
fn create_mcp_manager_with_http_transport() {
    use std::collections::HashMap;

    let mut config = Config::load(Path::new("/nonexistent")).unwrap();
    config.mcp.servers = vec![zeph_core::config::McpServerConfig {
        id: "test".into(),
        url: Some("http://localhost:3000".into()),
        command: None,
        args: vec![],
        env: HashMap::new(),
        headers: HashMap::new(),
        oauth: None,
        timeout: 30,
        policy: Default::default(),
        trust_level: Default::default(),
        tool_allowlist: None,
        expected_tools: vec![],
        roots: vec![],
        tool_metadata: HashMap::new(),
        elicitation_enabled: None,
        env_isolation: None,
        media_passthrough: false,
    }];

    let manager = create_mcp_manager(&config, false);
    let debug = format!("{manager:?}");
    assert!(debug.contains("server_count: 1"));
}

#[test]
fn create_mcp_manager_with_stdio_transport() {
    use std::collections::HashMap;

    let mut config = Config::load(Path::new("/nonexistent")).unwrap();
    config.mcp.servers = vec![zeph_core::config::McpServerConfig {
        id: "test".into(),
        url: None,
        command: Some("node".into()),
        args: vec!["server.js".into()],
        env: HashMap::new(),
        headers: HashMap::new(),
        oauth: None,
        timeout: 30,
        policy: Default::default(),
        trust_level: Default::default(),
        tool_allowlist: None,
        expected_tools: vec![],
        roots: vec![],
        tool_metadata: HashMap::new(),
        elicitation_enabled: None,
        env_isolation: None,
        media_passthrough: false,
    }];

    let manager = create_mcp_manager(&config, false);
    let debug = format!("{manager:?}");
    assert!(debug.contains("server_count: 1"));
}

#[test]
fn create_mcp_manager_empty_servers() {
    let mut config = Config::load(Path::new("/nonexistent")).unwrap();
    config.mcp.servers = vec![];

    let manager = create_mcp_manager(&config, false);
    let debug = format!("{manager:?}");
    assert!(debug.contains("server_count: 0"));
}

#[tokio::test]
async fn create_mcp_registry_when_semantic_disabled() {
    let config_path = Path::new("/nonexistent");
    let mut config = Config::load(config_path).unwrap();
    config.memory.semantic.enabled = false;

    let provider = AnyProvider::Ollama(OllamaProvider::new(
        "http://localhost:11434",
        "test".into(),
        "embed".into(),
    ));

    let mcp_tools = vec![];
    let registry = create_mcp_registry(&config, &provider, &mcp_tools, "test-model", None).await;
    assert!(registry.is_none());
}

#[test]
fn managed_skills_dir_returns_skills_subdir() {
    let dir = managed_skills_dir();
    assert!(
        dir.ends_with("skills"),
        "managed_skills_dir should end in 'skills', got: {dir:?}"
    );
}

#[test]
fn app_builder_managed_skills_dir_matches_free_fn() {
    assert_eq!(AppBuilder::managed_skills_dir(), managed_skills_dir());
}

#[test]
fn skill_paths_includes_managed_dir() {
    let config = Config::load(Path::new("/nonexistent")).unwrap();
    let builder = AppBuilder {
        config,
        config_path: PathBuf::from("/nonexistent/config.toml"),
        vault: Box::new(EnvVaultProvider),
        age_vault: None,
        qdrant_ops: None,
        resolved_overlay: zeph_plugins::ResolvedOverlay::default(),
        secret_registry: None,
    };
    let paths = builder.skill_paths_for_registry();
    let managed = managed_skills_dir();
    assert!(
        paths.contains(&managed),
        "skill_paths_for_registry() should include managed_skills_dir, got: {paths:?}"
    );
}

#[test]
fn skill_paths_does_not_duplicate_managed_dir() {
    let managed = managed_skills_dir();
    let mut config = Config::load(Path::new("/nonexistent")).unwrap();
    config.skills.paths = vec![managed.to_string_lossy().into_owned()];
    let builder = AppBuilder {
        config,
        config_path: PathBuf::from("/nonexistent/config.toml"),
        vault: Box::new(EnvVaultProvider),
        age_vault: None,
        qdrant_ops: None,
        resolved_overlay: zeph_plugins::ResolvedOverlay::default(),
        secret_registry: None,
    };
    let paths = builder.skill_paths_for_registry();
    let count = paths.iter().filter(|p| p == &&managed).count();
    assert_eq!(
        count, 1,
        "managed dir should appear exactly once, got: {paths:?}"
    );
}

#[test]
fn skill_paths_for_watcher_includes_plugins_root() {
    let config = Config::load(Path::new("/nonexistent")).unwrap();
    let builder = AppBuilder {
        config,
        config_path: PathBuf::from("/nonexistent/config.toml"),
        vault: Box::new(EnvVaultProvider),
        age_vault: None,
        qdrant_ops: None,
        resolved_overlay: zeph_plugins::ResolvedOverlay::default(),
        secret_registry: None,
    };
    let paths = builder.skill_paths_for_watcher();
    let plugins_root = plugins_dir();
    // The helper must either include the plugins root (created eagerly) or skip it cleanly
    // when creation fails. On CI where HOME is writable this should always be present.
    if plugins_root.exists() {
        assert!(
            paths.contains(&plugins_root),
            "skill_paths_for_watcher() should include plugins_root when it exists, got: {paths:?}"
        );
    }
    // Watcher paths must NOT contain per-plugin subdirs — only the root.
    for p in &paths {
        assert!(
            p != &plugins_root.join("some_plugin").join("skills").join("x"),
            "skill_paths_for_watcher() must not expand per-plugin skill dirs"
        );
    }
}

#[tokio::test]
async fn create_skill_matcher_when_semantic_disabled() {
    let tmp_dir = tempfile::tempdir().expect("tempdir");
    let tmp_path = tmp_dir
        .path()
        .join("skill_matcher_bootstrap.db")
        .to_string_lossy()
        .to_string();

    let mut config = Config::load(Path::new("/nonexistent")).unwrap();
    config.memory.semantic.enabled = false;
    config.memory.sqlite_path = tmp_path.clone();

    let provider = AnyProvider::Ollama(OllamaProvider::new(
        "http://localhost:11434",
        "test".into(),
        "embed".into(),
    ));

    let memory = SemanticMemory::with_sqlite_backend_and_pool_size(
        &tmp_path,
        provider.clone(),
        &config.llm.embedding_model,
        config.memory.semantic.vector_weight,
        config.memory.semantic.keyword_weight,
        1,
    )
    .await
    .unwrap();

    let meta: Vec<&SkillMeta> = vec![];
    let result = create_skill_matcher(&config, &provider, &meta, &memory, "test-model", None).await;
    assert!(result.is_none());
}

/// Regression test for #5920: `seed_skill_trust_db` — extracted from `runner.rs`'s previously
/// CLI-only inline block so `daemon.rs`/`acp.rs`/`serve/*` seed trust rows at construction time
/// too.
///
/// Covers both directions of the security-relevant classification:
/// - **Permissive**: a locally-sourced skill lands at `config.skills.trust.local_level`
///   (`Trusted`), not silently left at
///   `zeph_common::SkillTrustLevel::MISSING_ENTRY_FALLBACK` (also `Trusted` — the missing-row
///   fallback is fail-*open*, not fail-closed; see the constant's own doc comment). This
///   direction alone proves config is read but has no security value on its own.
/// - **Restrictive**: after the skill's `SKILL.md` content changes (hash mismatch against the
///   already-seeded row), a second seeding pass demotes it to
///   `config.skills.trust.hash_mismatch_level` (`Quarantined`) — this is the direction that
///   actually matters for #5920: an operator's restrictive classification must land in the DB,
///   not be skipped because daemon/ACP/serve never called this seeding step at all.
#[tokio::test]
async fn seed_skill_trust_db_applies_configured_local_level() {
    let tmp_dir = tempfile::tempdir().expect("tempdir");
    let skill_dir = tmp_dir.path().join("my-skill");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(skill_dir.join("SKILL.md"), "# My Skill\n").unwrap();

    let mut config = Config::load(Path::new("/nonexistent")).unwrap();
    config.skills.trust.default_level = zeph_common::SkillTrustLevel::Quarantined;
    config.skills.trust.local_level = zeph_common::SkillTrustLevel::Trusted;
    config.skills.trust.hash_mismatch_level = zeph_common::SkillTrustLevel::Quarantined;

    let builder = AppBuilder {
        config,
        config_path: PathBuf::from("/nonexistent/config.toml"),
        vault: Box::new(EnvVaultProvider),
        age_vault: None,
        qdrant_ops: None,
        resolved_overlay: zeph_plugins::ResolvedOverlay::default(),
        secret_registry: None,
    };

    let provider = AnyProvider::Ollama(OllamaProvider::new(
        "http://localhost:11434",
        "test".into(),
        "embed".into(),
    ));
    let memory = SemanticMemory::new(
        ":memory:",
        "http://127.0.0.1:1",
        None,
        provider,
        "test-model",
    )
    .await
    .unwrap();

    let meta = SkillMeta {
        name: "my-skill".to_owned(),
        description: "test skill".to_owned(),
        skill_dir: skill_dir.clone(),
        ..Default::default()
    };

    builder
        .seed_skill_trust_db(std::slice::from_ref(&meta), &memory)
        .await;

    let row = memory
        .sqlite()
        .load_skill_trust("my-skill")
        .await
        .unwrap()
        .expect("trust row must be seeded");
    assert_eq!(
        row.trust_level,
        zeph_common::SkillTrustLevel::Trusted,
        "local skill must be classified per config.skills.trust.local_level — this is the \
         shared seeding logic now called by runner.rs/daemon.rs/acp.rs/serve/*"
    );

    // Restrictive direction (S2, the security-relevant one): change the skill's content so its
    // hash no longer matches the stored row, then re-seed. A stale seeding path (or one that
    // silently skips the hash-mismatch check) would leave this at Trusted — the fail-open
    // posture #5920 exists to close.
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "# My Skill\n\ntampered content\n",
    )
    .unwrap();
    builder.seed_skill_trust_db(&[meta], &memory).await;

    let row_after_tamper = memory
        .sqlite()
        .load_skill_trust("my-skill")
        .await
        .unwrap()
        .expect("trust row must still exist after re-seeding");
    assert_eq!(
        row_after_tamper.trust_level,
        zeph_common::SkillTrustLevel::Quarantined,
        "a skill whose content hash no longer matches the stored row must be demoted to \
         config.skills.trust.hash_mismatch_level, not left at its prior Trusted level — this \
         is the restrictive-direction check that actually proves the fail-open hole is closed"
    );
}

#[test]
fn appbuilder_qdrant_ops_invalid_url_returns_err() {
    let mut config = Config::load(Path::new("/nonexistent")).unwrap();
    config.memory.vector_backend = zeph_core::config::VectorBackend::Qdrant;
    config.memory.qdrant_url = "not a valid url".into();

    let result = zeph_memory::QdrantOps::new(&config.memory.qdrant_url, None);
    assert!(
        result.is_err(),
        "QdrantOps::new with invalid URL must fail (CRIT-04)"
    );
}

#[test]
fn appbuilder_qdrant_ops_valid_url_succeeds() {
    let result = zeph_memory::QdrantOps::new("http://localhost:6334", None);
    assert!(result.is_ok(), "QdrantOps::new with valid URL must succeed");
}

#[test]
fn appbuilder_qdrant_ops_applies_configured_timeout() {
    let mut config = Config::load(Path::new("/nonexistent")).unwrap();
    config.memory.vector_backend = zeph_core::config::VectorBackend::Qdrant;
    config.memory.qdrant_timeout_secs = 3;

    // `build_qdrant_ops` is the exact function `AppBuilder::new` calls to construct the
    // shared production `QdrantOps` client — not a re-implementation. Deleting the
    // `.with_timeout(...)` call from that function (or from `AppBuilder::new`, which would
    // require breaking this call) fails this test. `timeout` is private on `QdrantOps`, so
    // the configured value is observed via `Debug`.
    let ops = build_qdrant_ops(&config)
        .expect("valid default qdrant_url must build")
        .expect("vector_backend explicitly set to Qdrant");
    assert!(
        format!("{ops:?}").contains("3s"),
        "QdrantOps must be built with the configured qdrant_timeout_secs, not the default"
    );
}

#[test]
fn build_qdrant_ops_returns_none_for_sqlite_backend() {
    let mut config = Config::load(Path::new("/nonexistent")).unwrap();
    config.memory.vector_backend = zeph_core::config::VectorBackend::Sqlite;

    let ops = build_qdrant_ops(&config).unwrap();
    assert!(
        ops.is_none(),
        "build_qdrant_ops must return None when vector_backend is not Qdrant"
    );
}

// ── build_feedback_classifier ─────────────────────────────────────────────────
//
// Full integration tests require a live provider (Ollama/OpenAI). These tests only verify
// the early-return behavior when `detector_mode != Model` — no LLM call is made.

fn make_builder_with_detector_mode(mode: zeph_core::config::DetectorMode) -> AppBuilder {
    let mut config = Config::load(Path::new("/nonexistent")).unwrap();
    config.skills.learning.detector_mode = mode;
    AppBuilder {
        config,
        config_path: PathBuf::from("/nonexistent/config.toml"),
        vault: Box::new(EnvVaultProvider),
        age_vault: None,
        qdrant_ops: None,
        resolved_overlay: zeph_plugins::ResolvedOverlay::default(),
        secret_registry: None,
    }
}

fn make_mock_primary() -> zeph_llm::any::AnyProvider {
    zeph_llm::any::AnyProvider::Ollama(zeph_llm::ollama::OllamaProvider::new(
        "http://localhost:11434",
        "llama3".into(),
        String::new(),
    ))
}

#[test]
fn build_feedback_classifier_regex_mode_returns_none() {
    let b = make_builder_with_detector_mode(zeph_core::config::DetectorMode::Regex);
    let primary = make_mock_primary();
    let result = b.build_feedback_classifier(&primary);
    assert!(
        result.is_none(),
        "Regex mode must not build feedback classifier"
    );
}

#[test]
fn build_feedback_classifier_judge_mode_returns_none() {
    let b = make_builder_with_detector_mode(zeph_core::config::DetectorMode::Judge);
    let primary = make_mock_primary();
    let result = b.build_feedback_classifier(&primary);
    assert!(
        result.is_none(),
        "Judge mode must not build feedback classifier"
    );
}

// ── create_embedding_provider ─────────────────────────────────────────────────

#[test]
fn create_embedding_provider_prefers_embed_flag() {
    let mut config = Config::load(Path::new("/nonexistent")).unwrap();
    // Two providers: first is Ollama (primary), second is OpenAI with embed=true.
    config.llm.providers = vec![
        ProviderEntry {
            provider_type: ProviderKind::Ollama,
            model: Some("qwen3:8b".into()),
            embedding_model: Some("nomic-embed-text".into()),
            embed: false,
            ..ProviderEntry::default()
        },
        ProviderEntry {
            provider_type: ProviderKind::Ollama,
            name: Some("embed".into()),
            model: Some("qwen3:8b".into()),
            embedding_model: Some("qwen3-embedding".into()),
            embed: true,
            ..ProviderEntry::default()
        },
    ];
    let primary = AnyProvider::Ollama(OllamaProvider::new(
        "http://localhost:11434",
        "qwen3:8b".into(),
        "nomic-embed-text".into(),
    ));
    let embed_provider = create_embedding_provider(&config, &primary);
    // Should resolve to an Ollama provider (the embed=true entry).
    assert!(matches!(embed_provider, AnyProvider::Ollama(_)));
}

#[test]
fn create_embedding_provider_falls_back_to_embedding_model_entry() {
    let mut config = Config::load(Path::new("/nonexistent")).unwrap();
    // Only one provider with embedding_model set but embed=false.
    config.llm.providers = vec![ProviderEntry {
        provider_type: ProviderKind::Ollama,
        name: Some("main".into()),
        model: Some("qwen3:8b".into()),
        embedding_model: Some("nomic-embed-text".into()),
        embed: false,
        ..ProviderEntry::default()
    }];
    let primary = AnyProvider::Ollama(OllamaProvider::new(
        "http://localhost:11434",
        "qwen3:8b".into(),
        "nomic-embed-text".into(),
    ));
    let embed_provider = create_embedding_provider(&config, &primary);
    assert!(matches!(embed_provider, AnyProvider::Ollama(_)));
}

#[test]
fn create_embedding_provider_falls_back_to_primary_when_no_embedding_entry() {
    let mut config = Config::load(Path::new("/nonexistent")).unwrap();
    // Provider with no embedding_model and embed=false.
    config.llm.providers = vec![ProviderEntry {
        provider_type: ProviderKind::Ollama,
        model: Some("qwen3:8b".into()),
        embedding_model: None,
        embed: false,
        ..ProviderEntry::default()
    }];
    let primary = AnyProvider::Ollama(OllamaProvider::new(
        "http://localhost:11434",
        "qwen3:8b".into(),
        "nomic-embed-text".into(),
    ));
    let embed_provider = create_embedding_provider(&config, &primary);
    // Falls back to primary clone — must still be Ollama.
    assert!(matches!(embed_provider, AnyProvider::Ollama(_)));
    assert_eq!(embed_provider.name(), primary.name());
}

// ── auto_budget_tokens fallback (#2773) ──────────────────────────────────────

#[test]
fn auto_budget_tokens_returns_configured_value() {
    let mut config = Config::load(Path::new("/nonexistent")).unwrap();
    config.memory.auto_budget = false;
    config.memory.context_budget_tokens = 65536;
    let builder = super::AppBuilder::for_test(config);
    let provider = AnyProvider::Mock(zeph_llm::mock::MockProvider::default());
    assert_eq!(builder.auto_budget_tokens(&provider), 65536);
}

#[test]
fn auto_budget_tokens_auto_budget_no_window_falls_back_to_128k() {
    let mut config = Config::load(Path::new("/nonexistent")).unwrap();
    config.memory.auto_budget = true;
    config.memory.context_budget_tokens = 0;
    let builder = super::AppBuilder::for_test(config);
    // MockProvider::context_window() returns None — triggers fallback
    let provider = AnyProvider::Mock(zeph_llm::mock::MockProvider::default());
    assert_eq!(builder.auto_budget_tokens(&provider), 128_000);
}

#[test]
fn auto_budget_tokens_explicit_zero_falls_back_to_128k() {
    let mut config = Config::load(Path::new("/nonexistent")).unwrap();
    config.memory.auto_budget = false;
    config.memory.context_budget_tokens = 0;
    let builder = super::AppBuilder::for_test(config);
    let provider = AnyProvider::Mock(zeph_llm::mock::MockProvider::default());
    assert_eq!(builder.auto_budget_tokens(&provider), 128_000);
}

#[test]
fn auto_budget_tokens_provider_window_zero_falls_back_to_128k() {
    let mut config = Config::load(Path::new("/nonexistent")).unwrap();
    config.memory.auto_budget = true;
    config.memory.context_budget_tokens = 0;
    let builder = super::AppBuilder::for_test(config);
    // Provider reports Some(0) — a misconfigured window, not "unknown" (None).
    // Must still fall back to 128k rather than resolving to a real 0-token budget.
    let provider =
        AnyProvider::Mock(zeph_llm::mock::MockProvider::default().with_context_window(0));
    assert_eq!(builder.auto_budget_tokens(&provider), 128_000);
}

// ── build_judge_provider ──────────────────────────────────────────────────────

fn make_builder_with_judge_config(
    judge_provider: &str,
    judge_model: &str,
    providers: Vec<ProviderEntry>,
) -> super::AppBuilder {
    let mut config = zeph_core::config::Config::load(Path::new("/nonexistent")).unwrap();
    config.skills.learning.detector_mode = zeph_core::config::DetectorMode::Judge;
    config.skills.learning.judge_provider = judge_provider.to_owned();
    config.skills.learning.judge_model = judge_model.to_owned();
    config.llm.providers = providers;
    super::AppBuilder::for_test(config)
}

fn ollama_entry(name: &str) -> ProviderEntry {
    ProviderEntry {
        provider_type: ProviderKind::Ollama,
        name: Some(name.to_owned()),
        model: Some("llama3".into()),
        ..ProviderEntry::default()
    }
}

#[test]
fn build_judge_provider_regex_mode_returns_none() {
    let mut config = zeph_core::config::Config::load(Path::new("/nonexistent")).unwrap();
    config.skills.learning.detector_mode = zeph_core::config::DetectorMode::Regex;
    let b = super::AppBuilder::for_test(config);
    assert!(
        b.build_judge_provider().is_none(),
        "Regex mode must return None"
    );
}

#[test]
fn build_judge_provider_valid_judge_provider_returns_some() {
    let b = make_builder_with_judge_config("judge", "", vec![ollama_entry("judge")]);
    assert!(
        b.build_judge_provider().is_some(),
        "Valid judge_provider entry must resolve to Some"
    );
}

/// Regression test for #4761: when `judge_provider` is set but the named provider does not exist,
/// the function must fall through to `judge_model` instead of returning `None` early.
#[test]
fn build_judge_provider_invalid_judge_provider_falls_back_to_judge_model() {
    let b = make_builder_with_judge_config(
        "nonexistent-provider",
        "fallback",
        vec![ollama_entry("fallback")],
    );
    assert!(
        b.build_judge_provider().is_some(),
        "Failed judge_provider lookup must fall back to judge_model"
    );
}

#[test]
fn build_judge_provider_both_empty_returns_none() {
    let b = make_builder_with_judge_config("", "", vec![]);
    assert!(
        b.build_judge_provider().is_none(),
        "Empty judge_provider and judge_model must return None"
    );
}

// ── build_ensemble_members (spec 073-orch-ensemble-merge, T3.4) ──────────────

fn make_builder_with_ensemble_config(
    enabled: bool,
    members: Vec<&str>,
    providers: Vec<ProviderEntry>,
) -> super::AppBuilder {
    let mut config = zeph_core::config::Config::load(Path::new("/nonexistent")).unwrap();
    config.orchestration.ensemble.enabled = enabled;
    config.orchestration.ensemble.members = members.into_iter().map(String::from).collect();
    config.llm.providers = providers;
    super::AppBuilder::for_test(config)
}

#[test]
fn build_ensemble_members_disabled_returns_empty_vec() {
    let b = make_builder_with_ensemble_config(
        false,
        vec!["a", "b", "c"],
        vec![ollama_entry("a"), ollama_entry("b"), ollama_entry("c")],
    );
    assert!(
        b.build_ensemble_members().is_empty(),
        "enabled=false must resolve zero members regardless of the configured list"
    );
}

#[test]
fn build_ensemble_members_all_valid_names_resolves_full_vec() {
    let b = make_builder_with_ensemble_config(
        true,
        vec!["a", "b", "c"],
        vec![ollama_entry("a"), ollama_entry("b"), ollama_entry("c")],
    );
    let members = b.build_ensemble_members();
    assert_eq!(members.len(), 3);
    let names: Vec<&str> = members.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, vec!["a", "b", "c"]);
}

#[test]
#[tracing_test::traced_test]
fn build_ensemble_members_one_unresolvable_name_shrinks_vec_and_warns() {
    let b = make_builder_with_ensemble_config(
        true,
        vec!["a", "missing", "c"],
        vec![ollama_entry("a"), ollama_entry("c")],
    );
    let members = b.build_ensemble_members();
    assert_eq!(
        members.len(),
        2,
        "the unresolvable member must be excluded, not substituted"
    );
    let names: Vec<&str> = members.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, vec!["a", "c"]);
    assert!(
        logs_contain("ensemble member resolution failed"),
        "a per-member resolution failure must log a warning"
    );
    assert!(
        logs_contain("ensemble member resolution shrank the effective member count"),
        "shrinkage below the configured count must log a bootstrap-level warning (critic S1)"
    );
}
