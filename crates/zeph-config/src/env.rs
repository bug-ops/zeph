// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::providers::{SttConfig, default_stt_language, default_stt_model, default_stt_provider};
use crate::root::Config;

impl Config {
    pub fn apply_env_overrides(&mut self) {
        self.apply_env_overrides_core();
        self.apply_env_overrides_security();
    }

    fn apply_env_overrides_core(&mut self) {
        self.apply_env_overrides_core_1();
        self.apply_env_overrides_core_1b();
        self.apply_env_overrides_core_2();
        self.apply_env_overrides_core_2b();
    }

    fn apply_env_overrides_core_1(&mut self) {
        if let Ok(v) = std::env::var("ZEPH_LLM_PROVIDER") {
            if let Ok(kind) = serde_json::from_value(serde_json::Value::String(v.clone())) {
                self.llm.provider = kind;
            } else {
                tracing::warn!("ignoring invalid ZEPH_LLM_PROVIDER value: {v}");
            }
        }
        if let Ok(v) = std::env::var("ZEPH_LLM_BASE_URL") {
            self.llm.base_url = v;
        }
        if let Ok(v) = std::env::var("ZEPH_LLM_MODEL") {
            self.llm.model = v;
        }
        if let Ok(v) = std::env::var("ZEPH_LLM_EMBEDDING_MODEL") {
            self.llm.embedding_model = v;
        }
        if let Ok(v) = std::env::var("ZEPH_SQLITE_PATH") {
            self.memory.sqlite_path = v;
        }
        if let Ok(v) = std::env::var("ZEPH_QDRANT_URL") {
            self.memory.qdrant_url = v;
        }
        if let Ok(v) = std::env::var("ZEPH_MEMORY_SEMANTIC_ENABLED")
            && let Ok(enabled) = v.parse::<bool>()
        {
            self.memory.semantic.enabled = enabled;
        }
        if let Ok(v) = std::env::var("ZEPH_MEMORY_RECALL_LIMIT")
            && let Ok(limit) = v.parse::<usize>()
        {
            self.memory.semantic.recall_limit = limit;
        }
        if let Ok(v) = std::env::var("ZEPH_MEMORY_SUMMARIZATION_THRESHOLD")
            && let Ok(threshold) = v.parse::<usize>()
        {
            self.memory.summarization_threshold = threshold;
        }
        if let Ok(v) = std::env::var("ZEPH_MEMORY_CONTEXT_BUDGET_TOKENS")
            && let Ok(tokens) = v.parse::<usize>()
        {
            self.memory.context_budget_tokens = tokens;
        }
        if let Ok(v) = std::env::var("ZEPH_MEMORY_COMPACTION_THRESHOLD")
            && let Ok(threshold) = v.parse::<f32>()
        {
            self.memory.hard_compaction_threshold = threshold;
        }
        if let Ok(v) = std::env::var("ZEPH_MEMORY_SOFT_COMPACTION_THRESHOLD")
            && let Ok(threshold) = v.parse::<f32>()
        {
            self.memory.soft_compaction_threshold = threshold;
        }
        if let Ok(v) = std::env::var("ZEPH_MEMORY_COMPACTION_PRESERVE_TAIL")
            && let Ok(tail) = v.parse::<usize>()
        {
            self.memory.compaction_preserve_tail = tail;
        }
        if let Ok(v) = std::env::var("ZEPH_MEMORY_AUTO_BUDGET")
            && let Ok(enabled) = v.parse::<bool>()
        {
            self.memory.auto_budget = enabled;
        }
        if let Ok(v) = std::env::var("ZEPH_MEMORY_PRUNE_PROTECT_TOKENS")
            && let Ok(tokens) = v.parse::<usize>()
        {
            self.memory.prune_protect_tokens = tokens;
        }
        if let Ok(v) = std::env::var("ZEPH_MEMORY_VECTOR_BACKEND") {
            match v.to_lowercase().as_str() {
                "sqlite" => {
                    self.memory.vector_backend = crate::memory::VectorBackend::Sqlite;
                }
                "qdrant" => {
                    self.memory.vector_backend = crate::memory::VectorBackend::Qdrant;
                }
                _ => {}
            }
        }
    }

    fn apply_env_overrides_core_1b(&mut self) {
        if let Ok(v) = std::env::var("ZEPH_SKILLS_MAX_ACTIVE")
            && let Ok(n) = v.parse::<usize>()
        {
            self.skills.max_active_skills = n;
        }
        if let Ok(v) = std::env::var("ZEPH_SKILLS_LEARNING_ENABLED")
            && let Ok(enabled) = v.parse::<bool>()
        {
            self.skills.learning.enabled = enabled;
        }
        if let Ok(v) = std::env::var("ZEPH_SKILLS_LEARNING_AUTO_ACTIVATE")
            && let Ok(auto_activate) = v.parse::<bool>()
        {
            self.skills.learning.auto_activate = auto_activate;
        }
        if let Ok(v) = std::env::var("ZEPH_SKILLS_PROMPT_MODE") {
            match v.to_lowercase().as_str() {
                "full" => self.skills.prompt_mode = crate::features::SkillPromptMode::Full,
                "compact" => self.skills.prompt_mode = crate::features::SkillPromptMode::Compact,
                "auto" => self.skills.prompt_mode = crate::features::SkillPromptMode::Auto,
                _ => {}
            }
        }
        if let Ok(v) = std::env::var("ZEPH_MEMORY_SEMANTIC_TEMPORAL_DECAY_ENABLED")
            && let Ok(enabled) = v.parse::<bool>()
        {
            self.memory.semantic.temporal_decay_enabled = enabled;
        }
        if let Ok(v) = std::env::var("ZEPH_MEMORY_SEMANTIC_TEMPORAL_DECAY_HALF_LIFE_DAYS")
            && let Ok(days) = v.parse::<u32>()
        {
            self.memory.semantic.temporal_decay_half_life_days = days;
        }
        if let Ok(v) = std::env::var("ZEPH_MEMORY_SEMANTIC_MMR_ENABLED")
            && let Ok(enabled) = v.parse::<bool>()
        {
            self.memory.semantic.mmr_enabled = enabled;
        }
        if let Ok(v) = std::env::var("ZEPH_MEMORY_SEMANTIC_MMR_LAMBDA")
            && let Ok(lambda) = v.parse::<f32>()
        {
            self.memory.semantic.mmr_lambda = lambda;
        }
        if let Ok(v) = std::env::var("ZEPH_MEMORY_TOKEN_SAFETY_MARGIN")
            && let Ok(margin) = v.parse::<f32>()
        {
            self.memory.token_safety_margin = margin.clamp(0.1, 10.0);
        }
        if let Ok(v) = std::env::var("ZEPH_TOOLS_SUMMARIZE_OUTPUT")
            && let Ok(enabled) = v.parse::<bool>()
        {
            self.tools.summarize_output = enabled;
        }
        if let Ok(v) = std::env::var("ZEPH_TOOLS_SHELL_ALLOWED_COMMANDS") {
            self.tools.shell.allowed_commands = v
                .split(',')
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
                .collect();
        }
        if let Ok(v) = std::env::var("ZEPH_TOOLS_TIMEOUT")
            && let Ok(secs) = v.parse::<u64>()
        {
            self.tools.shell.timeout = secs;
        }
        if let Ok(v) = std::env::var("ZEPH_TOOLS_SCRAPE_TIMEOUT")
            && let Ok(secs) = v.parse::<u64>()
        {
            self.tools.scrape.timeout = secs;
        }
        if let Ok(v) = std::env::var("ZEPH_TOOLS_SCRAPE_MAX_BODY")
            && let Ok(bytes) = v.parse::<usize>()
        {
            self.tools.scrape.max_body_bytes = bytes;
        }
    }

    fn apply_env_overrides_core_2(&mut self) {
        if let Ok(v) = std::env::var("ZEPH_INDEX_ENABLED")
            && let Ok(enabled) = v.parse::<bool>()
        {
            self.index.enabled = enabled;
        }
        if let Ok(v) = std::env::var("ZEPH_INDEX_MAX_CHUNKS")
            && let Ok(n) = v.parse::<usize>()
        {
            self.index.max_chunks = n;
        }
        if let Ok(v) = std::env::var("ZEPH_INDEX_SCORE_THRESHOLD")
            && let Ok(t) = v.parse::<f32>()
        {
            self.index.score_threshold = t.clamp(0.0, 1.0);
        }
        if let Ok(v) = std::env::var("ZEPH_INDEX_BUDGET_RATIO")
            && let Ok(r) = v.parse::<f32>()
        {
            self.index.budget_ratio = r.clamp(0.0, 1.0);
        }
        if let Ok(v) = std::env::var("ZEPH_INDEX_REPO_MAP_TOKENS")
            && let Ok(n) = v.parse::<usize>()
        {
            self.index.repo_map_tokens = n;
        }
        if let Ok(v) = std::env::var("ZEPH_STT_PROVIDER") {
            let stt = self.llm.stt.get_or_insert_with(|| SttConfig {
                provider: default_stt_provider(),
                model: default_stt_model(),
                language: default_stt_language(),
                base_url: None,
            });
            stt.provider = v;
        }
        if let Ok(v) = std::env::var("ZEPH_STT_MODEL") {
            let stt = self.llm.stt.get_or_insert_with(|| SttConfig {
                provider: default_stt_provider(),
                model: default_stt_model(),
                language: default_stt_language(),
                base_url: None,
            });
            stt.model = v;
        }
        if let Ok(v) = std::env::var("ZEPH_STT_LANGUAGE") {
            let stt = self.llm.stt.get_or_insert_with(|| SttConfig {
                provider: default_stt_provider(),
                model: default_stt_model(),
                language: default_stt_language(),
                base_url: None,
            });
            stt.language = v;
        }
        if let Ok(v) = std::env::var("ZEPH_STT_BASE_URL") {
            let stt = self.llm.stt.get_or_insert_with(|| SttConfig {
                provider: default_stt_provider(),
                model: default_stt_model(),
                language: default_stt_language(),
                base_url: None,
            });
            stt.base_url = Some(v);
        }
    }

    fn apply_env_overrides_core_2b(&mut self) {
        if let Ok(v) = std::env::var("ZEPH_AUTO_UPDATE_CHECK")
            && let Ok(enabled) = v.parse::<bool>()
        {
            self.agent.auto_update_check = enabled;
        }
        if let Ok(v) = std::env::var("ZEPH_A2A_ENABLED")
            && let Ok(enabled) = v.parse::<bool>()
        {
            self.a2a.enabled = enabled;
        }
        if let Ok(v) = std::env::var("ZEPH_A2A_HOST") {
            self.a2a.host = v;
        }
        if let Ok(v) = std::env::var("ZEPH_A2A_PORT")
            && let Ok(port) = v.parse::<u16>()
        {
            self.a2a.port = port;
        }
        if let Ok(v) = std::env::var("ZEPH_A2A_PUBLIC_URL") {
            self.a2a.public_url = v;
        }
        if let Ok(v) = std::env::var("ZEPH_A2A_RATE_LIMIT")
            && let Ok(rate) = v.parse::<u32>()
        {
            self.a2a.rate_limit = rate;
        }
        if let Ok(v) = std::env::var("ZEPH_MEMORY_AUTOSAVE_ASSISTANT")
            && let Ok(enabled) = v.parse::<bool>()
        {
            self.memory.autosave_assistant = enabled;
        }
        if let Ok(v) = std::env::var("ZEPH_MEMORY_AUTOSAVE_MIN_LENGTH")
            && let Ok(len) = v.parse::<usize>()
        {
            self.memory.autosave_min_length = len;
        }
        if let Ok(v) = std::env::var("ZEPH_LLM_RESPONSE_CACHE_ENABLED")
            && let Ok(enabled) = v.parse::<bool>()
        {
            self.llm.response_cache_enabled = enabled;
        }
        if let Ok(v) = std::env::var("ZEPH_LLM_RESPONSE_CACHE_TTL_SECS")
            && let Ok(secs) = v.parse::<u64>()
        {
            self.llm.response_cache_ttl_secs = secs;
        }
        if let Ok(v) = std::env::var("ZEPH_LLM_SEMANTIC_CACHE_ENABLED")
            && let Ok(enabled) = v.parse::<bool>()
        {
            self.llm.semantic_cache_enabled = enabled;
        }
        if let Ok(v) = std::env::var("ZEPH_LLM_SEMANTIC_CACHE_THRESHOLD")
            && let Ok(threshold) = v.parse::<f32>()
        {
            self.llm.semantic_cache_threshold = threshold;
        }
        if let Ok(v) = std::env::var("ZEPH_LLM_SEMANTIC_CACHE_MAX_CANDIDATES")
            && let Ok(max) = v.parse::<u32>()
        {
            self.llm.semantic_cache_max_candidates = max;
        }
        if let Ok(v) = std::env::var("ZEPH_ACP_ENABLED")
            && let Ok(enabled) = v.parse::<bool>()
        {
            self.acp.enabled = enabled;
        }
        if let Ok(v) = std::env::var("ZEPH_ACP_AGENT_NAME") {
            self.acp.agent_name = v;
        }
        if let Ok(v) = std::env::var("ZEPH_ACP_AGENT_VERSION") {
            self.acp.agent_version = v;
        }
        if let Ok(v) = std::env::var("ZEPH_ACP_MAX_SESSIONS")
            && let Ok(n) = v.parse::<usize>()
        {
            self.acp.max_sessions = n;
        }
        if let Ok(v) = std::env::var("ZEPH_ACP_SESSION_IDLE_TIMEOUT_SECS")
            && let Ok(secs) = v.parse::<u64>()
        {
            self.acp.session_idle_timeout_secs = secs;
        }
        if let Ok(v) = std::env::var("ZEPH_ACP_PERMISSION_FILE") {
            self.acp.permission_file = Some(std::path::PathBuf::from(v));
        }
        if let Ok(v) = std::env::var("ZEPH_ACP_AUTH_TOKEN") {
            self.acp.auth_token = Some(v);
        }
        if let Ok(v) = std::env::var("ZEPH_ACP_DISCOVERY_ENABLED")
            && let Ok(enabled) = v.parse::<bool>()
        {
            self.acp.discovery_enabled = enabled;
        }
        if let Ok(v) = std::env::var("ZEPH_LOG_FILE") {
            self.logging.file = v;
        }
        if let Ok(v) = std::env::var("ZEPH_LOG_LEVEL") {
            self.logging.level = v;
        }
    }

    fn apply_env_overrides_security(&mut self) {
        if let Ok(v) = std::env::var("ZEPH_TOOLS_SHELL_ALLOWED_PATHS") {
            self.tools.shell.allowed_paths = v
                .split(',')
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
                .collect();
        }
        if let Ok(v) = std::env::var("ZEPH_TOOLS_SHELL_ALLOW_NETWORK")
            && let Ok(allow) = v.parse::<bool>()
        {
            self.tools.shell.allow_network = allow;
        }
        if let Ok(v) = std::env::var("ZEPH_TOOLS_AUDIT_ENABLED")
            && let Ok(enabled) = v.parse::<bool>()
        {
            self.tools.audit.enabled = enabled;
        }
        if let Ok(v) = std::env::var("ZEPH_TOOLS_AUDIT_DESTINATION") {
            self.tools.audit.destination = v;
        }
        if let Ok(v) = std::env::var("ZEPH_SECURITY_REDACT_SECRETS")
            && let Ok(redact) = v.parse::<bool>()
        {
            self.security.redact_secrets = redact;
        }
        if let Ok(v) = std::env::var("ZEPH_TIMEOUT_LLM")
            && let Ok(secs) = v.parse::<u64>()
        {
            self.timeouts.llm_seconds = secs;
        }
        if let Ok(v) = std::env::var("ZEPH_TIMEOUT_LLM_REQUEST")
            && let Ok(secs) = v.parse::<u64>()
        {
            self.timeouts.llm_request_timeout_secs = secs;
        }
        if let Ok(v) = std::env::var("ZEPH_TIMEOUT_EMBEDDING")
            && let Ok(secs) = v.parse::<u64>()
        {
            self.timeouts.embedding_seconds = secs;
        }
        if let Ok(v) = std::env::var("ZEPH_TIMEOUT_A2A")
            && let Ok(secs) = v.parse::<u64>()
        {
            self.timeouts.a2a_seconds = secs;
        }
        if let Ok(v) = std::env::var("ZEPH_A2A_REQUIRE_TLS")
            && let Ok(require) = v.parse::<bool>()
        {
            self.a2a.require_tls = require;
        }
        if let Ok(v) = std::env::var("ZEPH_A2A_SSRF_PROTECTION")
            && let Ok(ssrf) = v.parse::<bool>()
        {
            self.a2a.ssrf_protection = ssrf;
        }
        if let Ok(v) = std::env::var("ZEPH_A2A_MAX_BODY_SIZE")
            && let Ok(size) = v.parse::<usize>()
        {
            self.a2a.max_body_size = size;
        }
    }
}
