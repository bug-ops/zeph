// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

pub use zeph_core::provider_factory::{BootstrapError, build_provider_from_entry};

use std::sync::Arc;

use zeph_llm::any::AnyProvider;
use zeph_llm::ollama::OllamaProvider;
use zeph_llm::provider_dyn::LlmProviderDyn;
use zeph_llm::router::cascade::ClassifierMode;
use zeph_llm::router::coe::CoeConfig as RouterCoeConfig;
use zeph_llm::router::triage::{ComplexityTier, TriageRouter};
use zeph_llm::router::{AsiRouterConfig, BanditRouterConfig, CascadeRouterConfig, RouterProvider};

use zeph_core::config::{Config, LlmRoutingStrategy, ProviderEntry};

/// Build the primary `AnyProvider` from the resolved config.
///
/// Delegates to the internal provider pool builder. Entry point for bootstrap and channel
/// initialization — call this once per startup or config reload.
///
/// # Errors
///
/// Returns `BootstrapError::Provider` when no provider in `[[llm.providers]]` can be
/// initialized.
pub fn create_provider(config: &Config) -> Result<AnyProvider, BootstrapError> {
    create_provider_from_pool(config)
}

fn build_cascade_router_config(
    cascade_cfg: &zeph_core::config::CascadeConfig,
    config: &Config,
) -> CascadeRouterConfig {
    let classifier_mode = match cascade_cfg.classifier_mode {
        zeph_core::config::CascadeClassifierMode::Heuristic => ClassifierMode::Heuristic,
        zeph_core::config::CascadeClassifierMode::Judge => ClassifierMode::Judge,
    };
    // SEC-CASCADE-01: clamp quality_threshold to [0.0, 1.0]; reject NaN/Inf.
    let raw_threshold = cascade_cfg.quality_threshold;
    let quality_threshold = if raw_threshold.is_finite() {
        raw_threshold.clamp(0.0, 1.0)
    } else {
        tracing::warn!(
            raw_threshold,
            "cascade quality_threshold is non-finite, defaulting to 0.5"
        );
        0.5
    };
    if (quality_threshold - raw_threshold).abs() > f64::EPSILON {
        tracing::warn!(
            raw_threshold,
            clamped = quality_threshold,
            "cascade quality_threshold out of range [0.0, 1.0], clamped"
        );
    }
    // SEC-CASCADE-02: clamp window_size to minimum 1 to prevent silent no-op tracking.
    let window_size = cascade_cfg.window_size.max(1);
    if window_size != cascade_cfg.window_size {
        tracing::warn!(
            raw = cascade_cfg.window_size,
            "cascade window_size=0 is invalid, clamped to 1"
        );
    }
    // Build summary provider for judge mode.
    let summary_provider: Option<Arc<dyn LlmProviderDyn>> =
        if classifier_mode == ClassifierMode::Judge {
            if let Some(model_spec) = config.llm.summary_model.as_deref() {
                match create_summary_provider(model_spec, config) {
                    Ok(p) => Some(Arc::new(p) as Arc<dyn LlmProviderDyn>),
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "cascade: failed to build judge provider, falling back to heuristic"
                        );
                        None
                    }
                }
            } else {
                tracing::warn!(
                    "cascade: classifier_mode=judge requires [llm] summary_model to \
                     be configured; falling back to heuristic"
                );
                None
            }
        } else {
            None
        };
    CascadeRouterConfig {
        quality_threshold,
        max_escalations: cascade_cfg.max_escalations,
        classifier_mode,
        window_size,
        max_cascade_tokens: cascade_cfg.max_cascade_tokens,
        summary_provider,
        cost_tiers: cascade_cfg.cost_tiers.clone(),
    }
}

/// Clamp a `CoE` threshold to `[0.0, 1.0]` and warn on invalid values.
fn validate_coe_threshold(name: &str, value: f64) -> f64 {
    if value.is_nan() || value.is_infinite() || !(0.0..=1.0).contains(&value) {
        tracing::warn!(
            field = name,
            value,
            "coe: threshold out of [0.0, 1.0] — clamping to valid range"
        );
        return value.clamp(0.0, 1.0);
    }
    value
}

/// Attach `CoE` to a `RouterProvider` if `[llm.coe]` is configured and enabled.
///
/// Skips silently when the secondary or embed provider cannot be resolved.
fn apply_coe(router: RouterProvider, config: &Config) -> RouterProvider {
    let Some(coe_cfg) = config.llm.coe.as_ref() else {
        return router;
    };
    if !coe_cfg.enabled {
        return router;
    }
    let pool = &config.llm.providers;
    let secondary = if coe_cfg.secondary_provider.is_empty() {
        // fall back to the first non-embed provider
        pool.iter()
            .find(|e| !e.embed)
            .and_then(|e| build_provider_from_entry(e, config).ok())
    } else {
        pool.iter()
            .find(|e| e.effective_name() == coe_cfg.secondary_provider.as_str())
            .and_then(|e| build_provider_from_entry(e, config).ok())
    };
    let embed = if coe_cfg.embed_provider.is_empty() {
        pool.iter()
            .find(|e| e.embed)
            .and_then(|e| build_provider_from_entry(e, config).ok())
    } else {
        pool.iter()
            .find(|e| e.effective_name() == coe_cfg.embed_provider.as_str())
            .and_then(|e| build_provider_from_entry(e, config).ok())
    };
    if let (Some(sec), Some(emb)) = (secondary, embed) {
        let intra = validate_coe_threshold("intra_threshold", coe_cfg.intra_threshold);
        let inter = validate_coe_threshold("inter_threshold", coe_cfg.inter_threshold);
        let shadow = validate_coe_threshold("shadow_sample_rate", coe_cfg.shadow_sample_rate);
        let router_coe = RouterCoeConfig {
            intra_threshold: intra,
            inter_threshold: inter,
            shadow_sample_rate: shadow,
        };
        tracing::info!("coe: enabled (intra={:.2} inter={:.2})", intra, inter);
        router.with_coe(router_coe, sec, emb)
    } else {
        tracing::warn!("coe: secondary or embed provider not resolved, CoE disabled");
        router
    }
}

/// Apply ASI and `quality_gate` configuration to a `RouterProvider` from `[llm.routing]` config.
fn apply_routing_signals(router: RouterProvider, config: &Config) -> RouterProvider {
    let router_cfg = config.llm.router.as_ref();
    let mut router = router;

    // ASI coherence tracking.
    if let Some(asi_cfg) = router_cfg.and_then(|r| r.asi.as_ref())
        && asi_cfg.enabled
    {
        let threshold = asi_cfg.coherence_threshold.clamp(0.0, 1.0);
        let penalty = asi_cfg.penalty_weight.clamp(0.0, 1.0);
        if (threshold - asi_cfg.coherence_threshold).abs() > f32::EPSILON
            || (penalty - asi_cfg.penalty_weight).abs() > f32::EPSILON
        {
            tracing::warn!("asi: coherence_threshold/penalty_weight clamped to [0.0, 1.0]");
        }
        router = router.with_asi(AsiRouterConfig {
            window: asi_cfg.window,
            coherence_threshold: threshold,
            penalty_weight: penalty,
        });
    }

    // Quality gate.
    if let Some(threshold) = router_cfg.and_then(|r| r.quality_gate) {
        if threshold.is_finite() && threshold > 0.0 && threshold <= 1.0 {
            router = router.with_quality_gate(threshold);
        } else {
            tracing::warn!(
                quality_gate = threshold,
                "quality_gate must be in (0.0, 1.0], ignoring"
            );
        }
    }

    // Embed concurrency semaphore.
    let embed_concurrency = router_cfg.map_or(4, |r| r.embed_concurrency);
    router = router.with_embed_concurrency(embed_concurrency);

    router
}

/// Look up a provider entry from the pool by name (exact match on `effective_name()`) or type.
///
/// Used by quarantine, guardrail, judge, and experiment eval model resolution.
pub fn create_named_provider(name: &str, config: &Config) -> Result<AnyProvider, BootstrapError> {
    let entry = config
        .llm
        .providers
        .iter()
        .find(|e| e.effective_name() == name || e.provider_type.as_str() == name)
        .ok_or_else(|| {
            BootstrapError::Provider(format!("provider '{name}' not found in [[llm.providers]]"))
        })?;
    build_provider_from_entry(entry, config)
}

/// Create an `AnyProvider` for use as the summarization provider.
///
/// `model_spec` format (set via `[llm] summary_model`):
/// - `<name>` — looks up a provider by name in `[[llm.providers]]`
/// - `ollama/<model>` — Ollama shorthand: uses the ollama provider from pool with model override
/// - `claude[/<model>]`, `openai[/<model>]`, `gemini[/<model>]` — type shorthand with optional model
pub fn create_summary_provider(
    model_spec: &str,
    config: &Config,
) -> Result<AnyProvider, BootstrapError> {
    // Try direct name lookup first (e.g. "claude", "my-openai").
    if let Some(entry) = config
        .llm
        .providers
        .iter()
        .find(|e| e.effective_name() == model_spec || e.provider_type.as_str() == model_spec)
    {
        return build_provider_from_entry(entry, config);
    }

    // Handle `type/model` shorthand: override the model on a matching provider.
    if let Some(((_, model), entry)) = model_spec.split_once('/').and_then(|(b, m)| {
        config
            .llm
            .providers
            .iter()
            .find(|e| e.provider_type.as_str() == b || e.effective_name() == b)
            .map(|e| ((b, m), e))
    }) {
        let mut cloned = entry.clone();
        cloned.model = Some(model.to_owned());
        // Cap summary max_tokens at 4096 — summaries are short.
        cloned.max_tokens = Some(cloned.max_tokens.unwrap_or(4096).min(4096));
        return build_provider_from_entry(&cloned, config);
    }

    Err(BootstrapError::Provider(format!(
        "summary_model '{model_spec}' not found in [[llm.providers]]. \
         Use a provider name or 'type/model' shorthand (e.g. 'ollama/qwen3:1.7b')."
    )))
}

/// Build the primary `AnyProvider` from the new `[[llm.providers]]` pool.
///
/// When `[llm] routing` is set to a non-None strategy, all providers in the pool are
/// initialized and wrapped in a `RouterProvider` with the appropriate strategy.
/// When routing is `None`, selects the provider marked `default = true` (or the first
/// entry) and falls back to subsequent entries on initialization failure.
#[allow(clippy::too_many_lines)]
fn create_provider_from_pool(config: &Config) -> Result<AnyProvider, BootstrapError> {
    let pool = &config.llm.providers;

    // Empty pool → default Ollama on localhost.
    if pool.is_empty() {
        let base_url = config.llm.effective_base_url();
        let model = config.llm.effective_model();
        let embed = &config.llm.embedding_model;
        return Ok(AnyProvider::Ollama(OllamaProvider::new(
            base_url,
            model.to_owned(),
            embed.clone(),
        )));
    }

    match config.llm.routing {
        LlmRoutingStrategy::None => build_single_provider_from_pool(pool, config),
        LlmRoutingStrategy::Ema => {
            let providers = build_all_pool_providers(pool, config)?;
            let raw_alpha = config.llm.router_ema_alpha;
            let alpha = raw_alpha.clamp(f64::MIN_POSITIVE, 1.0);
            if (alpha - raw_alpha).abs() > f64::EPSILON {
                tracing::warn!(
                    raw_alpha,
                    clamped = alpha,
                    "router_ema_alpha out of range [MIN_POSITIVE, 1.0], clamped"
                );
            }
            let router =
                RouterProvider::new(providers).with_ema(alpha, config.llm.router_reorder_interval);
            let router = apply_coe(router, config);
            Ok(AnyProvider::Router(Box::new(apply_routing_signals(
                router, config,
            ))))
        }
        LlmRoutingStrategy::Thompson => {
            let providers = build_all_pool_providers(pool, config)?;
            let state_path = config
                .llm
                .router
                .as_ref()
                .and_then(|r| r.thompson_state_path.as_deref())
                .map(std::path::Path::new);
            let router = RouterProvider::new(providers).with_thompson(state_path);
            let router = apply_coe(router, config);
            Ok(AnyProvider::Router(Box::new(apply_routing_signals(
                router, config,
            ))))
        }
        LlmRoutingStrategy::Cascade => {
            let providers = build_all_pool_providers(pool, config)?;
            let cascade_cfg = config
                .llm
                .router
                .as_ref()
                .and_then(|r| r.cascade.clone())
                .unwrap_or_default();
            let router_cascade_cfg = build_cascade_router_config(&cascade_cfg, config);
            let embed_concurrency = config
                .llm
                .router
                .as_ref()
                .map_or(4, |r| r.embed_concurrency);
            Ok(AnyProvider::Router(Box::new(
                RouterProvider::new(providers)
                    .with_cascade(router_cascade_cfg)
                    .with_embed_concurrency(embed_concurrency),
            )))
        }
        LlmRoutingStrategy::Bandit => {
            let providers = build_all_pool_providers(pool, config)?;
            let bandit_cfg = config
                .llm
                .router
                .as_ref()
                .and_then(|r| r.bandit.clone())
                .unwrap_or_default();
            let state_path = bandit_cfg.state_path.as_deref().map(std::path::Path::new);
            let router_bandit_cfg = BanditRouterConfig {
                alpha: bandit_cfg.alpha,
                dim: bandit_cfg.dim,
                cost_weight: bandit_cfg.cost_weight.clamp(0.0, 1.0),
                decay_factor: bandit_cfg.decay_factor,
                warmup_queries: bandit_cfg.warmup_queries.unwrap_or(0),
                embedding_timeout_ms: bandit_cfg.embedding_timeout_ms,
                cache_size: bandit_cfg.cache_size,
                memory_confidence_threshold: bandit_cfg.memory_confidence_threshold.clamp(0.0, 1.0),
            };
            // Resolve embedding provider for feature vectors.
            let embed_provider = if bandit_cfg.embedding_provider.is_empty() {
                None
            } else if let Some(entry) = pool
                .iter()
                .find(|e| e.effective_name() == bandit_cfg.embedding_provider.as_str())
            {
                match build_provider_from_entry(entry, config) {
                    Ok(p) => Some(p),
                    Err(e) => {
                        tracing::warn!(
                            provider = %bandit_cfg.embedding_provider,
                            error = %e,
                            "bandit: embedding provider failed to init, bandit will use Thompson fallback"
                        );
                        None
                    }
                }
            } else {
                tracing::warn!(
                    provider = %bandit_cfg.embedding_provider,
                    "bandit: embedding_provider not found in [[llm.providers]], \
                     bandit will use Thompson fallback"
                );
                None
            };
            let embed_concurrency = config
                .llm
                .router
                .as_ref()
                .map_or(4, |r| r.embed_concurrency);
            Ok(AnyProvider::Router(Box::new(
                RouterProvider::new(providers)
                    .with_bandit(router_bandit_cfg, state_path, embed_provider)
                    .with_embed_concurrency(embed_concurrency),
            )))
        }
        LlmRoutingStrategy::Triage => build_triage_provider(pool, config),
    }
}

/// Initialize all providers in the pool, skipping those that fail with a warning.
/// Returns an error if no provider could be initialized.
fn build_all_pool_providers(
    pool: &[ProviderEntry],
    config: &Config,
) -> Result<Vec<AnyProvider>, BootstrapError> {
    let mut providers = Vec::new();
    for entry in pool {
        if entry.embed {
            continue;
        }
        match build_provider_from_entry(entry, config) {
            Ok(p) => providers.push(p),
            Err(e) => {
                tracing::warn!(
                    provider = entry.name.as_deref().unwrap_or("?"),
                    error = %e,
                    "skipping pool provider during routing initialization"
                );
            }
        }
    }
    if providers.is_empty() {
        return Err(BootstrapError::Provider(
            "routing enabled but no providers in [[llm.providers]] could be initialized".into(),
        ));
    }
    Ok(providers)
}

/// Build a `TriageRouter`-backed `AnyProvider` from the pool.
///
/// Reads `[llm.complexity_routing]` config and constructs tier providers by name lookup.
/// If `bypass_single_provider = true` and all configured tiers resolve to the same provider,
/// returns a single provider instead of wrapping in a `TriageRouter`.
fn build_triage_provider(
    pool: &[zeph_core::config::ProviderEntry],
    config: &zeph_core::config::Config,
) -> Result<AnyProvider, BootstrapError> {
    let cr = config.llm.complexity_routing.as_ref().ok_or_else(|| {
        BootstrapError::Provider(
            "routing = \"triage\" requires [llm.complexity_routing] section".into(),
        )
    })?;

    // Resolve triage classification provider.
    let default_triage_name = pool
        .first()
        .map(zeph_core::config::ProviderEntry::effective_name)
        .unwrap_or_default();
    let triage_prov_name = cr
        .triage_provider
        .as_deref()
        .unwrap_or(default_triage_name.as_str());
    let triage_provider = create_named_provider(triage_prov_name, config).map_err(|e| {
        BootstrapError::Provider(format!(
            "triage_provider '{triage_prov_name}' not found in [[llm.providers]]: {e}"
        ))
    })?;

    // Build tier provider list. Tiers not configured in the mapping are skipped.
    let tier_config: [(ComplexityTier, Option<&str>); 4] = [
        (ComplexityTier::Simple, cr.tiers.simple.as_deref()),
        (ComplexityTier::Medium, cr.tiers.medium.as_deref()),
        (ComplexityTier::Complex, cr.tiers.complex.as_deref()),
        (ComplexityTier::Expert, cr.tiers.expert.as_deref()),
    ];

    // Collect (tier, config_name, provider) triples.
    // Bypass detection compares config names (not provider.name()) to correctly distinguish
    // two pool entries using the same provider type (e.g., two Claude configs for Haiku + Opus).
    let mut tier_providers: Vec<(ComplexityTier, AnyProvider)> = Vec::new();
    let mut tier_config_names: Vec<&str> = Vec::new();
    for (tier, maybe_name) in &tier_config {
        let Some(name) = maybe_name else { continue };
        match create_named_provider(name, config) {
            Ok(p) => {
                tier_providers.push((*tier, p));
                tier_config_names.push(name);
            }
            Err(e) => {
                tracing::warn!(
                    tier = tier.as_str(),
                    provider = name,
                    error = %e,
                    "triage: skipping tier provider (not found in pool)"
                );
            }
        }
    }

    if tier_providers.is_empty() {
        // No tiers configured — fall through to single provider.
        tracing::warn!(
            "triage routing: no tier providers configured, \
             falling back to single provider"
        );
        return build_single_provider_from_pool(pool, config);
    }

    // bypass_single_provider: if all tiers reference the same config entry name, skip triage.
    if cr.bypass_single_provider
        && let Some(first_name) = tier_config_names
            .first()
            .copied()
            .filter(|&n| tier_config_names.iter().all(|m| *m == n))
    {
        tracing::debug!(
            provider = first_name,
            "triage routing: all tiers map to same config entry, bypassing triage"
        );
        return build_single_provider_from_pool(pool, config);
    }

    let router = TriageRouter::new(
        triage_provider,
        tier_providers,
        cr.triage_timeout_secs,
        cr.max_triage_tokens,
    );
    Ok(AnyProvider::Triage(Box::new(router)))
}

/// Pick the default (or first) provider from the pool with fallback on failure.
fn build_single_provider_from_pool(
    pool: &[ProviderEntry],
    config: &Config,
) -> Result<AnyProvider, BootstrapError> {
    let primary_idx = pool.iter().position(|e| e.default).unwrap_or(0);
    let primary = &pool[primary_idx];
    match build_provider_from_entry(primary, config) {
        Ok(p) => Ok(p),
        Err(e) => {
            let name = primary.name.as_deref().unwrap_or("primary");
            tracing::warn!(provider = name, error = %e, "primary provider failed, trying next");
            for (i, entry) in pool.iter().enumerate() {
                if i == primary_idx {
                    continue;
                }
                match build_provider_from_entry(entry, config) {
                    Ok(p) => return Ok(p),
                    Err(e2) => {
                        tracing::warn!(
                            provider = entry.name.as_deref().unwrap_or("?"),
                            error = %e2,
                            "fallback provider failed"
                        );
                    }
                }
            }
            Err(BootstrapError::Provider(format!(
                "all providers in [[llm.providers]] failed to initialize; first error: {e}"
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use zeph_core::config::{Config, ProviderEntry, ProviderKind};

    use super::build_all_pool_providers;

    #[test]
    fn excludes_embed_only_entry() {
        let mut config = Config::load(Path::new("/nonexistent")).unwrap();
        config.llm.providers = vec![
            ProviderEntry {
                provider_type: ProviderKind::Ollama,
                name: Some("chat".into()),
                model: Some("qwen3:8b".into()),
                embed: false,
                ..ProviderEntry::default()
            },
            ProviderEntry {
                provider_type: ProviderKind::Ollama,
                name: Some("embedder".into()),
                model: Some("nomic-embed-text".into()),
                embed: true,
                ..ProviderEntry::default()
            },
        ];
        let providers = build_all_pool_providers(&config.llm.providers, &config).unwrap();
        assert_eq!(providers.len(), 1);
    }

    #[test]
    fn includes_all_non_embed_entries() {
        let mut config = Config::load(Path::new("/nonexistent")).unwrap();
        config.llm.providers = vec![
            ProviderEntry {
                provider_type: ProviderKind::Ollama,
                name: Some("chat1".into()),
                model: Some("qwen3:8b".into()),
                embed: false,
                ..ProviderEntry::default()
            },
            ProviderEntry {
                provider_type: ProviderKind::Ollama,
                name: Some("chat2".into()),
                model: Some("qwen3:1.7b".into()),
                embed: false,
                ..ProviderEntry::default()
            },
        ];
        let providers = build_all_pool_providers(&config.llm.providers, &config).unwrap();
        assert_eq!(providers.len(), 2);
    }

    #[test]
    fn errors_when_all_providers_are_embed_only() {
        let mut config = Config::load(Path::new("/nonexistent")).unwrap();
        config.llm.providers = vec![ProviderEntry {
            provider_type: ProviderKind::Ollama,
            name: Some("embedder".into()),
            model: Some("nomic-embed-text".into()),
            embed: true,
            ..ProviderEntry::default()
        }];
        let result = build_all_pool_providers(&config.llm.providers, &config);
        assert!(result.is_err());
    }

    #[test]
    fn active_provider_name_skips_embed_only_first_entry() {
        let providers = vec![
            ProviderEntry {
                provider_type: ProviderKind::Ollama,
                name: Some("embedder".into()),
                model: Some("nomic-embed-text".into()),
                embed: true,
                ..ProviderEntry::default()
            },
            ProviderEntry {
                provider_type: ProviderKind::Ollama,
                name: Some("chat".into()),
                model: Some("qwen3:8b".into()),
                embed: false,
                ..ProviderEntry::default()
            },
        ];
        let active = providers
            .iter()
            .find(|e| !e.embed)
            .map_or_else(String::new, ProviderEntry::effective_name);
        assert_eq!(active, "chat");
    }
}
