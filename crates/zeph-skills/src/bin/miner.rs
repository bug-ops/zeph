// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `zeph-skills-miner` — automated skill mining from GitHub repositories.
//!
//! Reads config from a Zeph `config.toml`, resolves the GitHub token from the vault,
//! searches GitHub for repositories matching configured queries, generates SKILL.md candidates
//! via LLM, deduplicates against existing skills, and writes novel skills to `output_dir`.
//!
//! # Usage
//!
//! ```text
//! zeph-skills-miner [OPTIONS]
//!   --config <PATH>     Path to config.toml (default: ~/.config/zeph/config.toml)
//!   --query <QUERY>     Override search queries from config (repeatable)
//!   --output <DIR>      Override output directory from config
//!   --dry-run           Report without writing to disk
//!   --verbose           Enable debug logging
//! ```

use std::path::PathBuf;

use anyhow::{Context, bail};
use clap::Parser;
use tracing_subscriber::EnvFilter;
use zeph_llm::any::AnyProvider;
use zeph_llm::claude::ClaudeProvider;
use zeph_llm::ollama::OllamaProvider;
use zeph_llm::openai::OpenAiProvider;

use zeph_skills::loader::{SkillMeta, load_skill_meta};
use zeph_skills::miner::{MiningConfig, SkillMiner};

#[derive(Parser, Debug)]
#[command(
    name = "zeph-skills-miner",
    about = "Mine skills from GitHub repositories"
)]
struct Cli {
    /// Path to Zeph config.toml.
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Override search queries from config (repeatable).
    #[arg(long = "query", value_name = "QUERY")]
    queries: Vec<String>,

    /// Override output directory from config.
    #[arg(long, value_name = "DIR")]
    output: Option<PathBuf>,

    /// Generate and report without writing to disk.
    #[arg(long)]
    dry_run: bool,

    /// Enable debug logging.
    #[arg(long)]
    verbose: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    let log_level = if cli.verbose { "debug" } else { "info" };
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(log_level)),
        )
        .init();

    let config_path = cli.config.unwrap_or_else(default_config_path);
    tracing::info!(path = %config_path.display(), "loading config");
    let config_str = std::fs::read_to_string(&config_path)
        .with_context(|| format!("failed to read config from {}", config_path.display()))?;
    let config: zeph_config::Config = toml::from_str(&config_str)
        .with_context(|| format!("failed to parse config from {}", config_path.display()))?;

    let github_token = resolve_github_token()?;

    let generation_provider_name = config.skills.mining.generation_provider.as_str().to_owned();
    let embed_provider_name = config.skills.mining.embedding_provider.as_str().to_owned();

    let generation_provider = build_provider(&config, &generation_provider_name)
        .context("failed to build generation provider")?;
    let embed_provider = if embed_provider_name.is_empty() {
        generation_provider.clone()
    } else {
        build_provider(&config, &embed_provider_name)
            .context("failed to build embedding provider")?
    };

    let output_dir = if let Some(dir) = cli.output {
        dir
    } else if let Some(ref dir) = config.skills.mining.output_dir {
        expand_tilde(dir)
    } else if let Some(first_path) = config.skills.paths.first() {
        expand_tilde(first_path)
    } else {
        bail!("no output directory configured; set skills.mining.output_dir or skills.paths");
    };

    std::fs::create_dir_all(&output_dir)
        .with_context(|| format!("failed to create output dir {}", output_dir.display()))?;

    let queries: Vec<String> = if !cli.queries.is_empty() {
        cli.queries
    } else {
        config.skills.mining.queries.clone()
    };

    if queries.is_empty() {
        bail!("no search queries configured; set skills.mining.queries or pass --query");
    }

    let existing_skills = load_existing_skills(&config);
    tracing::info!(
        existing = existing_skills.len(),
        "loaded existing skills for dedup"
    );

    let mining_config = MiningConfig {
        queries,
        max_repos_per_query: config.skills.mining.max_repos_per_query.min(100),
        dedup_threshold: config.skills.mining.dedup_threshold,
        output_dir: output_dir.clone(),
        rate_limit_rpm: config.skills.mining.rate_limit_rpm,
        dry_run: cli.dry_run,
        generation_timeout_ms: config.skills.mining.generation_timeout_ms,
    };

    let miner = SkillMiner::new(
        generation_provider,
        embed_provider,
        github_token,
        mining_config,
    )?;

    if cli.dry_run {
        tracing::info!("dry-run mode: no files will be written");
    }

    tracing::info!(output = %output_dir.display(), "starting skill mining");

    let results = miner.run(&existing_skills).await?;

    tracing::info!(mined = results.len(), "mining complete");

    for mined in &results {
        println!(
            "[{}] {} (nearest_sim={:.2}{})",
            mined.repo,
            mined.skill.name,
            mined.nearest_similarity,
            if cli.dry_run { ", dry-run" } else { "" }
        );
        for w in &mined.skill.warnings {
            println!("  WARNING: {w}");
        }
    }

    Ok(())
}

fn default_config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("~/.config"))
        .join("zeph")
        .join("config.toml")
}

/// Resolve GitHub token from ZEPH_GITHUB_TOKEN environment variable.
///
/// Production use: store the token in the Zeph age vault via `zeph vault set ZEPH_GITHUB_TOKEN`.
/// The miner binary cannot access the vault directly (no `zeph-core` dependency), so the token
/// must be exported to the environment before running the binary:
///   `export ZEPH_GITHUB_TOKEN=$(zeph vault get ZEPH_GITHUB_TOKEN)`
fn resolve_github_token() -> anyhow::Result<String> {
    match std::env::var("ZEPH_GITHUB_TOKEN") {
        Ok(token) if !token.is_empty() => Ok(token),
        _ => bail!(
            "GitHub token not found. Export it before running:\n  \
             export ZEPH_GITHUB_TOKEN=$(zeph vault get ZEPH_GITHUB_TOKEN)"
        ),
    }
}

/// Build an `AnyProvider` from config, selecting by name (or primary if name is empty).
fn build_provider(config: &zeph_config::Config, name: &str) -> anyhow::Result<AnyProvider> {
    let entry = if name.is_empty() {
        config
            .llm
            .providers
            .first()
            .context("no providers configured in [[llm.providers]]")?
    } else {
        config
            .llm
            .providers
            .iter()
            .find(|e| e.effective_name() == name || e.provider_type.as_str() == name)
            .with_context(|| format!("provider '{name}' not found in [[llm.providers]]"))?
    };

    let model = entry.effective_model();
    let max_tokens = entry.max_tokens.unwrap_or(4096);
    // api_key in ProviderEntry is Option<String> (plain string, not Secret).
    let api_key = entry.api_key.clone().unwrap_or_default();

    let provider = match entry.provider_type {
        zeph_config::ProviderKind::Claude => {
            AnyProvider::Claude(ClaudeProvider::new(api_key, model, max_tokens))
        }
        zeph_config::ProviderKind::OpenAi => {
            let base_url = entry
                .base_url
                .clone()
                .unwrap_or_else(|| "https://api.openai.com/v1".into());
            let embedding_model = entry.embedding_model.clone();
            AnyProvider::OpenAi(OpenAiProvider::new(
                api_key,
                base_url,
                model,
                max_tokens,
                embedding_model,
                None, // reasoning_effort
            ))
        }
        zeph_config::ProviderKind::Ollama => {
            let base_url = entry
                .base_url
                .as_deref()
                .unwrap_or("http://localhost:11434");
            let embed_model = entry
                .embedding_model
                .clone()
                .unwrap_or_else(|| "nomic-embed-text".into());
            AnyProvider::Ollama(OllamaProvider::new(base_url, model, embed_model))
        }
        other => {
            bail!("unsupported provider type '{other}' in miner; use claude, openai, or ollama")
        }
    };

    Ok(provider)
}

fn expand_tilde(path: &str) -> PathBuf {
    if let Some(stripped) = path.strip_prefix("~/") {
        dirs::home_dir()
            .map(|h| h.join(stripped))
            .unwrap_or_else(|| PathBuf::from(path))
    } else {
        PathBuf::from(path)
    }
}

fn load_existing_skills(config: &zeph_config::Config) -> Vec<SkillMeta> {
    let mut skills = Vec::new();
    for path_str in &config.skills.paths {
        let base = expand_tilde(path_str);
        if !base.exists() {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&base) else {
            continue;
        };
        for entry in entries.flatten() {
            let skill_dir = entry.path();
            if !skill_dir.is_dir() {
                continue;
            }
            let skill_path = skill_dir.join("SKILL.md");
            if !skill_path.exists() {
                continue;
            }
            match load_skill_meta(&skill_path) {
                Ok(meta) => skills.push(meta),
                Err(e) => tracing::warn!(
                    path = %skill_path.display(),
                    error = %e,
                    "skipping invalid skill"
                ),
            }
        }
    }
    skills
}
