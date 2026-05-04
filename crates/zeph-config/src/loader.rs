// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::path::Path;

use crate::error::ConfigError;
use crate::root::Config;

impl Config {
    /// Load configuration from a TOML file with env var overrides.
    ///
    /// Falls back to sensible defaults when the file does not exist.
    ///
    /// # Errors
    ///
    /// Returns an error if the file exists but cannot be read or parsed.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let mut config = if path.exists() {
            let content = std::fs::read_to_string(path)?;
            toml::from_str::<Self>(&content)?
        } else {
            Self::default()
        };

        config.apply_env_overrides();
        config.normalize_legacy_runtime_defaults();
        Ok(config)
    }

    /// Serialize the default configuration to a TOML string.
    ///
    /// Produces a pretty-printed TOML representation of [`Config::default()`].
    /// Useful for bootstrapping a new config file or documenting available options.
    ///
    /// The `secrets` field is always excluded from the output because it is
    /// populated at runtime only and must never be written to disk.
    ///
    /// # Errors
    ///
    /// Returns an error if serialization fails (unlikely — the default value is
    /// always structurally valid).
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use zeph_config::Config;
    ///
    /// let toml = Config::dump_defaults().expect("serialization failed");
    /// assert!(toml.contains("[agent]"));
    /// assert!(toml.contains("[memory]"));
    /// ```
    pub fn dump_defaults() -> Result<String, crate::error::ConfigError> {
        let defaults = Self::default();
        toml::to_string_pretty(&defaults).map_err(|e| {
            crate::error::ConfigError::Validation(format!("failed to serialize defaults: {e}"))
        })
    }

    /// Validate configuration values are within sane bounds.
    ///
    /// # Errors
    ///
    /// Returns an error if any value is out of range.
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.validate_scalar_bounds()?;
        self.validate_memory_compression()?;
        self.validate_memory_probe_and_graph()?;
        self.validate_mcp_servers()?;
        self.experiments
            .validate()
            .map_err(ConfigError::Validation)?;
        if self.orchestration.plan_cache.enabled {
            self.orchestration
                .plan_cache
                .validate()
                .map_err(ConfigError::Validation)?;
        }
        self.validate_orchestration()?;
        self.validate_focus_and_sidequest()?;
        self.validate_llm_and_skills()?;
        self.validate_provider_names()?;
        self.validate_mcp_misc()?;
        Ok(())
    }

    /// Validate scalar bounds for memory, agent, a2a, and gateway fields.
    fn validate_scalar_bounds(&self) -> Result<(), ConfigError> {
        if self.memory.history_limit > 10_000 {
            return Err(ConfigError::Validation(format!(
                "history_limit must be <= 10000, got {}",
                self.memory.history_limit
            )));
        }
        if self.memory.context_budget_tokens > 1_000_000 {
            return Err(ConfigError::Validation(format!(
                "context_budget_tokens must be <= 1000000, got {}",
                self.memory.context_budget_tokens
            )));
        }
        if self.agent.max_tool_iterations > 100 {
            return Err(ConfigError::Validation(format!(
                "max_tool_iterations must be <= 100, got {}",
                self.agent.max_tool_iterations
            )));
        }
        if self.a2a.rate_limit == 0 {
            return Err(ConfigError::Validation("a2a.rate_limit must be > 0".into()));
        }
        if self.gateway.rate_limit == 0 {
            return Err(ConfigError::Validation(
                "gateway.rate_limit must be > 0".into(),
            ));
        }
        if self.gateway.max_body_size > 10_485_760 {
            return Err(ConfigError::Validation(format!(
                "gateway.max_body_size must be <= 10485760 (10 MiB), got {}",
                self.gateway.max_body_size
            )));
        }
        if self.memory.token_safety_margin <= 0.0 {
            return Err(ConfigError::Validation(format!(
                "token_safety_margin must be > 0.0, got {}",
                self.memory.token_safety_margin
            )));
        }
        if self.memory.tool_call_cutoff == 0 {
            return Err(ConfigError::Validation(
                "tool_call_cutoff must be >= 1".into(),
            ));
        }
        Ok(())
    }

    /// Validate memory compression strategy bounds and compaction thresholds.
    fn validate_memory_compression(&self) -> Result<(), ConfigError> {
        if let crate::memory::CompressionStrategy::Proactive {
            threshold_tokens,
            max_summary_tokens,
        } = &self.memory.compression.strategy
        {
            if *threshold_tokens < 1_000 {
                return Err(ConfigError::Validation(format!(
                    "compression.threshold_tokens must be >= 1000, got {threshold_tokens}"
                )));
            }
            if *max_summary_tokens < 128 {
                return Err(ConfigError::Validation(format!(
                    "compression.max_summary_tokens must be >= 128, got {max_summary_tokens}"
                )));
            }
        }
        if !self.memory.soft_compaction_threshold.is_finite()
            || self.memory.soft_compaction_threshold <= 0.0
            || self.memory.soft_compaction_threshold >= 1.0
        {
            return Err(ConfigError::Validation(format!(
                "soft_compaction_threshold must be in (0.0, 1.0) exclusive, got {}",
                self.memory.soft_compaction_threshold
            )));
        }
        if !self.memory.hard_compaction_threshold.is_finite()
            || self.memory.hard_compaction_threshold <= 0.0
            || self.memory.hard_compaction_threshold >= 1.0
        {
            return Err(ConfigError::Validation(format!(
                "hard_compaction_threshold must be in (0.0, 1.0) exclusive, got {}",
                self.memory.hard_compaction_threshold
            )));
        }
        if self.memory.soft_compaction_threshold >= self.memory.hard_compaction_threshold {
            return Err(ConfigError::Validation(format!(
                "soft_compaction_threshold ({}) must be less than hard_compaction_threshold ({})",
                self.memory.soft_compaction_threshold, self.memory.hard_compaction_threshold,
            )));
        }
        Ok(())
    }

    /// Validate memory probe thresholds and graph temporal decay rate.
    fn validate_memory_probe_and_graph(&self) -> Result<(), ConfigError> {
        if self.memory.graph.temporal_decay_rate < 0.0
            || self.memory.graph.temporal_decay_rate > 10.0
        {
            return Err(ConfigError::Validation(format!(
                "memory.graph.temporal_decay_rate must be in [0.0, 10.0], got {}",
                self.memory.graph.temporal_decay_rate
            )));
        }
        if self.memory.compression.probe.enabled {
            let probe = &self.memory.compression.probe;
            if !probe.threshold.is_finite() || probe.threshold <= 0.0 || probe.threshold > 1.0 {
                return Err(ConfigError::Validation(format!(
                    "memory.compression.probe.threshold must be in (0.0, 1.0], got {}",
                    probe.threshold
                )));
            }
            if !probe.hard_fail_threshold.is_finite()
                || probe.hard_fail_threshold < 0.0
                || probe.hard_fail_threshold >= 1.0
            {
                return Err(ConfigError::Validation(format!(
                    "memory.compression.probe.hard_fail_threshold must be in [0.0, 1.0), got {}",
                    probe.hard_fail_threshold
                )));
            }
            if probe.hard_fail_threshold >= probe.threshold {
                return Err(ConfigError::Validation(format!(
                    "memory.compression.probe.hard_fail_threshold ({}) must be less than \
                     memory.compression.probe.threshold ({})",
                    probe.hard_fail_threshold, probe.threshold
                )));
            }
            if probe.max_questions < 1 {
                return Err(ConfigError::Validation(
                    "memory.compression.probe.max_questions must be >= 1".into(),
                ));
            }
            if probe.timeout_secs < 1 {
                return Err(ConfigError::Validation(
                    "memory.compression.probe.timeout_secs must be >= 1".into(),
                ));
            }
        }
        Ok(())
    }

    /// Validate MCP server entries for header/oauth exclusivity and vault key uniqueness.
    fn validate_mcp_servers(&self) -> Result<(), ConfigError> {
        use std::collections::HashSet;
        let mut seen_oauth_vault_keys: HashSet<String> = HashSet::new();
        for s in &self.mcp.servers {
            // headers and oauth are mutually exclusive
            if !s.headers.is_empty() && s.oauth.as_ref().is_some_and(|o| o.enabled) {
                return Err(ConfigError::Validation(format!(
                    "MCP server '{}': cannot use both 'headers' and 'oauth' simultaneously",
                    s.id
                )));
            }
            // vault key collision detection
            if s.oauth.as_ref().is_some_and(|o| o.enabled) {
                let key = format!("ZEPH_MCP_OAUTH_{}", s.id.to_uppercase().replace('-', "_"));
                if !seen_oauth_vault_keys.insert(key.clone()) {
                    return Err(ConfigError::Validation(format!(
                        "MCP server '{}' has vault key collision ('{key}'): another server \
                         with the same normalized ID already uses this key",
                        s.id
                    )));
                }
            }
        }
        Ok(())
    }

    /// Validate orchestration thresholds and cascade settings.
    fn validate_orchestration(&self) -> Result<(), ConfigError> {
        let ct = self.orchestration.completeness_threshold;
        if !ct.is_finite() || !(0.0..=1.0).contains(&ct) {
            return Err(ConfigError::Validation(format!(
                "orchestration.completeness_threshold must be in [0.0, 1.0], got {ct}"
            )));
        }
        // Cascade chain threshold must not be 1 — that would abort on every single failure.
        if self.orchestration.cascade_chain_threshold == 1 {
            return Err(ConfigError::Validation(
                "orchestration.cascade_chain_threshold=1 aborts on every failure; \
                 use 0 to disable linear-chain cascade abort instead"
                    .into(),
            ));
        }
        let cfrat = self.orchestration.cascade_failure_rate_abort_threshold;
        if !cfrat.is_finite() || !(0.0..=1.0).contains(&cfrat) {
            return Err(ConfigError::Validation(format!(
                "orchestration.cascade_failure_rate_abort_threshold must be in [0.0, 1.0], got {cfrat}"
            )));
        }
        if self.orchestration.lineage_ttl_secs == 0 {
            return Err(ConfigError::Validation(
                "orchestration.lineage_ttl_secs must be > 0; \
                 set cascade_chain_threshold=0 to disable lineage tracking instead"
                    .into(),
            ));
        }
        Ok(())
    }

    /// Validate focus and sidequest interval and ratio constraints.
    fn validate_focus_and_sidequest(&self) -> Result<(), ConfigError> {
        if self.agent.focus.compression_interval == 0 {
            return Err(ConfigError::Validation(
                "agent.focus.compression_interval must be >= 1".into(),
            ));
        }
        if self.agent.focus.min_messages_per_focus == 0 {
            return Err(ConfigError::Validation(
                "agent.focus.min_messages_per_focus must be >= 1".into(),
            ));
        }
        if self.agent.focus.auto_consolidate_min_window == 0 {
            return Err(ConfigError::Validation(
                "agent.focus.auto_consolidate_min_window must be >= 1 \
                 (set focus.enabled = false to disable auto-consolidation)"
                    .into(),
            ));
        }
        if self.memory.sidequest.interval_turns == 0 {
            return Err(ConfigError::Validation(
                "memory.sidequest.interval_turns must be >= 1".into(),
            ));
        }
        if !self.memory.sidequest.max_eviction_ratio.is_finite()
            || self.memory.sidequest.max_eviction_ratio <= 0.0
            || self.memory.sidequest.max_eviction_ratio > 1.0
        {
            return Err(ConfigError::Validation(format!(
                "memory.sidequest.max_eviction_ratio must be in (0.0, 1.0], got {}",
                self.memory.sidequest.max_eviction_ratio
            )));
        }
        Ok(())
    }

    /// Validate LLM semantic cache threshold and skill evaluation weight sum.
    fn validate_llm_and_skills(&self) -> Result<(), ConfigError> {
        let sct = self.llm.semantic_cache_threshold;
        if !(sct.is_finite() && (0.0..=1.0).contains(&sct)) {
            return Err(ConfigError::Validation(format!(
                "llm.semantic_cache_threshold must be in [0.0, 1.0], got {sct} \
                 (override via ZEPH_LLM_SEMANTIC_CACHE_THRESHOLD env var)"
            )));
        }
        // MemCoT distill provider fast-tier soft-warn (#3574).
        if self.memory.memcot.enabled && !self.memory.memcot.distill_provider.is_empty() {
            self.llm.warn_non_fast_tier_provider(
                &self.memory.memcot.distill_provider,
                "memory.memcot.distill_provider",
                &self.memory.memcot.fast_tier_models,
            );
        }
        // Skill evaluation weight-sum validation (#3319).
        if self.skills.evaluation.enabled {
            let weight_sum = self.skills.evaluation.weight_correctness
                + self.skills.evaluation.weight_reusability
                + self.skills.evaluation.weight_specificity;
            if (weight_sum - 1.0_f32).abs() > 1e-3 {
                return Err(ConfigError::Validation(format!(
                    "skills.evaluation weights must sum to 1.0 (got {weight_sum:.4})"
                )));
            }
        }
        Ok(())
    }

    /// Validate miscellaneous MCP output schema hint size.
    fn validate_mcp_misc(&self) -> Result<(), ConfigError> {
        if self.mcp.output_schema_hint_bytes < 64 {
            return Err(ConfigError::Validation(format!(
                "mcp.output_schema_hint_bytes must be >= 64, got {}; \
                 use forward_output_schema = false to disable forwarding",
                self.mcp.output_schema_hint_bytes
            )));
        }
        Ok(())
    }

    fn validate_provider_names(&self) -> Result<(), ConfigError> {
        let known = self.known_provider_names();
        self.validate_named_provider_refs(&known)?;
        self.validate_optional_provider_refs(&known)?;
        Ok(())
    }

    /// Build the set of declared provider names from all `[[llm.providers]]` entries.
    fn known_provider_names(&self) -> std::collections::HashSet<String> {
        self.llm
            .providers
            .iter()
            .map(super::providers::ProviderEntry::effective_name)
            .collect()
    }

    /// Validate every required `*_provider` field references a declared provider.
    ///
    /// The field table lists all 19 subsystem provider references. Each non-empty value must
    /// match a name in `known`.
    fn validate_named_provider_refs(
        &self,
        known: &std::collections::HashSet<String>,
    ) -> Result<(), ConfigError> {
        let fields: &[(&str, &crate::providers::ProviderName)] = &[
            (
                "memory.tiers.scene_provider",
                &self.memory.tiers.scene_provider,
            ),
            (
                "memory.compression.compress_provider",
                &self.memory.compression.compress_provider,
            ),
            (
                "memory.consolidation.consolidation_provider",
                &self.memory.consolidation.consolidation_provider,
            ),
            (
                "memory.admission.admission_provider",
                &self.memory.admission.admission_provider,
            ),
            (
                "memory.admission.goal_utility_provider",
                &self.memory.admission.goal_utility_provider,
            ),
            (
                "memory.store_routing.routing_classifier_provider",
                &self.memory.store_routing.routing_classifier_provider,
            ),
            (
                "skills.learning.feedback_provider",
                &self.skills.learning.feedback_provider,
            ),
            (
                "skills.learning.arise_trace_provider",
                &self.skills.learning.arise_trace_provider,
            ),
            (
                "skills.learning.stem_provider",
                &self.skills.learning.stem_provider,
            ),
            (
                "skills.learning.erl_extract_provider",
                &self.skills.learning.erl_extract_provider,
            ),
            (
                "mcp.pruning.pruning_provider",
                &self.mcp.pruning.pruning_provider,
            ),
            (
                "mcp.tool_discovery.embedding_provider",
                &self.mcp.tool_discovery.embedding_provider,
            ),
            (
                "security.response_verification.verifier_provider",
                &self.security.response_verification.verifier_provider,
            ),
            (
                "orchestration.planner_provider",
                &self.orchestration.planner_provider,
            ),
            (
                "orchestration.verify_provider",
                &self.orchestration.verify_provider,
            ),
            (
                "orchestration.tool_provider",
                &self.orchestration.tool_provider,
            ),
            (
                "skills.evaluation.provider",
                &self.skills.evaluation.provider,
            ),
            (
                "skills.proactive_exploration.provider",
                &self.skills.proactive_exploration.provider,
            ),
            (
                "memory.compression_spectrum.promotion_provider",
                &self.memory.compression_spectrum.promotion_provider,
            ),
        ];

        for (field, name) in fields {
            if !name.is_empty() && !known.contains(name.as_str()) {
                return Err(ConfigError::Validation(format!(
                    "{field} = {:?} does not match any [[llm.providers]] entry",
                    name.as_str()
                )));
            }
        }
        Ok(())
    }

    /// Validate optional provider references in complexity routing and router bandit config.
    fn validate_optional_provider_refs(
        &self,
        known: &std::collections::HashSet<String>,
    ) -> Result<(), ConfigError> {
        if let Some(triage) = self
            .llm
            .complexity_routing
            .as_ref()
            .and_then(|cr| cr.triage_provider.as_ref())
            .filter(|t| !t.is_empty() && !known.contains(t.as_str()))
        {
            return Err(ConfigError::Validation(format!(
                "llm.complexity_routing.triage_provider = {:?} does not match any \
                 [[llm.providers]] entry",
                triage.as_str()
            )));
        }

        if let Some(embed) = self
            .llm
            .router
            .as_ref()
            .and_then(|r| r.bandit.as_ref())
            .map(|b| &b.embedding_provider)
            .filter(|p| !p.is_empty() && !known.contains(p.as_str()))
        {
            return Err(ConfigError::Validation(format!(
                "llm.router.bandit.embedding_provider = {:?} does not match any \
                 [[llm.providers]] entry",
                embed.as_str()
            )));
        }

        Ok(())
    }

    fn normalize_legacy_runtime_defaults(&mut self) {
        use crate::defaults::{
            default_debug_dir, default_log_file_path, default_skills_dir, default_sqlite_path,
            is_legacy_default_debug_dir, is_legacy_default_log_file, is_legacy_default_skills_path,
            is_legacy_default_sqlite_path,
        };

        if is_legacy_default_sqlite_path(&self.memory.sqlite_path) {
            self.memory.sqlite_path = default_sqlite_path();
        }

        for skill_path in &mut self.skills.paths {
            if is_legacy_default_skills_path(skill_path) {
                *skill_path = default_skills_dir();
            }
        }

        if is_legacy_default_debug_dir(&self.debug.output_dir) {
            self.debug.output_dir = default_debug_dir();
        }

        if is_legacy_default_log_file(&self.logging.file) {
            self.logging.file = default_log_file_path();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with_sct(threshold: f32) -> Config {
        let mut cfg = Config::default();
        cfg.llm.semantic_cache_threshold = threshold;
        cfg
    }

    #[test]
    fn semantic_cache_threshold_valid_zero() {
        assert!(config_with_sct(0.0).validate().is_ok());
    }

    #[test]
    fn semantic_cache_threshold_valid_mid() {
        assert!(config_with_sct(0.5).validate().is_ok());
    }

    #[test]
    fn semantic_cache_threshold_valid_one() {
        assert!(config_with_sct(1.0).validate().is_ok());
    }

    #[test]
    fn semantic_cache_threshold_invalid_negative() {
        let err = config_with_sct(-0.1).validate().unwrap_err();
        assert!(
            err.to_string().contains("semantic_cache_threshold"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn semantic_cache_threshold_invalid_above_one() {
        let err = config_with_sct(1.1).validate().unwrap_err();
        assert!(
            err.to_string().contains("semantic_cache_threshold"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn semantic_cache_threshold_invalid_nan() {
        let err = config_with_sct(f32::NAN).validate().unwrap_err();
        assert!(
            err.to_string().contains("semantic_cache_threshold"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn semantic_cache_threshold_invalid_infinity() {
        let err = config_with_sct(f32::INFINITY).validate().unwrap_err();
        assert!(
            err.to_string().contains("semantic_cache_threshold"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn semantic_cache_threshold_invalid_neg_infinity() {
        let err = config_with_sct(f32::NEG_INFINITY).validate().unwrap_err();
        assert!(
            err.to_string().contains("semantic_cache_threshold"),
            "unexpected error: {err}"
        );
    }

    fn probe_config(enabled: bool, threshold: f32, hard_fail_threshold: f32) -> Config {
        let mut cfg = Config::default();
        cfg.memory.compression.probe.enabled = enabled;
        cfg.memory.compression.probe.threshold = threshold;
        cfg.memory.compression.probe.hard_fail_threshold = hard_fail_threshold;
        cfg
    }

    #[test]
    fn probe_disabled_skips_validation() {
        // Invalid thresholds when probe is disabled must not cause errors.
        let cfg = probe_config(false, 0.0, 1.0);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn probe_valid_thresholds() {
        let cfg = probe_config(true, 0.6, 0.35);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn probe_threshold_zero_invalid() {
        let err = probe_config(true, 0.0, 0.0).validate().unwrap_err();
        assert!(
            err.to_string().contains("probe.threshold"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn probe_hard_fail_threshold_above_one_invalid() {
        let err = probe_config(true, 0.6, 1.0).validate().unwrap_err();
        assert!(
            err.to_string().contains("probe.hard_fail_threshold"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn probe_hard_fail_gte_threshold_invalid() {
        let err = probe_config(true, 0.3, 0.9).validate().unwrap_err();
        assert!(
            err.to_string().contains("probe.hard_fail_threshold"),
            "unexpected error: {err}"
        );
    }

    fn config_with_completeness_threshold(ct: f32) -> Config {
        let mut cfg = Config::default();
        cfg.orchestration.completeness_threshold = ct;
        cfg
    }

    #[test]
    fn completeness_threshold_valid_zero() {
        assert!(config_with_completeness_threshold(0.0).validate().is_ok());
    }

    #[test]
    fn completeness_threshold_valid_default() {
        assert!(config_with_completeness_threshold(0.7).validate().is_ok());
    }

    #[test]
    fn completeness_threshold_valid_one() {
        assert!(config_with_completeness_threshold(1.0).validate().is_ok());
    }

    #[test]
    fn completeness_threshold_invalid_negative() {
        let err = config_with_completeness_threshold(-0.1)
            .validate()
            .unwrap_err();
        assert!(
            err.to_string().contains("completeness_threshold"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn completeness_threshold_invalid_above_one() {
        let err = config_with_completeness_threshold(1.1)
            .validate()
            .unwrap_err();
        assert!(
            err.to_string().contains("completeness_threshold"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn completeness_threshold_invalid_nan() {
        let err = config_with_completeness_threshold(f32::NAN)
            .validate()
            .unwrap_err();
        assert!(
            err.to_string().contains("completeness_threshold"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn completeness_threshold_invalid_infinity() {
        let err = config_with_completeness_threshold(f32::INFINITY)
            .validate()
            .unwrap_err();
        assert!(
            err.to_string().contains("completeness_threshold"),
            "unexpected error: {err}"
        );
    }

    fn config_with_provider(name: &str) -> Config {
        let mut cfg = Config::default();
        cfg.llm.providers.push(crate::providers::ProviderEntry {
            provider_type: crate::providers::ProviderKind::Ollama,
            name: Some(name.into()),
            ..Default::default()
        });
        cfg
    }

    #[test]
    fn validate_provider_names_all_empty_ok() {
        let cfg = Config::default();
        assert!(cfg.validate_provider_names().is_ok());
    }

    #[test]
    fn validate_provider_names_matching_provider_ok() {
        let mut cfg = config_with_provider("fast");
        cfg.memory.admission.admission_provider = crate::providers::ProviderName::new("fast");
        assert!(cfg.validate_provider_names().is_ok());
    }

    #[test]
    fn validate_provider_names_unknown_provider_err() {
        let mut cfg = config_with_provider("fast");
        cfg.memory.admission.admission_provider =
            crate::providers::ProviderName::new("nonexistent");
        let err = cfg.validate_provider_names().unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("admission_provider") && msg.contains("nonexistent"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn validate_provider_names_triage_provider_none_ok() {
        let mut cfg = config_with_provider("fast");
        cfg.llm.complexity_routing = Some(crate::providers::ComplexityRoutingConfig {
            triage_provider: None,
            ..Default::default()
        });
        assert!(cfg.validate_provider_names().is_ok());
    }

    #[test]
    fn validate_provider_names_triage_provider_matching_ok() {
        let mut cfg = config_with_provider("fast");
        cfg.llm.complexity_routing = Some(crate::providers::ComplexityRoutingConfig {
            triage_provider: Some(crate::providers::ProviderName::new("fast")),
            ..Default::default()
        });
        assert!(cfg.validate_provider_names().is_ok());
    }

    #[test]
    fn validate_provider_names_triage_provider_unknown_err() {
        let mut cfg = config_with_provider("fast");
        cfg.llm.complexity_routing = Some(crate::providers::ComplexityRoutingConfig {
            triage_provider: Some(crate::providers::ProviderName::new("ghost")),
            ..Default::default()
        });
        let err = cfg.validate_provider_names().unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("triage_provider") && msg.contains("ghost"),
            "unexpected error: {msg}"
        );
    }

    // Regression test for issue #2599: TOML float values must deserialise without error
    // across all config sections that contain f32/f64 fields.
    #[test]
    fn toml_float_fields_deserialise_correctly() {
        let toml = r"
[llm.router.reputation]
enabled = true
decay_factor = 0.95
weight = 0.3

[llm.router.bandit]
enabled = false
cost_weight = 0.3
alpha = 1.0
decay_factor = 0.99

[skills]
disambiguation_threshold = 0.25
cosine_weight = 0.7
";
        // Wrap in a full Config to exercise the nested paths.
        let wrapped = format!(
            "{}\n{}",
            toml,
            r"[memory.semantic]
mmr_lambda = 0.7
"
        );
        // We only need the sub-structs to round-trip; build minimal wrappers.
        let router: crate::providers::RouterConfig = toml::from_str(
            r"[reputation]
enabled = true
decay_factor = 0.95
weight = 0.3
",
        )
        .expect("RouterConfig with float fields must deserialise");
        assert!((router.reputation.unwrap().decay_factor - 0.95).abs() < f64::EPSILON);

        let bandit: crate::providers::BanditConfig =
            toml::from_str("cost_weight = 0.3\nalpha = 1.0\n")
                .expect("BanditConfig with float fields must deserialise");
        assert!((bandit.cost_weight - 0.3_f32).abs() < f32::EPSILON);

        let semantic: crate::memory::SemanticConfig = toml::from_str("mmr_lambda = 0.7\n")
            .expect("SemanticConfig with float fields must deserialise");
        assert!((semantic.mmr_lambda - 0.7_f32).abs() < f32::EPSILON);

        let skills: crate::features::SkillsConfig =
            toml::from_str("disambiguation_threshold = 0.25\n")
                .expect("SkillsConfig with float fields must deserialise");
        assert!((skills.disambiguation_threshold - 0.25_f32).abs() < f32::EPSILON);

        let _ = wrapped; // silence unused-variable lint
    }

    #[test]
    fn focus_auto_consolidate_min_window_zero_rejected() {
        let mut cfg = Config::default();
        cfg.agent.focus.auto_consolidate_min_window = 0;
        let err = cfg.validate().unwrap_err().to_string();
        assert!(
            err.contains("auto_consolidate_min_window"),
            "expected auto_consolidate_min_window in error, got: {err}"
        );
    }

    #[test]
    fn focus_auto_consolidate_min_window_one_accepted() {
        let mut cfg = Config::default();
        cfg.agent.focus.auto_consolidate_min_window = 1;
        assert!(cfg.validate().is_ok());
    }
}
