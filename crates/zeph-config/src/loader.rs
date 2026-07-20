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
    #[must_use = "validation result must be checked"]
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
        self.validate_scheduler()?;
        self.acp
            .validate_auth_clients()
            .map_err(ConfigError::Validation)?;
        // Provider pool: empty pool, duplicate names, and multiple `default = true`
        // entries. Load-bearing guarantee relied on (verbatim) by
        // `Agent::resolve_pool_entry_provider` (tier_loop.rs) and `arise.rs` — both assume
        // a genuinely empty pool never occurs for a fully constructed production `Agent`.
        crate::providers::validate_pool(&self.llm.providers)?;
        self.llm.validate_stt()?;
        self.security
            .trajectory
            .validate()
            .map_err(ConfigError::Validation)?;
        self.gateway.validate().map_err(ConfigError::Validation)?;
        self.tools
            .utility
            .validate()
            .map_err(ConfigError::Validation)?;
        if let Some(fidelity) = &self.memory.fidelity {
            fidelity.validate().map_err(ConfigError::Validation)?;
        }
        self.memory
            .compression
            .acon
            .validate()
            .map_err(ConfigError::Validation)?;
        if self.memory.shadow_memory.enabled {
            self.memory
                .shadow_memory
                .validate()
                .map_err(ConfigError::Validation)?;
        }
        self.warn_insecure_qdrant_endpoint();
        Ok(())
    }

    /// Log a warning when `memory.qdrant_url` points at a non-loopback host without TLS or
    /// an API key configured (issue #6553).
    ///
    /// Deliberately non-fatal — repointing at a remote/managed Qdrant cluster without TLS or
    /// auth is a real deployment (e.g. an internal network already trusted by other means),
    /// so this warns instead of hard-failing like the bound checks in [`Self::validate`] above.
    /// Skipped entirely for loopback targets (`localhost`, `127.0.0.1`, `::1`): connecting to
    /// your own machine is definitionally not the plaintext-over-the-wire risk this guards
    /// against, matching the same carve-out `A2aClientConfig` documents for `--connect`.
    fn warn_insecure_qdrant_endpoint(&self) {
        let Ok(url) = url::Url::parse(&self.memory.qdrant_url) else {
            return;
        };
        let Some(host) = url.host_str() else {
            return;
        };
        if zeph_common::net::is_loopback_host(host) {
            return;
        }

        let has_tls = url.scheme().eq_ignore_ascii_case("https");
        let has_api_key = self
            .memory
            .qdrant_api_key
            .as_ref()
            .is_some_and(|k| !k.expose().trim().is_empty());

        if !has_tls || !has_api_key {
            tracing::warn!(
                qdrant_url = %self.memory.qdrant_url,
                tls = has_tls,
                api_key_configured = has_api_key,
                "memory.qdrant_url points at a non-loopback host without TLS and/or an API key \
                 configured — memory content would travel in plaintext with no server \
                 authentication; set qdrant_url to an https:// endpoint and configure \
                 memory.qdrant_api_key (vault key ZEPH_QDRANT_API_KEY) for remote/managed Qdrant"
            );
        }
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
        self.validate_a2a_client_trust()?;
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
        if self.worktree.max_worktrees == Some(0) {
            return Err(ConfigError::Validation(
                "worktree.max_worktrees must be > 0 or unset (unlimited); 0 would block all \
                 worktree creation"
                    .into(),
            ));
        }
        if self.worktree.disk_quota_mb == Some(0) {
            return Err(ConfigError::Validation(
                "worktree.disk_quota_mb must be > 0 or unset (no accounting); 0 would leave \
                 every non-empty worktree permanently over quota"
                    .into(),
            ));
        }
        if self.worktree.disk_quota_mb.is_some()
            && self.worktree.auto_reconcile_secs == 0
            && !self.worktree.reconcile_on_startup
        {
            return Err(ConfigError::Validation(
                "worktree.disk_quota_mb is set but neither reconcile_on_startup nor \
                 auto_reconcile_secs is enabled — the quota will only be checked when you run \
                 `zeph worktree list` manually, never automatically"
                    .into(),
            ));
        }
        if (1..60).contains(&self.worktree.auto_reconcile_secs) {
            return Err(ConfigError::Validation(format!(
                "worktree.auto_reconcile_secs must be 0 (disabled) or >= 60, got {}; a short \
                 interval runs a full filesystem walk in a tight loop",
                self.worktree.auto_reconcile_secs
            )));
        }
        Ok(())
    }

    /// Fail fast if `[a2a_client].card_trust_policy = "require"` is set without the
    /// `card-signing` feature compiled in anywhere in the binary (S3, #5928).
    ///
    /// Without this check, `require` would either silently degrade to no signature
    /// enforcement or brick all discovery, depending on how the unreachable code path is
    /// interpreted — both are worse than a loud config-load error. See
    /// `zeph_a2a::discovery::signature_severity` for the runtime-side half of this
    /// contract (treats `FeatureDisabled` the same as `Unverifiable`/`Invalid` under
    /// `require`, which only matters if this validation is ever bypassed).
    #[cfg_attr(
        feature = "card-signing",
        allow(clippy::unused_self, clippy::unnecessary_wraps)
    )]
    fn validate_a2a_client_trust(&self) -> Result<(), ConfigError> {
        #[cfg(not(feature = "card-signing"))]
        if self.a2a_client.card_trust_policy == crate::channels::CardTrustPolicy::Require {
            return Err(ConfigError::Validation(
                "a2a_client.card_trust_policy = require requires the card-signing feature \
                 to be enabled at build time (see the `a2a` feature in the root Cargo.toml)"
                    .into(),
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
        if self.orchestration.max_parallel == 0 {
            return Err(ConfigError::Validation(
                "orchestration.max_parallel must be > 0".into(),
            ));
        }
        if self.orchestration.max_tasks == 0 {
            return Err(ConfigError::Validation(
                "orchestration.max_tasks must be > 0".into(),
            ));
        }
        let ct = self.orchestration.completeness_threshold;
        if !ct.is_finite() || !(0.0..=1.0).contains(&ct) {
            return Err(ConfigError::Validation(format!(
                "orchestration.completeness_threshold must be in [0.0, 1.0], got {ct}"
            )));
        }
        // Ensemble member-list shape is only meaningful once the ensemble is actually wired
        // into a verification decision (`enabled && verify`) — an unused/staged config with
        // an invalid `members` list must not block startup (spec 073 FR-014).
        let ensemble = &self.orchestration.ensemble;
        if ensemble.enabled && ensemble.verify {
            let n = ensemble.members.len();
            if n.is_multiple_of(2) || n < 3 {
                return Err(ConfigError::Validation(format!(
                    "orchestration.ensemble.members must be odd and >= 3, got {n}"
                )));
            }
            let unique: std::collections::HashSet<&str> =
                ensemble.members.iter().map(String::as_str).collect();
            if unique.len() != ensemble.members.len() {
                return Err(ConfigError::Validation(
                    "orchestration.ensemble.members contains a duplicate provider name".into(),
                ));
            }
            // Defense-in-depth (security P3): EnsembleTracker's EMA params are telemetry-only
            // in PR-1 and never gate a verification/dispatch decision, but an out-of-range value
            // would still produce a meaningless displayed score and could bias a future phase
            // that wires EMA into member selection.
            let alpha = ensemble.ema_alpha;
            if !alpha.is_finite() || !(0.0..=1.0).contains(&alpha) {
                return Err(ConfigError::Validation(format!(
                    "orchestration.ensemble.ema_alpha must be in [0.0, 1.0], got {alpha}"
                )));
            }
            let decay = ensemble.ema_decay;
            if !decay.is_finite() || !(0.0..=1.0).contains(&decay) {
                return Err(ConfigError::Validation(format!(
                    "orchestration.ensemble.ema_decay must be in [0.0, 1.0], got {decay}"
                )));
            }
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
        if self.orchestration.aggregator_timeout_secs == 0 {
            return Err(ConfigError::Validation(
                "orchestration.aggregator_timeout_secs must be > 0".into(),
            ));
        }
        if self.orchestration.planner_timeout_secs == 0 {
            return Err(ConfigError::Validation(
                "orchestration.planner_timeout_secs must be > 0".into(),
            ));
        }
        if self.orchestration.verifier_timeout_secs == 0 {
            return Err(ConfigError::Validation(
                "orchestration.verifier_timeout_secs must be > 0".into(),
            ));
        }
        if self.orchestration.default_idle_timeout_secs == Some(0) {
            return Err(ConfigError::Validation(
                "orchestration.default_idle_timeout_secs must be > 0 or unset; 0 would mean \
                 an instant idle timeout"
                    .into(),
            ));
        }
        self.validate_command_handoff()?;
        Ok(())
    }

    /// Validate `[orchestration.command]` (spec-080, GitHub #6363): `max_handoffs` bounds
    /// and, when the feature is enabled, its cross-crate prerequisites.
    fn validate_command_handoff(&self) -> Result<(), ConfigError> {
        if self.orchestration.command.max_handoffs == 0 {
            return Err(ConfigError::Validation(
                "orchestration.command.max_handoffs must be > 0; set \
                 orchestration.command.enabled = false to disable Command handoff instead"
                    .into(),
            ));
        }
        // SEC-2: an operator-set max_handoffs with no upper sanity bound defeats the
        // livelock counter's purpose as a footgun guard (topology + forward-only still
        // terminate a graph regardless, so this is not itself an exploitable hole — see
        // the security audit handoff — but an unbounded value is never a deliberate,
        // reasonable config).
        if self.orchestration.command.max_handoffs > 10_000 {
            return Err(ConfigError::Validation(format!(
                "orchestration.command.max_handoffs must be <= 10000, got {}",
                self.orchestration.command.max_handoffs
            )));
        }
        // Deviation #4 / SEC-1: reject at config-validation time rather than discovering
        // the misconfiguration per-task at runtime write-attempt time (spec-080 §7 edge
        // case table; critic P2 confirmed the runtime-only check wastes real LLM-call
        // work on non-handoff tasks in the same misconfigured graph before the first
        // handoff attempt fails). Command handoff's produce-side seam
        // (`determine_task_outcome`, zeph-core) has nowhere to persist `update` without
        // the store, and its FR-B-003 sanitizer scan becomes a silent no-op without
        // content isolation (`ContentSanitizer::sanitize` early-returns with empty
        // `injection_flags` when `enabled = false`, and `flag_injection_patterns = false`
        // has the same effect) — both are genuine security/correctness prerequisites of
        // this feature, not merely convenient defaults.
        if !self.orchestration.command.enabled {
            return Ok(());
        }
        if !self.memory.store.enabled {
            return Err(ConfigError::Validation(
                "orchestration.command.enabled = true requires memory.store.enabled = \
                 true — Command handoff has nowhere to persist its `update` payload \
                 without the cross-thread store"
                    .into(),
            ));
        }
        if !self.security.content_isolation.enabled {
            return Err(ConfigError::Validation(
                "orchestration.command.enabled = true requires \
                 security.content_isolation.enabled = true — the FR-B-003 sanitizer \
                 scan that gates a Command handoff before it drives routing or a \
                 store write becomes a silent no-op otherwise"
                    .into(),
            ));
        }
        if !self.security.content_isolation.flag_injection_patterns {
            return Err(ConfigError::Validation(
                "orchestration.command.enabled = true requires \
                 security.content_isolation.flag_injection_patterns = true — the \
                 FR-B-003 sanitizer scan never flags anything otherwise, silently \
                 bypassing the reject gate"
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
        self.skills
            .learning
            .validate()
            .map_err(ConfigError::Validation)?;
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

    /// Validate that each `[[scheduler.tasks]]` entry has exactly one of `cron` or `run_at` set.
    fn validate_scheduler(&self) -> Result<(), ConfigError> {
        for task in &self.scheduler.tasks {
            match (&task.cron, &task.run_at) {
                (Some(_), Some(_)) => {
                    return Err(ConfigError::Validation(format!(
                        "scheduler task {:?}: only one of `cron` or `run_at` may be set, not both",
                        task.name
                    )));
                }
                (None, None) => {
                    return Err(ConfigError::Validation(format!(
                        "scheduler task {:?}: either `cron` or `run_at` must be set",
                        task.name
                    )));
                }
                _ => {}
            }
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
    /// The field table lists all subsystem provider references. Each non-empty value must
    /// match a name in `known`.
    fn validate_named_provider_refs(
        &self,
        known: &std::collections::HashSet<String>,
    ) -> Result<(), ConfigError> {
        self.validate_core_provider_refs(known)?;
        self.validate_tool_and_quality_provider_refs(known)
    }

    fn validate_core_provider_refs(
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
        Self::check_provider_refs(fields, known)
    }

    fn validate_tool_and_quality_provider_refs(
        &self,
        known: &std::collections::HashSet<String>,
    ) -> Result<(), ConfigError> {
        let fields: &[(&str, &crate::providers::ProviderName)] = &[
            (
                "security.shadow_sentinel.probe_provider",
                &self.security.shadow_sentinel.probe_provider,
            ),
            (
                "tools.retry.parameter_reformat_provider",
                &self.tools.retry.parameter_reformat_provider,
            ),
            (
                "tools.policy.policy_provider",
                &self.tools.policy.policy_provider,
            ),
            (
                "tools.adversarial_policy.policy_provider",
                &self.tools.adversarial_policy.policy_provider,
            ),
            (
                "tools.speculative.pattern.rerank_provider",
                &self.tools.speculative.pattern.rerank_provider,
            ),
            (
                "tools.compression.evolution_provider",
                &self.tools.compression.evolution_provider,
            ),
            ("quality.proposer_provider", &self.quality.proposer_provider),
            ("quality.checker_provider", &self.quality.checker_provider),
        ];
        Self::check_provider_refs(fields, known)
    }

    fn check_provider_refs(
        fields: &[(&str, &crate::providers::ProviderName)],
        known: &std::collections::HashSet<String>,
    ) -> Result<(), ConfigError> {
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

    #[cfg(not(feature = "card-signing"))]
    #[test]
    fn card_trust_policy_require_without_feature_fails_validation() {
        let mut cfg = Config::default();
        cfg.a2a_client.card_trust_policy = crate::channels::CardTrustPolicy::Require;
        let err = cfg.validate().unwrap_err();
        assert!(
            err.to_string().contains("card_trust_policy"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn card_trust_policy_ignore_and_prefer_always_pass_validation() {
        let mut cfg = Config::default();
        cfg.a2a_client.card_trust_policy = crate::channels::CardTrustPolicy::Ignore;
        assert!(cfg.validate().is_ok());
        cfg.a2a_client.card_trust_policy = crate::channels::CardTrustPolicy::Prefer;
        assert!(cfg.validate().is_ok());
    }

    #[cfg(feature = "card-signing")]
    #[test]
    fn card_trust_policy_require_with_feature_passes_validation() {
        let mut cfg = Config::default();
        cfg.a2a_client.card_trust_policy = crate::channels::CardTrustPolicy::Require;
        assert!(cfg.validate().is_ok());
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
    fn validate_max_parallel_zero_rejected() {
        let mut cfg = Config::default();
        cfg.orchestration.max_parallel = 0;
        let err = cfg.validate().unwrap_err().to_string();
        assert!(
            err.contains("max_parallel"),
            "expected max_parallel in error, got: {err}"
        );
    }

    #[test]
    fn validate_max_parallel_one_accepted() {
        let mut cfg = Config::default();
        cfg.orchestration.max_parallel = 1;
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn validate_max_tasks_zero_rejected() {
        let mut cfg = Config::default();
        cfg.orchestration.max_tasks = 0;
        let err = cfg.validate().unwrap_err().to_string();
        assert!(
            err.contains("max_tasks"),
            "expected max_tasks in error, got: {err}"
        );
    }

    #[test]
    fn validate_max_tasks_one_accepted() {
        let mut cfg = Config::default();
        cfg.orchestration.max_tasks = 1;
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn validate_aggregator_timeout_zero_rejected() {
        let mut cfg = Config::default();
        cfg.orchestration.aggregator_timeout_secs = 0;
        let err = cfg.validate().unwrap_err().to_string();
        assert!(
            err.contains("aggregator_timeout_secs"),
            "expected aggregator_timeout_secs in error, got: {err}"
        );
    }

    #[test]
    fn validate_planner_timeout_zero_rejected() {
        let mut cfg = Config::default();
        cfg.orchestration.planner_timeout_secs = 0;
        let err = cfg.validate().unwrap_err().to_string();
        assert!(
            err.contains("planner_timeout_secs"),
            "expected planner_timeout_secs in error, got: {err}"
        );
    }

    #[test]
    fn validate_verifier_timeout_zero_rejected() {
        let mut cfg = Config::default();
        cfg.orchestration.verifier_timeout_secs = 0;
        let err = cfg.validate().unwrap_err().to_string();
        assert!(
            err.contains("verifier_timeout_secs"),
            "expected verifier_timeout_secs in error, got: {err}"
        );
    }

    #[test]
    fn validate_default_idle_timeout_zero_rejected() {
        let mut cfg = Config::default();
        cfg.orchestration.default_idle_timeout_secs = Some(0);
        let err = cfg.validate().unwrap_err().to_string();
        assert!(
            err.contains("default_idle_timeout_secs"),
            "expected default_idle_timeout_secs in error, got: {err}"
        );
    }

    #[test]
    fn validate_default_idle_timeout_none_accepted() {
        let mut cfg = Config::default();
        cfg.orchestration.default_idle_timeout_secs = None;
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn validate_default_idle_timeout_positive_accepted() {
        let mut cfg = Config::default();
        cfg.orchestration.default_idle_timeout_secs = Some(60);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn validate_command_max_handoffs_zero_rejected() {
        let mut cfg = Config::default();
        cfg.orchestration.command.max_handoffs = 0;
        let err = cfg.validate().unwrap_err().to_string();
        assert!(
            err.contains("max_handoffs"),
            "expected max_handoffs in error, got: {err}"
        );
    }

    #[test]
    fn validate_command_max_handoffs_default_accepted() {
        let cfg = Config::default();
        assert_eq!(cfg.orchestration.command.max_handoffs, 16);
        assert!(!cfg.orchestration.command.enabled);
        assert!(cfg.validate().is_ok());
    }

    // --- SEC-2: max_handoffs upper sanity bound ---

    #[test]
    fn validate_command_max_handoffs_over_10000_rejected() {
        let mut cfg = Config::default();
        cfg.orchestration.command.max_handoffs = 10_001;
        let err = cfg.validate().unwrap_err().to_string();
        assert!(
            err.contains("max_handoffs") && err.contains("<= 10000"),
            "expected max_handoffs upper-bound error, got: {err}"
        );
    }

    #[test]
    fn validate_command_max_handoffs_exactly_10000_accepted() {
        let mut cfg = Config::default();
        cfg.orchestration.command.max_handoffs = 10_000;
        assert!(cfg.validate().is_ok());
    }

    // --- Deviation #4 / SEC-1: command.enabled requires store.enabled + content_isolation ---

    fn config_with_command_enabled() -> Config {
        let mut cfg = Config::default();
        cfg.orchestration.command.enabled = true;
        cfg.memory.store.enabled = true;
        cfg.security.content_isolation.enabled = true;
        cfg.security.content_isolation.flag_injection_patterns = true;
        cfg
    }

    #[test]
    fn validate_command_enabled_with_all_prerequisites_accepted() {
        assert!(config_with_command_enabled().validate().is_ok());
    }

    #[test]
    fn validate_command_enabled_without_store_enabled_rejected() {
        let mut cfg = config_with_command_enabled();
        cfg.memory.store.enabled = false;
        let err = cfg.validate().unwrap_err().to_string();
        assert!(
            err.contains("memory.store.enabled"),
            "expected store-prerequisite error, got: {err}"
        );
    }

    #[test]
    fn validate_command_enabled_without_content_isolation_enabled_rejected() {
        let mut cfg = config_with_command_enabled();
        cfg.security.content_isolation.enabled = false;
        let err = cfg.validate().unwrap_err().to_string();
        assert!(
            err.contains("content_isolation.enabled"),
            "expected content_isolation-prerequisite error, got: {err}"
        );
    }

    #[test]
    fn validate_command_enabled_without_flag_injection_patterns_rejected() {
        let mut cfg = config_with_command_enabled();
        cfg.security.content_isolation.flag_injection_patterns = false;
        let err = cfg.validate().unwrap_err().to_string();
        assert!(
            err.contains("flag_injection_patterns"),
            "expected flag_injection_patterns-prerequisite error, got: {err}"
        );
    }

    #[test]
    fn validate_command_disabled_ignores_store_and_content_isolation_state() {
        // command.enabled = false (default): misconfigured store/content_isolation must
        // not block startup — the prerequisites only apply once the feature is opted in.
        let mut cfg = Config::default();
        cfg.memory.store.enabled = false;
        cfg.security.content_isolation.enabled = false;
        cfg.security.content_isolation.flag_injection_patterns = false;
        assert!(cfg.validate().is_ok());
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

    fn task_with(cron: Option<&str>, run_at: Option<&str>) -> crate::features::ScheduledTaskConfig {
        crate::features::ScheduledTaskConfig {
            name: "test-task".into(),
            cron: cron.map(Into::into),
            run_at: run_at.map(Into::into),
            kind: crate::features::ScheduledTaskKind::HealthCheck,
            config: serde_json::Value::Null,
        }
    }

    #[test]
    fn scheduler_task_valid_cron() {
        let mut cfg = Config::default();
        cfg.scheduler.tasks.push(task_with(Some("0 9 * * *"), None));
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn scheduler_task_valid_run_at() {
        let mut cfg = Config::default();
        cfg.scheduler
            .tasks
            .push(task_with(None, Some("2025-01-01T09:00:00Z")));
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn scheduler_task_neither_cron_nor_run_at_rejected() {
        let mut cfg = Config::default();
        cfg.scheduler.tasks.push(task_with(None, None));
        let err = cfg.validate().unwrap_err().to_string();
        assert!(
            err.contains("either `cron` or `run_at` must be set"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn scheduler_task_both_cron_and_run_at_rejected() {
        let mut cfg = Config::default();
        cfg.scheduler
            .tasks
            .push(task_with(Some("0 9 * * *"), Some("2025-01-01T09:00:00Z")));
        let err = cfg.validate().unwrap_err().to_string();
        assert!(
            err.contains("only one of `cron` or `run_at` may be set"),
            "unexpected error: {err}"
        );
    }

    // ── #5932: 7 previously-dead validate() functions now wired into Config::validate() ──────

    #[test]
    fn validate_rejects_empty_provider_pool() {
        // This is the most severe gap from #5932: `validate_pool` was documented (verbatim)
        // as a load-bearing guarantee by tier_loop.rs/arise.rs but was never actually wired
        // in. `Config::default()` itself now seeds one provider (critic S1 follow-up, so
        // `--dump-config-defaults` output stays self-consistent) — clear it explicitly to
        // exercise the empty-pool branch.
        let mut cfg = Config::default();
        cfg.llm.providers.clear();
        let err = cfg.validate().unwrap_err().to_string();
        assert!(
            err.contains("at least one LLM provider"),
            "expected empty-pool error, got: {err}"
        );
    }

    #[test]
    fn validate_rejects_duplicate_provider_names() {
        let mut cfg = Config::default();
        cfg.llm.providers.push(crate::providers::ProviderEntry {
            provider_type: crate::providers::ProviderKind::Ollama,
            ..Default::default()
        });
        let err = cfg.validate().unwrap_err().to_string();
        assert!(
            err.contains("duplicate provider name"),
            "expected duplicate-name error, got: {err}"
        );
    }

    #[test]
    fn validate_rejects_multiple_default_providers() {
        let mut cfg = Config::default();
        cfg.llm.providers[0].default = true;
        cfg.llm.providers.push(crate::providers::ProviderEntry {
            provider_type: crate::providers::ProviderKind::Ollama,
            name: Some("second".into()),
            default: true,
            ..Default::default()
        });
        let err = cfg.validate().unwrap_err().to_string();
        assert!(
            err.contains("default = true"),
            "expected multiple-default error, got: {err}"
        );
    }

    #[test]
    fn validate_rejects_stt_provider_pointing_at_nonexistent_provider() {
        let mut cfg = Config::default();
        cfg.llm.stt = Some(crate::providers::SttConfig {
            provider: crate::providers::ProviderName::new("ghost"),
            language: crate::providers::default_stt_language(),
        });
        let err = cfg.validate().unwrap_err().to_string();
        assert!(
            err.contains("[llm.stt].provider") && err.contains("ghost"),
            "expected stt-provider-mismatch error, got: {err}"
        );
    }

    #[test]
    fn validate_accepts_stt_provider_matching_existing_provider() {
        let mut cfg = Config::default();
        cfg.llm.stt = Some(crate::providers::SttConfig {
            provider: crate::providers::ProviderName::new("ollama"),
            language: crate::providers::default_stt_language(),
        });
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn validate_rejects_trajectory_sentinel_inverted_thresholds() {
        let mut cfg = Config::default();
        cfg.security.trajectory.elevated_at = 0.9;
        cfg.security.trajectory.high_at = 0.5;
        let err = cfg.validate().unwrap_err().to_string();
        assert!(
            err.contains("elevated_at") && err.contains("high_at"),
            "expected trajectory threshold-ordering error, got: {err}"
        );
    }

    #[test]
    fn validate_rejects_gateway_invalid_webhook_timeout() {
        // `rate_limit` and `max_body_size` are already covered by `validate_scalar_bounds`
        // (runs earlier in the pipeline), so testing those wouldn't prove
        // `GatewayConfig::validate()` is actually wired in — it would pass identically on
        // pre-#5932 code (critic-flagged shadowing, S2). `webhook_send_timeout_secs` is the
        // one field uniquely reachable only through the new call.
        let mut cfg = Config::default();
        cfg.gateway.webhook_send_timeout_secs = 0;
        let err = cfg.validate().unwrap_err().to_string();
        assert!(
            err.contains("webhook_send_timeout_secs"),
            "expected gateway webhook_send_timeout_secs error, got: {err}"
        );
    }

    #[test]
    fn validate_rejects_negative_utility_scoring_weight() {
        let mut cfg = Config::default();
        cfg.tools.utility.gain_weight = -1.0;
        let err = cfg.validate().unwrap_err().to_string();
        assert!(
            err.contains("gain_weight"),
            "expected utility-scoring weight error, got: {err}"
        );
    }

    #[test]
    fn validate_rejects_fidelity_threshold_ordering() {
        let mut cfg = Config::default();
        cfg.memory.fidelity = Some(crate::fidelity::FidelityConfig {
            full_threshold: 0.2,
            compressed_threshold: 0.5,
            ..Default::default()
        });
        let err = cfg.validate().unwrap_err().to_string();
        assert!(
            err.contains("full_threshold") && err.contains("compressed_threshold"),
            "expected fidelity threshold-ordering error, got: {err}"
        );
    }

    #[test]
    fn validate_accepts_absent_fidelity_config() {
        let cfg = Config::default();
        assert!(cfg.memory.fidelity.is_none());
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn validate_rejects_acon_inverted_thresholds() {
        let mut cfg = Config::default();
        cfg.memory.compression.acon.passthrough_threshold = 5000;
        cfg.memory.compression.acon.summarize_threshold = 1000;
        let err = cfg.validate().unwrap_err().to_string();
        assert!(
            err.contains("passthrough_threshold") && err.contains("summarize_threshold"),
            "expected acon threshold-ordering error, got: {err}"
        );
    }

    #[test]
    fn validate_rejects_shadow_memory_inverted_thresholds() {
        let mut cfg = Config::default();
        cfg.memory.shadow_memory.enabled = true;
        cfg.memory.shadow_memory.escalation_threshold = 0.75;
        cfg.memory.shadow_memory.risk_threshold = 0.50;
        let err = cfg.validate().unwrap_err().to_string();
        assert!(
            err.contains("escalation_threshold") && err.contains("risk_threshold"),
            "expected shadow_memory threshold-ordering error, got: {err}"
        );
    }

    #[test]
    fn validate_rejects_shadow_memory_equal_thresholds() {
        let mut cfg = Config::default();
        cfg.memory.shadow_memory.enabled = true;
        cfg.memory.shadow_memory.escalation_threshold = 0.6;
        cfg.memory.shadow_memory.risk_threshold = 0.6;
        assert!(
            cfg.validate().is_err(),
            "equal thresholds must be rejected — the escalation band would be empty"
        );
    }

    #[test]
    fn validate_ignores_shadow_memory_thresholds_when_disabled() {
        let mut cfg = Config::default();
        cfg.memory.shadow_memory.enabled = false;
        cfg.memory.shadow_memory.escalation_threshold = 0.9;
        cfg.memory.shadow_memory.risk_threshold = 0.1;
        assert!(
            cfg.validate().is_ok(),
            "inverted thresholds on a disabled shadow_memory config must not fail validation"
        );
    }

    #[test]
    fn validate_rejects_worktree_max_worktrees_zero() {
        let mut cfg = Config::default();
        cfg.worktree.max_worktrees = Some(0);
        let err = cfg.validate().unwrap_err().to_string();
        assert!(
            err.contains("max_worktrees"),
            "expected max_worktrees in error, got: {err}"
        );
    }

    #[test]
    fn validate_accepts_worktree_max_worktrees_positive_or_unset() {
        let mut cfg = Config::default();
        cfg.worktree.max_worktrees = Some(1);
        assert!(cfg.validate().is_ok());
        cfg.worktree.max_worktrees = None;
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn validate_rejects_worktree_disk_quota_mb_zero() {
        let mut cfg = Config::default();
        cfg.worktree.disk_quota_mb = Some(0);
        let err = cfg.validate().unwrap_err().to_string();
        assert!(
            err.contains("disk_quota_mb"),
            "expected disk_quota_mb in error, got: {err}"
        );
    }

    #[test]
    fn validate_accepts_worktree_disk_quota_mb_positive_or_unset() {
        let mut cfg = Config::default();
        cfg.worktree.disk_quota_mb = Some(1);
        assert!(cfg.validate().is_ok());
        cfg.worktree.disk_quota_mb = None;
        assert!(cfg.validate().is_ok());
    }

    /// Review N1 / critic M1(b): `disk_quota_mb` set with neither the startup sweep nor the
    /// periodic sweep enabled means the quota is evaluated nowhere automatically — must be a
    /// hard config error, not a silent no-op.
    #[test]
    fn validate_rejects_worktree_disk_quota_mb_with_no_evaluation_path_enabled() {
        let mut cfg = Config::default();
        cfg.worktree.disk_quota_mb = Some(100);
        cfg.worktree.auto_reconcile_secs = 0;
        cfg.worktree.reconcile_on_startup = false;
        let err = cfg.validate().unwrap_err().to_string();
        assert!(
            err.contains("disk_quota_mb") && err.contains("never automatically"),
            "expected inert-path error, got: {err}"
        );
    }

    #[test]
    fn validate_accepts_worktree_disk_quota_mb_when_startup_sweep_enabled() {
        let mut cfg = Config::default();
        cfg.worktree.disk_quota_mb = Some(100);
        cfg.worktree.auto_reconcile_secs = 0;
        cfg.worktree.reconcile_on_startup = true;
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn validate_accepts_worktree_disk_quota_mb_when_periodic_sweep_enabled() {
        let mut cfg = Config::default();
        cfg.worktree.disk_quota_mb = Some(100);
        cfg.worktree.auto_reconcile_secs = 3600;
        cfg.worktree.reconcile_on_startup = false;
        assert!(cfg.validate().is_ok());
    }

    /// Review perf#3: a short `auto_reconcile_secs` runs a full filesystem walk in a tight
    /// loop — must be rejected, matching the `Some(0)` rejection style for the sibling fields.
    #[test]
    fn validate_rejects_worktree_auto_reconcile_secs_short_interval() {
        let mut cfg = Config::default();
        cfg.worktree.auto_reconcile_secs = 1;
        let err = cfg.validate().unwrap_err().to_string();
        assert!(
            err.contains("auto_reconcile_secs"),
            "expected auto_reconcile_secs in error, got: {err}"
        );
    }

    #[test]
    fn validate_accepts_worktree_auto_reconcile_secs_zero_or_at_least_60() {
        let mut cfg = Config::default();
        cfg.worktree.auto_reconcile_secs = 0;
        assert!(cfg.validate().is_ok());
        cfg.worktree.auto_reconcile_secs = 60;
        assert!(cfg.validate().is_ok());
        cfg.worktree.auto_reconcile_secs = 3600;
        assert!(cfg.validate().is_ok());
    }

    /// Regression test (critic S1): `Config::default()` must itself satisfy `validate_pool`
    /// so `--dump-config-defaults` (which serializes `Config::default()` verbatim,
    /// `src/runner.rs`) emits a config that `zeph --config <dump>` can actually load and
    /// validate, rather than a self-inconsistent onboarding trap.
    #[test]
    fn dump_defaults_output_is_self_consistent_and_validates() {
        assert!(Config::default().validate().is_ok());

        let dumped = Config::dump_defaults().expect("dump defaults");
        assert!(
            dumped.contains("[[llm.providers]]"),
            "dumped defaults must include an active provider entry, got:\n{dumped}"
        );
        let reparsed: Config = toml::from_str(&dumped).expect("reparse dumped defaults");
        assert!(reparsed.validate().is_ok());
    }

    // --- orchestration.ensemble validation (spec 073-orch-ensemble-merge, M5/M7) ---

    fn config_with_ensemble(enabled: bool, verify: bool, members: Vec<&str>) -> Config {
        let mut cfg = Config::default();
        cfg.orchestration.ensemble.enabled = enabled;
        cfg.orchestration.ensemble.verify = verify;
        cfg.orchestration.ensemble.members = members.into_iter().map(String::from).collect();
        cfg
    }

    #[test]
    fn ensemble_default_config_validates_trivially() {
        assert!(Config::default().validate().is_ok());
    }

    #[test]
    fn ensemble_disabled_skips_member_list_validation() {
        // enabled=false: an invalid members list must not block startup.
        let cfg = config_with_ensemble(false, false, vec!["a", "b"]);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn ensemble_enabled_but_not_verify_skips_member_list_validation() {
        // enabled=true, verify=false: still an unused/staged config, checks skipped.
        let cfg = config_with_ensemble(true, false, vec!["a", "b"]);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn ensemble_active_even_length_members_rejected() {
        let cfg = config_with_ensemble(true, true, vec!["a", "b"]);
        let err = cfg.validate().unwrap_err();
        assert!(
            err.to_string().contains("must be odd and >= 3"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn ensemble_active_short_members_rejected() {
        let cfg = config_with_ensemble(true, true, vec!["a"]);
        let err = cfg.validate().unwrap_err();
        assert!(
            err.to_string().contains("must be odd and >= 3"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn ensemble_active_duplicate_members_rejected() {
        let cfg = config_with_ensemble(true, true, vec!["a", "b", "a"]);
        let err = cfg.validate().unwrap_err();
        assert!(
            err.to_string().contains("duplicate provider name"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn ensemble_active_valid_odd_unique_members_accepted() {
        let cfg = config_with_ensemble(true, true, vec!["a", "b", "c"]);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn ensemble_active_valid_five_members_accepted() {
        let cfg = config_with_ensemble(true, true, vec!["a", "b", "c", "d", "e"]);
        assert!(cfg.validate().is_ok());
    }

    // --- ema_alpha / ema_decay range validation (security P3) ---

    #[test]
    fn ensemble_active_ema_alpha_above_one_rejected() {
        let mut cfg = config_with_ensemble(true, true, vec!["a", "b", "c"]);
        cfg.orchestration.ensemble.ema_alpha = 1.5;
        let err = cfg.validate().unwrap_err();
        assert!(
            err.to_string().contains("ema_alpha"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn ensemble_active_ema_alpha_negative_rejected() {
        let mut cfg = config_with_ensemble(true, true, vec!["a", "b", "c"]);
        cfg.orchestration.ensemble.ema_alpha = -0.1;
        let err = cfg.validate().unwrap_err();
        assert!(
            err.to_string().contains("ema_alpha"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn ensemble_active_ema_alpha_nan_rejected() {
        let mut cfg = config_with_ensemble(true, true, vec!["a", "b", "c"]);
        cfg.orchestration.ensemble.ema_alpha = f64::NAN;
        let err = cfg.validate().unwrap_err();
        assert!(
            err.to_string().contains("ema_alpha"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn ensemble_active_ema_decay_above_one_rejected() {
        let mut cfg = config_with_ensemble(true, true, vec!["a", "b", "c"]);
        cfg.orchestration.ensemble.ema_decay = 1.1;
        let err = cfg.validate().unwrap_err();
        assert!(
            err.to_string().contains("ema_decay"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn ensemble_active_ema_decay_negative_rejected() {
        let mut cfg = config_with_ensemble(true, true, vec!["a", "b", "c"]);
        cfg.orchestration.ensemble.ema_decay = -0.1;
        let err = cfg.validate().unwrap_err();
        assert!(
            err.to_string().contains("ema_decay"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn ensemble_active_ema_boundaries_zero_and_one_accepted() {
        let mut cfg = config_with_ensemble(true, true, vec!["a", "b", "c"]);
        cfg.orchestration.ensemble.ema_alpha = 0.0;
        cfg.orchestration.ensemble.ema_decay = 1.0;
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn ensemble_disabled_skips_ema_range_validation() {
        // enabled=false: an out-of-range EMA param must not block startup.
        let mut cfg = config_with_ensemble(false, false, vec![]);
        cfg.orchestration.ensemble.ema_alpha = 5.0;
        assert!(cfg.validate().is_ok());
    }

    // ── warn_insecure_qdrant_endpoint (issue #6553) ───────────────────────────

    #[test]
    #[tracing_test::traced_test]
    fn qdrant_loopback_url_never_warns() {
        let mut cfg = Config::default();
        cfg.memory.qdrant_url = "http://localhost:6334".into();
        assert!(cfg.validate().is_ok());
        assert!(!logs_contain("memory.qdrant_url"));
    }

    #[test]
    #[tracing_test::traced_test]
    fn qdrant_non_loopback_plaintext_no_key_warns() {
        let mut cfg = Config::default();
        cfg.memory.qdrant_url = "http://qdrant.example.com:6334".into();
        assert!(cfg.validate().is_ok(), "must warn, not hard-fail");
        assert!(logs_contain("memory.qdrant_url"));
    }

    #[test]
    #[tracing_test::traced_test]
    fn qdrant_non_loopback_https_with_key_does_not_warn() {
        let mut cfg = Config::default();
        cfg.memory.qdrant_url = "https://qdrant.example.com:6334".into();
        cfg.memory.qdrant_api_key = Some(zeph_common::secret::Secret::new("test-key"));
        assert!(cfg.validate().is_ok());
        assert!(!logs_contain("memory.qdrant_url"));
    }

    #[test]
    #[tracing_test::traced_test]
    fn qdrant_non_loopback_https_without_key_still_warns() {
        let mut cfg = Config::default();
        cfg.memory.qdrant_url = "https://qdrant.example.com:6334".into();
        assert!(cfg.validate().is_ok());
        assert!(logs_contain("memory.qdrant_url"));
    }

    #[test]
    #[tracing_test::traced_test]
    fn qdrant_non_loopback_plaintext_with_key_still_warns() {
        // TLS is still required even with an API key — a key sent over plaintext HTTP is
        // itself exposed on the wire.
        let mut cfg = Config::default();
        cfg.memory.qdrant_url = "http://qdrant.example.com:6334".into();
        cfg.memory.qdrant_api_key = Some(zeph_common::secret::Secret::new("test-key"));
        assert!(cfg.validate().is_ok());
        assert!(logs_contain("memory.qdrant_url"));
    }
}
