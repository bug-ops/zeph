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

    /// Validate configuration values are within sane bounds.
    ///
    /// # Errors
    ///
    /// Returns an error if any value is out of range.
    #[allow(clippy::too_many_lines)]
    pub fn validate(&self) -> Result<(), ConfigError> {
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
        // MCP server validation
        {
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
        }

        self.experiments
            .validate()
            .map_err(ConfigError::Validation)?;

        if self.orchestration.plan_cache.enabled {
            self.orchestration
                .plan_cache
                .validate()
                .map_err(ConfigError::Validation)?;
        }

        let ct = self.orchestration.completeness_threshold;
        if !ct.is_finite() || !(0.0..=1.0).contains(&ct) {
            return Err(ConfigError::Validation(format!(
                "orchestration.completeness_threshold must be in [0.0, 1.0], got {ct}"
            )));
        }

        // Focus config validation
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

        // SideQuest config validation
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

        let sct = self.llm.semantic_cache_threshold;
        if !(sct.is_finite() && (0.0..=1.0).contains(&sct)) {
            return Err(ConfigError::Validation(format!(
                "llm.semantic_cache_threshold must be in [0.0, 1.0], got {sct} \
                 (override via ZEPH_LLM_SEMANTIC_CACHE_THRESHOLD env var)"
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
}
