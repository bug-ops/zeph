use super::{Config, SttConfig, default_stt_model, default_stt_provider};

impl Config {
    pub(crate) fn apply_env_overrides(&mut self) {
        self.apply_env_overrides_core();
        self.apply_env_overrides_security();
    }

    #[allow(clippy::too_many_lines)]
    fn apply_env_overrides_core(&mut self) {
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
            self.memory.compaction_threshold = threshold;
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
            });
            stt.provider = v;
        }
        if let Ok(v) = std::env::var("ZEPH_STT_MODEL") {
            let stt = self.llm.stt.get_or_insert_with(|| SttConfig {
                provider: default_stt_provider(),
                model: default_stt_model(),
            });
            stt.model = v;
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
