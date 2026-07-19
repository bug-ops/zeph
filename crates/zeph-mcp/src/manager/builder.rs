// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! [`McpManager`] construction and fluent configuration builders.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use parking_lot::{Mutex as SyncMutex, RwLock as SyncRwLock};
use tokio_util::sync::CancellationToken;

use dashmap::DashMap;
use rmcp::transport::auth::CredentialStore;
use tokio::sync::{RwLock, mpsc, watch};

use crate::embedding_guard::EmbeddingAnomalyGuard;
use crate::policy::PolicyEnforcer;
use crate::prober::DefaultMcpProber;
use crate::tool::ToolSecurityMeta;
use crate::trust_score::TrustScoreStore;

use super::{McpManager, ServerEntry, StatusTx};

impl McpManager {
    /// Create a new `McpManager` with default settings.
    ///
    /// Uses an elicitation channel capacity of 16. Call builder methods such as
    /// [`with_prober`](Self::with_prober), [`with_lock_tool_list`](Self::with_lock_tool_list),
    /// and [`with_trust_store`](Self::with_trust_store) before [`connect_all`](Self::connect_all).
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_mcp::{McpManager, McpTransport, ServerEntry};
    /// use zeph_mcp::policy::PolicyEnforcer;
    ///
    /// let manager = McpManager::new(
    ///     vec![],
    ///     vec!["npx".to_owned()],
    ///     PolicyEnforcer::new(vec![]),
    /// );
    /// ```
    #[must_use]
    pub fn new(
        configs: Vec<ServerEntry>,
        allowed_commands: Vec<String>,
        enforcer: PolicyEnforcer,
    ) -> Self {
        Self::with_elicitation_capacity(configs, allowed_commands, enforcer, 16)
    }

    /// Like [`McpManager::new`] but with a configurable elicitation channel capacity.
    ///
    /// Use this when you need to override the default bounded-channel size (16).
    #[must_use]
    pub fn with_elicitation_capacity(
        configs: Vec<ServerEntry>,
        allowed_commands: Vec<String>,
        enforcer: PolicyEnforcer,
        elicitation_queue_capacity: usize,
    ) -> Self {
        let (refresh_tx, refresh_rx) = mpsc::channel(16);
        let (elicitation_tx, elicitation_rx) = mpsc::channel(elicitation_queue_capacity.max(1));
        let (tools_watch_tx, _) = watch::channel(Vec::new());
        let server_trust: HashMap<String, _> = configs
            .iter()
            .map(|c| {
                (
                    c.id.clone(),
                    (
                        c.trust_level,
                        c.tool_allowlist.clone(),
                        c.expected_tools.clone(),
                        c.allow_untrusted_without_allowlist,
                    ),
                )
            })
            .collect();
        let server_tool_metadata: HashMap<String, HashMap<String, ToolSecurityMeta>> = configs
            .iter()
            .map(|c| (c.id.clone(), c.tool_metadata.clone()))
            .collect();
        let server_elicitation: HashMap<String, bool> = configs
            .iter()
            .map(|c| (c.id.clone(), c.elicitation_enabled))
            .collect();
        let server_elicitation_timeout: HashMap<String, u64> = configs
            .iter()
            .map(|c| (c.id.clone(), c.elicitation_timeout_secs))
            .collect();
        Self {
            configs,
            allowed_commands,
            clients: Arc::new(RwLock::new(HashMap::new())),
            connected_server_ids: SyncRwLock::new(HashSet::new()),
            enforcer: Arc::new(enforcer),
            suppress_stderr: false,
            server_tools: Arc::new(RwLock::new(HashMap::new())),
            refresh_tx: SyncMutex::new(Some(refresh_tx)),
            refresh_rx: SyncMutex::new(Some(refresh_rx)),
            tools_watch_tx,
            last_refresh: Arc::new(DashMap::new()),
            oauth_credentials: HashMap::new(),
            status_tx: None,
            server_trust: Arc::new(tokio::sync::RwLock::new(server_trust)),
            server_fingerprints: Arc::new(RwLock::new(HashMap::new())),
            prober: None,
            trust_store: None,
            embedding_guard: None,
            server_tool_metadata: Arc::new(server_tool_metadata),
            max_description_bytes: crate::sanitize::DEFAULT_MAX_TOOL_DESCRIPTION_BYTES,
            max_instructions_bytes: 2048,
            server_instructions: Arc::new(RwLock::new(HashMap::new())),
            elicitation_tx: SyncMutex::new(Some(elicitation_tx)),
            elicitation_rx: SyncMutex::new(Some(elicitation_rx)),
            server_elicitation,
            server_elicitation_timeout,
            add_remove_lock: tokio::sync::Mutex::new(()),
            lock_tool_list: false,
            tool_list_locked: Arc::new(DashMap::new()),
            shutdown_token: CancellationToken::new(),
            max_connect_attempts: 3,
            startup_retry_backoff_ms: 1_000,
            tool_timeout_secs: None,
        }
    }

    /// Enable tool-list locking after initial connect.
    ///
    /// When enabled, `tools/list_changed` refresh events are rejected for all servers
    /// that have completed their initial connection, preventing mid-session tool injection.
    #[must_use]
    pub fn with_lock_tool_list(mut self, lock: bool) -> Self {
        self.lock_tool_list = lock;
        self
    }

    /// Configure the maximum byte lengths for tool descriptions and server instructions.
    ///
    /// Both default to 2048. Pass values from `[mcp]` config section.
    #[must_use]
    pub fn with_description_limits(mut self, desc: usize, instr: usize) -> Self {
        self.max_description_bytes = desc;
        self.max_instructions_bytes = instr;
        self
    }

    /// Attach a pre-connect prober. Called on every new server connection.
    #[must_use]
    pub fn with_prober(mut self, prober: DefaultMcpProber) -> Self {
        self.prober = Some(prober);
        self
    }

    /// Attach a persistent trust score store.
    #[must_use]
    pub fn with_trust_store(mut self, store: Arc<TrustScoreStore>) -> Self {
        self.trust_store = Some(store);
        self
    }

    /// Attach an embedding anomaly guard.
    #[must_use]
    pub fn with_embedding_guard(mut self, guard: EmbeddingAnomalyGuard) -> Self {
        self.embedding_guard = Some(guard);
        self
    }

    /// Set a status sender for OAuth authorization messages.
    ///
    /// When set, the OAuth authorization URL is sent as a status message so the
    /// TUI can display it in the status panel. In CLI mode this is not required.
    #[must_use]
    pub fn with_status_tx(mut self, tx: StatusTx) -> Self {
        self.status_tx = Some(tx);
        self
    }

    /// Set the maximum number of connection attempts per server at startup.
    ///
    /// Value `1` means a single attempt with no retry; value `3` (default) allows two retries.
    /// Values outside `1..=10` are clamped as defence in depth — prefer validation at the config
    /// deserialisation boundary so callers get an early, descriptive error.
    #[must_use]
    pub fn with_max_connect_attempts(mut self, attempts: u8) -> Self {
        self.max_connect_attempts = attempts.clamp(1, 10);
        self
    }

    /// Set the base backoff delay for startup retry attempts.
    ///
    /// The actual inter-attempt delay is `min(base_ms * 2^(k-1), 8_000) ms` where `k` is
    /// the 1-based attempt index. Default: 1 000 ms. Values of 0 are treated as 1 ms.
    #[must_use]
    pub fn with_startup_retry_backoff_ms(mut self, base_ms: u64) -> Self {
        self.startup_retry_backoff_ms = base_ms.max(1);
        self
    }

    /// Set a global per-call timeout for MCP tool invocations.
    ///
    /// When set, this timeout overrides the per-server `ServerEntry.timeout` for every
    /// `tools/call` request, while leaving connection and `tools/list` timeouts unchanged.
    /// When not set (the default), the per-server timeout governs all operations.
    #[must_use]
    pub fn with_tool_timeout_secs(mut self, secs: u64) -> Self {
        self.tool_timeout_secs = Some(secs.max(1));
        self
    }

    /// Register a credential store for an OAuth server.
    ///
    /// Must be called before `connect_all()` for any server using `McpTransport::OAuth`.
    #[must_use]
    pub fn with_oauth_credential_store(
        mut self,
        server_id: impl Into<String>,
        store: Arc<dyn CredentialStore>,
    ) -> Self {
        self.oauth_credentials.insert(server_id.into(), store);
        self
    }

    /// When `true`, stderr of spawned MCP child processes is suppressed (`Stdio::null()`).
    ///
    /// Use in TUI mode to prevent child stderr from corrupting the terminal.
    #[must_use]
    pub fn with_suppress_stderr(mut self, suppress: bool) -> Self {
        self.suppress_stderr = suppress;
        self
    }
}
