// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::path::{Path, PathBuf};

use super::*;
use crate::config::{Config, ProviderKind};
use zeph_llm::claude::ClaudeProvider;
use zeph_llm::ollama::OllamaProvider;

#[test]
fn vault_args_defaults_in_test_context() {
    let config = Config::load(Path::new("/nonexistent")).unwrap();
    let args = parse_vault_args(&config, None, None, None);
    assert_eq!(args.backend, "env");
    assert!(args.key_path.is_none());
    assert!(args.vault_path.is_none());
}

#[test]
fn vault_args_uses_config_backend_as_fallback() {
    let mut config = Config::load(Path::new("/nonexistent")).unwrap();
    config.vault.backend = "age".into();
    let args = parse_vault_args(&config, None, None, None);
    assert_eq!(args.backend, "age");
}

#[test]
fn vault_args_env_overrides_config() {
    let mut config = Config::load(Path::new("/nonexistent")).unwrap();
    config.vault.backend = "age".into();
    unsafe { std::env::set_var("ZEPH_VAULT_BACKEND", "env") };
    let args = parse_vault_args(&config, None, None, None);
    unsafe { std::env::remove_var("ZEPH_VAULT_BACKEND") };
    assert_eq!(args.backend, "env");
}

#[test]
fn vault_args_struct_construction() {
    let args = VaultArgs {
        backend: "age".into(),
        key_path: Some("/tmp/key".into()),
        vault_path: Some("/tmp/vault".into()),
    };
    assert_eq!(args.backend, "age");
    assert_eq!(args.key_path.as_deref(), Some("/tmp/key"));
    assert_eq!(args.vault_path.as_deref(), Some("/tmp/vault"));
}

#[test]
fn vault_args_cli_overrides_env_and_config() {
    let mut config = Config::load(Path::new("/nonexistent")).unwrap();
    config.vault.backend = "env".into();
    unsafe { std::env::set_var("ZEPH_VAULT_BACKEND", "env") };
    let args = parse_vault_args(
        &config,
        Some("age"),
        Some(Path::new("/cli/key")),
        Some(Path::new("/cli/vault")),
    );
    unsafe { std::env::remove_var("ZEPH_VAULT_BACKEND") };
    assert_eq!(args.backend, "age");
    assert_eq!(args.key_path.as_deref(), Some("/cli/key"));
    assert_eq!(args.vault_path.as_deref(), Some("/cli/vault"));
}

#[test]
fn vault_args_env_key_and_path_fallback() {
    let config = Config::load(Path::new("/nonexistent")).unwrap();
    unsafe { std::env::set_var("ZEPH_VAULT_KEY", "/env/key") };
    unsafe { std::env::set_var("ZEPH_VAULT_PATH", "/env/vault") };
    let args = parse_vault_args(&config, None, None, None);
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
        backend: "env".into(),
        key_path: None,
        vault_path: None,
    };
    assert_eq!(args.backend, "env");
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
fn create_provider_claude_without_cloud_config_errors() {
    let mut config = Config::load(Path::new("/nonexistent")).unwrap();
    config.llm.provider = ProviderKind::Claude;
    config.llm.cloud = None;
    let result = create_provider(&config);
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("llm.cloud config section required")
    );
}

#[test]
fn create_provider_claude_without_api_key_errors() {
    let mut config = Config::load(Path::new("/nonexistent")).unwrap();
    config.llm.provider = ProviderKind::Claude;
    config.llm.cloud = Some(crate::config::CloudLlmConfig {
        model: "claude-3-opus".into(),
        max_tokens: 4096,
    });
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

#[tokio::test]
async fn health_check_claude_noop() {
    let provider = AnyProvider::Claude(ClaudeProvider::new("key".into(), "model".into(), 1024));
    health_check(&provider).await;
}

#[test]
fn effective_embedding_model_defaults_to_llm() {
    let config = Config::load(Path::new("/nonexistent")).unwrap();
    assert_eq!(effective_embedding_model(&config), "qwen3-embedding");
}

#[test]
fn effective_embedding_model_uses_openai_when_set() {
    let mut config = Config::load(Path::new("/nonexistent")).unwrap();
    config.llm.provider = ProviderKind::OpenAi;
    config.llm.openai = Some(crate::config::OpenAiConfig {
        base_url: "https://api.openai.com/v1".into(),
        model: "gpt-5.2".into(),
        max_tokens: 4096,
        embedding_model: Some("text-embedding-3-small".into()),
        reasoning_effort: None,
    });
    assert_eq!(effective_embedding_model(&config), "text-embedding-3-small");
}

#[test]
fn effective_embedding_model_falls_back_when_openai_embed_missing() {
    let mut config = Config::load(Path::new("/nonexistent")).unwrap();
    config.llm.provider = ProviderKind::OpenAi;
    config.llm.openai = Some(crate::config::OpenAiConfig {
        base_url: "https://api.openai.com/v1".into(),
        model: "gpt-5.2".into(),
        max_tokens: 4096,
        embedding_model: None,
        reasoning_effort: None,
    });
    assert_eq!(effective_embedding_model(&config), "qwen3-embedding");
}

#[test]
fn create_provider_openai_missing_config_errors() {
    let mut config = Config::load(Path::new("/nonexistent")).unwrap();
    config.llm.provider = ProviderKind::OpenAi;
    config.llm.openai = None;
    let result = create_provider(&config);
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("llm.openai config section required")
    );
}

#[test]
fn create_provider_openai_missing_api_key_errors() {
    let mut config = Config::load(Path::new("/nonexistent")).unwrap();
    config.llm.provider = ProviderKind::OpenAi;
    config.llm.openai = Some(crate::config::OpenAiConfig {
        base_url: "https://api.openai.com/v1".into(),
        model: "gpt-4o".into(),
        max_tokens: 4096,
        embedding_model: None,
        reasoning_effort: None,
    });
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
#[test]
fn select_device_cpu_default() {
    let device = select_device("cpu").unwrap();
    assert!(matches!(device, zeph_llm::candle_provider::Device::Cpu));
}

#[cfg(feature = "candle")]
#[test]
fn select_device_unknown_defaults_to_cpu() {
    let device = select_device("unknown").unwrap();
    assert!(matches!(device, zeph_llm::candle_provider::Device::Cpu));
}

#[cfg(all(feature = "candle", not(feature = "metal")))]
#[test]
fn select_device_metal_without_feature_errors() {
    let result = select_device("metal");
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("metal feature"));
}

#[cfg(all(feature = "candle", not(feature = "cuda")))]
#[test]
fn select_device_cuda_without_feature_errors() {
    let result = select_device("cuda");
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("cuda feature"));
}

#[cfg(feature = "candle")]
#[test]
fn select_device_auto_fallback() {
    let device = select_device("auto").unwrap();
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
    config.llm.provider = ProviderKind::Candle;
    config.llm.candle = None;
    let result = create_provider(&config);
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("llm.candle config section required")
    );
}

#[test]
fn create_provider_orchestrator_without_config_errors() {
    let mut config = Config::load(Path::new("/nonexistent")).unwrap();
    config.llm.provider = ProviderKind::Orchestrator;
    config.llm.orchestrator = None;
    let result = create_provider(&config);
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("llm.orchestrator config section required")
    );
}

#[test]
fn build_orchestrator_with_unknown_provider_errors() {
    use crate::config::OrchestratorProviderConfig;
    use std::collections::HashMap;

    let mut config = Config::load(Path::new("/nonexistent")).unwrap();
    config.llm.provider = ProviderKind::Orchestrator;

    let mut providers = HashMap::new();
    providers.insert(
        "test".to_string(),
        OrchestratorProviderConfig {
            provider_type: "unknown_type".to_string(),
            model: None,
            base_url: None,
            embedding_model: None,
            filename: None,
            device: None,
        },
    );

    config.llm.orchestrator = Some(crate::config::OrchestratorConfig {
        providers,
        routes: HashMap::new(),
        default: "test".to_string(),
        embed: "test".to_string(),
    });

    let result = build_orchestrator(&config);
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("unknown orchestrator sub-provider type")
    );
}

#[test]
fn build_orchestrator_claude_without_cloud_config_errors() {
    use crate::config::OrchestratorProviderConfig;
    use std::collections::HashMap;

    let mut config = Config::load(Path::new("/nonexistent")).unwrap();
    config.llm.provider = ProviderKind::Orchestrator;
    config.llm.cloud = None;

    let mut providers = HashMap::new();
    providers.insert(
        "claude_sub".to_string(),
        OrchestratorProviderConfig {
            provider_type: "claude".to_string(),
            model: None,
            base_url: None,
            embedding_model: None,
            filename: None,
            device: None,
        },
    );

    config.llm.orchestrator = Some(crate::config::OrchestratorConfig {
        providers,
        routes: HashMap::new(),
        default: "claude_sub".to_string(),
        embed: "claude_sub".to_string(),
    });

    let result = build_orchestrator(&config);
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("llm.cloud config required")
    );
}

#[test]
fn build_orchestrator_claude_sub_without_api_key_errors() {
    use crate::config::OrchestratorProviderConfig;
    use std::collections::HashMap;

    let mut config = Config::load(Path::new("/nonexistent")).unwrap();
    config.llm.provider = ProviderKind::Orchestrator;
    config.llm.cloud = Some(crate::config::CloudLlmConfig {
        model: "claude-3".into(),
        max_tokens: 4096,
    });
    config.secrets.claude_api_key = None;

    let mut providers = HashMap::new();
    providers.insert(
        "claude_sub".to_string(),
        OrchestratorProviderConfig {
            provider_type: "claude".to_string(),
            model: None,
            base_url: None,
            embedding_model: None,
            filename: None,
            device: None,
        },
    );

    config.llm.orchestrator = Some(crate::config::OrchestratorConfig {
        providers,
        routes: HashMap::new(),
        default: "claude_sub".to_string(),
        embed: "claude_sub".to_string(),
    });

    let result = build_orchestrator(&config);
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("ZEPH_CLAUDE_API_KEY required")
    );
}

#[cfg(feature = "candle")]
#[test]
fn build_orchestrator_candle_without_config_errors() {
    use crate::config::OrchestratorProviderConfig;
    use std::collections::HashMap;

    let mut config = Config::load(Path::new("/nonexistent")).unwrap();
    config.llm.provider = ProviderKind::Orchestrator;
    config.llm.candle = None;

    let mut providers = HashMap::new();
    providers.insert(
        "candle_sub".to_string(),
        OrchestratorProviderConfig {
            provider_type: "candle".to_string(),
            model: None,
            base_url: None,
            embedding_model: None,
            filename: None,
            device: None,
        },
    );

    config.llm.orchestrator = Some(crate::config::OrchestratorConfig {
        providers,
        routes: HashMap::new(),
        default: "candle_sub".to_string(),
        embed: "candle_sub".to_string(),
    });

    let result = build_orchestrator(&config);
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("llm.candle config required")
    );
}

#[test]
fn build_orchestrator_with_ollama_sub_provider() {
    use crate::config::OrchestratorProviderConfig;
    use std::collections::HashMap;

    let mut config = Config::load(Path::new("/nonexistent")).unwrap();
    config.llm.provider = ProviderKind::Orchestrator;

    let mut providers = HashMap::new();
    providers.insert(
        "ollama_sub".to_string(),
        OrchestratorProviderConfig {
            provider_type: "ollama".to_string(),
            model: Some("llama2".to_string()),
            base_url: None,
            embedding_model: None,
            filename: None,
            device: None,
        },
    );

    config.llm.orchestrator = Some(crate::config::OrchestratorConfig {
        providers,
        routes: HashMap::new(),
        default: "ollama_sub".to_string(),
        embed: "ollama_sub".to_string(),
    });

    let result = build_orchestrator(&config);
    assert!(result.is_ok());
}

#[test]
fn build_orchestrator_ollama_per_provider_base_url() {
    use crate::config::OrchestratorProviderConfig;
    use std::collections::HashMap;

    let mut config = Config::load(Path::new("/nonexistent")).unwrap();
    config.llm.provider = ProviderKind::Orchestrator;
    config.llm.base_url = "http://localhost:11434".into();

    let mut providers = HashMap::new();
    providers.insert(
        "ollama_custom".to_string(),
        OrchestratorProviderConfig {
            provider_type: "ollama".to_string(),
            model: Some("llama3".to_string()),
            base_url: Some("http://gpu-server:11434".to_string()),
            embedding_model: None,
            filename: None,
            device: None,
        },
    );

    config.llm.orchestrator = Some(crate::config::OrchestratorConfig {
        providers,
        routes: HashMap::new(),
        default: "ollama_custom".to_string(),
        embed: "ollama_custom".to_string(),
    });

    let result = build_orchestrator(&config);
    assert!(result.is_ok());
}

#[test]
fn build_orchestrator_ollama_per_provider_embedding_model() {
    use crate::config::OrchestratorProviderConfig;
    use std::collections::HashMap;

    let mut config = Config::load(Path::new("/nonexistent")).unwrap();
    config.llm.provider = ProviderKind::Orchestrator;

    let mut providers = HashMap::new();
    providers.insert(
        "ollama_embed".to_string(),
        OrchestratorProviderConfig {
            provider_type: "ollama".to_string(),
            model: None,
            base_url: None,
            embedding_model: Some("nomic-embed-text".to_string()),
            filename: None,
            device: None,
        },
    );

    config.llm.orchestrator = Some(crate::config::OrchestratorConfig {
        providers,
        routes: HashMap::new(),
        default: "ollama_embed".to_string(),
        embed: "ollama_embed".to_string(),
    });

    let result = build_orchestrator(&config);
    assert!(result.is_ok());
}

#[test]
fn effective_embedding_model_reads_orchestrator_sub_provider() {
    use crate::config::OrchestratorProviderConfig;
    use std::collections::HashMap;

    let mut config = Config::default();
    config.llm.provider = ProviderKind::Orchestrator;
    config.llm.embedding_model = "default-embed".into();

    let mut providers = HashMap::new();
    providers.insert(
        "ollama_embed".to_string(),
        OrchestratorProviderConfig {
            provider_type: "ollama".to_string(),
            model: None,
            base_url: None,
            embedding_model: Some("custom-embed".to_string()),
            filename: None,
            device: None,
        },
    );

    config.llm.orchestrator = Some(crate::config::OrchestratorConfig {
        providers,
        routes: HashMap::new(),
        default: "ollama_embed".to_string(),
        embed: "ollama_embed".to_string(),
    });

    assert_eq!(effective_embedding_model(&config), "custom-embed");
}

#[test]
fn build_orchestrator_routes_parsing() {
    use crate::config::OrchestratorProviderConfig;
    use std::collections::HashMap;

    let mut config = Config::load(Path::new("/nonexistent")).unwrap();
    config.llm.provider = ProviderKind::Orchestrator;

    let mut providers = HashMap::new();
    providers.insert(
        "ollama_sub".to_string(),
        OrchestratorProviderConfig {
            provider_type: "ollama".to_string(),
            model: None,
            base_url: None,
            embedding_model: None,
            filename: None,
            device: None,
        },
    );

    let mut routes = HashMap::new();
    routes.insert("chat".to_string(), vec!["ollama_sub".to_string()]);
    routes.insert("embed".to_string(), vec!["ollama_sub".to_string()]);

    config.llm.orchestrator = Some(crate::config::OrchestratorConfig {
        providers,
        routes,
        default: "ollama_sub".to_string(),
        embed: "ollama_sub".to_string(),
    });

    let result = build_orchestrator(&config);
    assert!(result.is_ok());
}

#[cfg(feature = "candle")]
#[test]
fn build_orchestrator_with_candle_local_source() {
    use crate::config::OrchestratorProviderConfig;
    use std::collections::HashMap;

    let mut config = Config::load(Path::new("/nonexistent")).unwrap();
    config.llm.provider = ProviderKind::Orchestrator;
    config.llm.candle = Some(crate::config::CandleConfig {
        source: "local".into(),
        local_path: "/tmp/model.gguf".into(),
        filename: Some("model.gguf".to_string()),
        chat_template: "{{ messages[0].content }}".into(),
        device: "cpu".into(),
        embedding_repo: Some("embed/model".into()),
        generation: crate::config::GenerationParams {
            temperature: 0.7,
            top_p: Some(0.9),
            top_k: Some(50),
            max_tokens: 512,
            seed: 42,
            repeat_penalty: 1.1,
            repeat_last_n: 64,
        },
    });

    let mut providers = HashMap::new();
    providers.insert(
        "candle_local".to_string(),
        OrchestratorProviderConfig {
            provider_type: "candle".to_string(),
            model: Some("local-model".to_string()),
            base_url: None,
            embedding_model: None,
            filename: None,
            device: Some("cpu".to_string()),
        },
    );

    config.llm.orchestrator = Some(crate::config::OrchestratorConfig {
        providers,
        routes: HashMap::new(),
        default: "candle_local".to_string(),
        embed: "candle_local".to_string(),
    });

    let result = build_orchestrator(&config);
    assert!(result.is_err(), "expected error loading nonexistent model");
}

#[cfg(feature = "candle")]
#[tokio::test]
async fn health_check_candle_logs_device() {
    use zeph_llm::candle_provider::CandleProvider;

    let source = zeph_llm::candle_provider::loader::ModelSource::HuggingFace {
        repo_id: "test/model".to_string(),
        filename: Some("model.gguf".to_string()),
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

    let candle_result =
        CandleProvider::new(&source, template, gen_config, Some("embed/model"), device);

    if let Ok(candle) = candle_result {
        let provider = AnyProvider::Candle(candle);
        health_check(&provider).await;
    }
}

#[tokio::test]
async fn health_check_orchestrator_logs_providers() {
    use std::collections::HashMap;
    use zeph_llm::orchestrator::{ModelOrchestrator, SubProvider};

    let mut providers = HashMap::new();
    providers.insert(
        "ollama_local".to_string(),
        SubProvider::Ollama(OllamaProvider::new(
            "http://localhost:11434",
            "test".into(),
            "embed".into(),
        )),
    );

    let routes = HashMap::new();
    let orch = ModelOrchestrator::new(
        routes,
        providers,
        "ollama_local".to_string(),
        "ollama_local".to_string(),
    )
    .unwrap();

    let provider = AnyProvider::Orchestrator(Box::new(orch));
    health_check(&provider).await;
}

#[test]
fn create_mcp_manager_with_http_transport() {
    use std::collections::HashMap;

    let mut config = Config::load(Path::new("/nonexistent")).unwrap();
    config.mcp.servers = vec![crate::config::McpServerConfig {
        id: "test".into(),
        url: Some("http://localhost:3000".into()),
        command: None,
        args: vec![],
        env: HashMap::new(),
        timeout: 30,
    }];

    let manager = create_mcp_manager(&config);
    let debug = format!("{manager:?}");
    assert!(debug.contains("server_count: 1"));
}

#[test]
fn create_mcp_manager_with_stdio_transport() {
    use std::collections::HashMap;

    let mut config = Config::load(Path::new("/nonexistent")).unwrap();
    config.mcp.servers = vec![crate::config::McpServerConfig {
        id: "test".into(),
        url: None,
        command: Some("node".into()),
        args: vec!["server.js".into()],
        env: HashMap::new(),
        timeout: 30,
    }];

    let manager = create_mcp_manager(&config);
    let debug = format!("{manager:?}");
    assert!(debug.contains("server_count: 1"));
}

#[test]
fn create_mcp_manager_empty_servers() {
    let mut config = Config::load(Path::new("/nonexistent")).unwrap();
    config.mcp.servers = vec![];

    let manager = create_mcp_manager(&config);
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
    let registry = create_mcp_registry(&config, &provider, &mcp_tools, "test-model").await;
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
    };
    let paths = builder.skill_paths();
    let managed = managed_skills_dir();
    assert!(
        paths.contains(&managed),
        "skill_paths() should include managed_skills_dir, got: {paths:?}"
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
    };
    let paths = builder.skill_paths();
    let count = paths.iter().filter(|p| p == &&managed).count();
    assert_eq!(
        count, 1,
        "managed dir should appear exactly once, got: {paths:?}"
    );
}

#[tokio::test]
async fn create_skill_matcher_when_semantic_disabled() {
    let tmp = std::env::temp_dir().join("zeph_test_skill_matcher_bootstrap.db");
    let tmp_path = tmp.to_string_lossy().to_string();

    let mut config = Config::load(Path::new("/nonexistent")).unwrap();
    config.memory.semantic.enabled = false;
    config.memory.sqlite_path = tmp_path.clone();

    let provider = AnyProvider::Ollama(OllamaProvider::new(
        "http://localhost:11434",
        "test".into(),
        "embed".into(),
    ));

    let memory = SemanticMemory::new(
        &tmp_path,
        &config.memory.qdrant_url,
        provider.clone(),
        &config.llm.embedding_model,
    )
    .await
    .unwrap();

    let meta: Vec<&SkillMeta> = vec![];
    let result = create_skill_matcher(&config, &provider, &meta, &memory, "test-model").await;
    assert!(result.is_none());

    let _ = std::fs::remove_file(&tmp);
}
