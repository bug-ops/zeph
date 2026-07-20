// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Builder/config methods for `ZephAcpAgentState`.
//!
//! Groups the constructor and all `with_*` configuration methods so the fluent builder
//! surface is isolated from session lifecycle and dispatch logic in [`super`].

#[cfg(feature = "unstable-llm-providers")]
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use parking_lot::{Mutex, RwLock};
use tokio_util::sync::CancellationToken;

use agent_client_protocol as acp;
use zeph_common::task_supervisor::TaskSupervisor;
use zeph_core::{ContentIsolationConfig, ContentSanitizer};
use zeph_mcp::McpManager;
use zeph_memory::store::SqliteStore;

use crate::lsp::DiagnosticsCache;
use crate::transport::SharedAvailableModels;

use super::{AgentSpawner, ProviderFactory, ZephAcpAgentState};

impl ZephAcpAgentState {
    pub fn new(
        spawner: AgentSpawner,
        max_sessions: usize,
        session_idle_timeout_secs: u64,
        permission_file: Option<std::path::PathBuf>,
    ) -> Self {
        let lsp_config = zeph_core::config::AcpLspConfig::default();
        let max_diag_files = lsp_config.max_diagnostic_files;
        let reaper_cancel = CancellationToken::new();
        let task_supervisor = TaskSupervisor::new(reaper_cancel.clone());
        Self {
            spawner,
            sessions: Arc::new(Mutex::new(std::collections::HashMap::new())),
            agent_name: "zeph".to_owned(),
            agent_version: env!("CARGO_PKG_VERSION").to_owned(),
            max_sessions,
            idle_timeout: std::time::Duration::from_secs(session_idle_timeout_secs),
            store: None,
            session_data_dir: None,
            permission_file,
            client_caps: RwLock::new(acp::schema::v1::ClientCapabilities::default()),
            provider_factory: None,
            available_models: Arc::new(RwLock::new(Vec::new())),
            mcp_manager: None,
            project_rules: Vec::new(),
            title_max_chars: 60,
            max_history: 100,
            lsp_config,
            diagnostics_cache: Arc::new(RwLock::new(DiagnosticsCache::new(max_diag_files))),
            reaper_cancel,
            task_supervisor,
            additional_directories_allow: Vec::new(),
            auth_methods_config: vec![zeph_core::config::AcpAuthMethod::Agent],
            timeouts: zeph_config::AcpTimeoutsConfig::default(),
            model_config: zeph_config::AcpModelConfigConfig::default(),
            prompt_injection_detector: ContentSanitizer::new(&ContentIsolationConfig {
                spotlight_untrusted: false,
                ..ContentIsolationConfig::default()
            }),
            #[cfg(feature = "unstable-elicitation")]
            elicitation_supported: std::sync::atomic::AtomicBool::new(false),
            #[cfg(feature = "unstable-llm-providers")]
            provider_names: Vec::new(),
            #[cfg(feature = "unstable-llm-providers")]
            global_disabled_providers: Mutex::new(HashSet::new()),
            #[cfg(feature = "unstable-llm-providers")]
            global_provider_overrides: Mutex::new(HashMap::new()),
            owner_key: crate::transport::OWNER_KEY_LOCAL.to_owned(),
        }
    }

    /// Set this connection's owner identity (#5868) — see the `owner_key` field doc.
    #[must_use]
    pub fn with_owner_key(mut self, owner_key: impl Into<String>) -> Self {
        self.owner_key = owner_key.into();
        self
    }

    /// Configure the additional-directories allowlist policy.
    #[must_use]
    pub fn with_additional_directories(
        mut self,
        dirs: Vec<zeph_core::config::AdditionalDir>,
    ) -> Self {
        self.additional_directories_allow = dirs
            .into_iter()
            .map(|d| d.as_path().to_path_buf())
            .collect();
        self
    }

    /// Configure auth methods advertised in `initialize`.
    #[must_use]
    pub fn with_auth_methods(mut self, methods: Vec<zeph_core::config::AcpAuthMethod>) -> Self {
        self.auth_methods_config = methods;
        self
    }

    /// Configure ACP operation timeouts.
    #[must_use]
    pub fn with_timeouts(mut self, timeouts: zeph_config::AcpTimeoutsConfig) -> Self {
        self.timeouts = timeouts;
        self
    }

    /// Configure model-related configuration parameters (`[acp.model_config]`).
    #[must_use]
    pub fn with_model_config(mut self, model_config: zeph_config::AcpModelConfigConfig) -> Self {
        self.model_config = model_config;
        self
    }

    /// Configure LSP extension settings.
    #[must_use]
    pub fn with_lsp_config(mut self, config: zeph_core::config::AcpLspConfig) -> Self {
        let max_files = config.max_diagnostic_files;
        self.lsp_config = config;
        self.diagnostics_cache = Arc::new(RwLock::new(DiagnosticsCache::new(max_files)));
        self
    }

    #[must_use]
    pub fn with_store(mut self, store: SqliteStore) -> Self {
        self.store = Some(store);
        self
    }

    /// Set the durable per-session JSONL event-log directory (spec-068, #5343).
    #[must_use]
    pub fn with_session_data_dir(mut self, data_dir: std::path::PathBuf) -> Self {
        self.session_data_dir = Some(data_dir);
        self
    }

    #[must_use]
    pub fn with_agent_info(mut self, name: impl Into<String>, version: impl Into<String>) -> Self {
        self.agent_name = name.into();
        self.agent_version = version.into();
        self
    }

    #[must_use]
    pub fn with_provider_factory(
        mut self,
        factory: ProviderFactory,
        available_models: SharedAvailableModels,
    ) -> Self {
        self.provider_factory = Some(factory);
        self.available_models = available_models;
        self
    }

    pub(crate) fn available_models_snapshot(&self) -> Vec<String> {
        self.available_models.read().clone()
    }

    pub(crate) fn initial_model(&self) -> String {
        self.available_models_snapshot()
            .into_iter()
            .next()
            .unwrap_or_default()
    }

    #[must_use]
    pub fn with_mcp_manager(mut self, manager: Arc<McpManager>) -> Self {
        self.mcp_manager = Some(manager);
        self
    }

    #[must_use]
    pub fn with_project_rules(mut self, rules: Vec<std::path::PathBuf>) -> Self {
        self.project_rules = rules;
        self
    }

    #[must_use]
    pub fn with_title_max_chars(mut self, max_chars: usize) -> Self {
        self.title_max_chars = max_chars;
        self
    }

    #[must_use]
    pub fn with_max_history(mut self, max_history: usize) -> Self {
        self.max_history = max_history;
        self
    }
}
