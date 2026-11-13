// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Per-session lifecycle of IDE-provided MCP servers.
//!
//! The IDE sends `mcpServers` in `session/new`, `session/load`, `session/fork` and
//! `session/resume`. Each server is registered on the shared [`McpManager`] under an id scoped to
//! the session, so two sessions that name the same server never collide, and is released when the
//! session's [`IdeMcpLease`] is dropped (close, delete, idle reaping, LRU eviction).
//!
//! Release is asynchronous (`Drop` cannot await), so the scoped id of a session that is closed and
//! immediately loaded again can still have a release queued while the new session attaches. Every
//! attach therefore takes ownership of its ids first, and a queued release only removes a server
//! while the lease that registered it is still the owner.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use agent_client_protocol as acp;
use tokio::sync::{Mutex, mpsc};
use tracing::Instrument as _;
use zeph_common::task_supervisor::{RestartPolicy, TaskDescriptor};
use zeph_common::types::McpServerId;
use zeph_mcp::{McpError, McpManager, McpTool, ServerEntry};

use crate::mcp_bridge::acp_mcp_servers_to_entries;

use super::ZephAcpAgentState;

/// Registration surface of the MCP manager that the IDE lease protocol needs.
///
/// A trait only so the ownership and release protocol can be tested against a fake host; the one
/// production implementation is [`McpManager`].
trait IdeMcpHost: Sync {
    fn add_server(
        &self,
        entry: &ServerEntry,
    ) -> impl Future<Output = Result<Vec<McpTool>, McpError>> + Send;

    fn remove_server(&self, server_id: &str) -> impl Future<Output = Result<(), McpError>> + Send;
}

impl IdeMcpHost for McpManager {
    fn add_server(
        &self,
        entry: &ServerEntry,
    ) -> impl Future<Output = Result<Vec<McpTool>, McpError>> + Send {
        McpManager::add_server(self, entry)
    }

    fn remove_server(&self, server_id: &str) -> impl Future<Output = Result<(), McpError>> + Send {
        McpManager::remove_server(self, server_id)
    }
}

/// Number of leading session-id characters that scope an IDE server id.
const SESSION_SUFFIX_CHARS: usize = 8;

/// Identity of one attach, used to tell a current owner from a stale release.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LeaseId(u64);

/// A queued release: the ids one lease registered.
#[derive(Debug)]
struct ReleaseRequest {
    lease: LeaseId,
    servers: Vec<McpServerId>,
}

/// Drop guard that releases the IDE servers a session attached.
///
/// Held by the session entry; dropping it queues a release that the supervised
/// `acp_ide_mcp_release` task executes, skipping ids that a newer lease has since taken over.
#[derive(Debug)]
pub(crate) struct IdeMcpLease {
    id: LeaseId,
    servers: Vec<McpServerId>,
    release_tx: mpsc::UnboundedSender<ReleaseRequest>,
}

impl Drop for IdeMcpLease {
    fn drop(&mut self) {
        let request = ReleaseRequest {
            lease: self.id,
            servers: std::mem::take(&mut self.servers),
        };
        if self.release_tx.send(request).is_err() {
            tracing::debug!("IDE MCP release dropped: release task is gone");
        }
    }
}

/// Result of attaching a session's IDE servers.
pub(crate) struct IdeMcpAttachment {
    /// Tools the attached servers expose, to be added to the session's tool list.
    pub(crate) tools: Vec<McpTool>,
    /// Guard that releases the servers; `None` when nothing was attached.
    pub(crate) lease: Option<IdeMcpLease>,
}

impl IdeMcpAttachment {
    fn empty() -> Self {
        Self {
            tools: Vec::new(),
            lease: None,
        }
    }
}

/// Ownership map and release queue for IDE servers on one ACP connection.
pub(crate) struct IdeMcpRegistry {
    // Held across `add_server`/`remove_server` on purpose: both are rare control-plane calls with
    // the manager's own timeouts, and the lock is what serializes attach against release.
    owners: Mutex<HashMap<McpServerId, LeaseId>>,
    next_lease: AtomicU64,
    release_tx: mpsc::UnboundedSender<ReleaseRequest>,
    release_rx: Mutex<mpsc::UnboundedReceiver<ReleaseRequest>>,
    release_task_started: AtomicBool,
}

impl IdeMcpRegistry {
    pub(crate) fn new() -> Self {
        let (release_tx, release_rx) = mpsc::unbounded_channel();
        Self {
            owners: Mutex::new(HashMap::new()),
            next_lease: AtomicU64::new(0),
            release_tx,
            release_rx: Mutex::new(release_rx),
            release_task_started: AtomicBool::new(false),
        }
    }

    /// Register `entries` (already scoped) and return their tools plus the releasing lease.
    ///
    /// Takes ownership of each id before touching the manager. `ServerAlreadyConnected` means a
    /// release of an older lease is still queued, or its owner never released: this lease owns the
    /// id now, so the stale server is removed and registered once more. Any other failure skips
    /// that server.
    async fn attach(
        &self,
        manager: &impl IdeMcpHost,
        entries: Vec<ServerEntry>,
    ) -> IdeMcpAttachment {
        let lease = LeaseId(self.next_lease.fetch_add(1, Ordering::Relaxed));
        let mut owners = self.owners.lock().await;
        let mut tools = Vec::new();
        let mut attached = Vec::new();
        for entry in entries {
            let server_id = McpServerId::from(entry.id.as_str());
            owners.insert(server_id.clone(), lease);
            match add_with_recovery(manager, &entry).await {
                Ok(server_tools) => {
                    tools.extend(server_tools);
                    attached.push(server_id);
                }
                Err(e) => {
                    tracing::warn!(server_id = %server_id, error = %e, "failed to register IDE MCP server");
                    owners.remove(&server_id);
                }
            }
        }
        drop(owners);
        let lease = (!attached.is_empty()).then(|| IdeMcpLease {
            id: lease,
            servers: attached,
            release_tx: self.release_tx.clone(),
        });
        IdeMcpAttachment { tools, lease }
    }

    /// Execute a queued release, removing only the servers `request.lease` still owns.
    async fn release(&self, manager: &impl IdeMcpHost, request: ReleaseRequest) {
        let mut owners = self.owners.lock().await;
        for server_id in request.servers {
            if owners.get(&server_id) != Some(&request.lease) {
                tracing::debug!(server_id = %server_id, "skipping stale IDE MCP release");
                continue;
            }
            owners.remove(&server_id);
            match manager.remove_server(server_id.as_str()).await {
                Ok(()) | Err(McpError::ServerNotFound { .. }) => {}
                Err(e) => {
                    tracing::warn!(server_id = %server_id, error = %e, "failed to release IDE MCP server");
                }
            }
        }
    }

    /// Drain queued releases until `cancel` fires.
    async fn run_release_loop(
        &self,
        manager: &impl IdeMcpHost,
        cancel: tokio_util::sync::CancellationToken,
    ) {
        let mut rx = self.release_rx.lock().await;
        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => break,
                request = rx.recv() => {
                    let Some(request) = request else { break };
                    self.release(manager, request)
                        .instrument(tracing::info_span!("acp.session.ide_mcp_release"))
                        .await;
                }
            }
        }
    }
}

async fn add_with_recovery(
    manager: &impl IdeMcpHost,
    entry: &ServerEntry,
) -> Result<Vec<McpTool>, McpError> {
    match manager.add_server(entry).await {
        Err(McpError::ServerAlreadyConnected { .. }) => {
            match manager.remove_server(&entry.id).await {
                Ok(()) | Err(McpError::ServerNotFound { .. }) => {}
                Err(e) => return Err(e),
            }
            manager.add_server(entry).await
        }
        other => other,
    }
}

/// Scope an IDE-chosen server id to a session: `"{ide_id}-{first 8 chars of session id}"`.
fn scoped_server_id(ide_id: &str, session_id: &str) -> String {
    let suffix: String = session_id.chars().take(SESSION_SUFFIX_CHARS).collect();
    format!("{ide_id}-{suffix}")
}

/// Convert, de-duplicate by IDE id (first wins) and scope the servers of one request.
fn scoped_entries(
    servers: &[acp::schema::v1::McpServer],
    session_id: &str,
    elicitation_default_timeout_secs: u64,
) -> Vec<ServerEntry> {
    let mut seen = HashSet::new();
    acp_mcp_servers_to_entries(servers, elicitation_default_timeout_secs)
        .into_iter()
        .filter_map(|mut entry| {
            if !seen.insert(entry.id.clone()) {
                tracing::warn!(server_id = %entry.id, "duplicate IDE MCP server id in request ignored");
                return None;
            }
            entry.id = scoped_server_id(&entry.id, session_id);
            Some(entry)
        })
        .collect()
}

impl ZephAcpAgentState {
    /// Register the IDE-provided `servers` of one session on the shared MCP manager.
    ///
    /// Shared by `session/new`, `session/load`, `session/fork` and `session/resume`. Ids are
    /// scoped to `session_id`; the returned lease releases them when the session entry is
    /// dropped. Returns an empty attachment when no MCP manager is configured.
    pub(crate) async fn attach_ide_mcp_servers(
        &self,
        session_id: &acp::schema::v1::SessionId,
        servers: &[acp::schema::v1::McpServer],
    ) -> IdeMcpAttachment {
        let Some(ref manager) = self.mcp_manager else {
            return IdeMcpAttachment::empty();
        };
        let entries = scoped_entries(
            servers,
            &session_id.to_string(),
            self.timeouts.elicitation_secs,
        );
        if entries.is_empty() {
            return IdeMcpAttachment::empty();
        }
        self.ensure_ide_mcp_release_task(manager);
        self.ide_mcp.attach(&**manager, entries).await
    }

    /// Start the supervised release drain on first use.
    fn ensure_ide_mcp_release_task(&self, manager: &std::sync::Arc<McpManager>) {
        if self
            .ide_mcp
            .release_task_started
            .swap(true, Ordering::AcqRel)
        {
            return;
        }
        let registry = std::sync::Arc::clone(&self.ide_mcp);
        let manager = std::sync::Arc::clone(manager);
        let cancel = self.reaper_cancel.clone();
        self.task_supervisor.spawn(TaskDescriptor {
            name: "acp_ide_mcp_release",
            restart: RestartPolicy::RunOnce,
            factory: move || {
                let registry = std::sync::Arc::clone(&registry);
                let manager = std::sync::Arc::clone(&manager);
                let cancel = cancel.clone();
                async move { registry.run_release_loop(&*manager, cancel).await }
            },
        });
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use parking_lot::Mutex as SyncMutex;

    use super::*;

    /// In-memory host: `connected` mirrors the manager's client map.
    #[derive(Default)]
    struct FakeHost {
        connected: SyncMutex<HashSet<String>>,
        failing: SyncMutex<HashSet<String>>,
        removed: SyncMutex<Vec<String>>,
    }

    impl FakeHost {
        fn with_connected(ids: &[&str]) -> Self {
            let host = Self::default();
            host.connected
                .lock()
                .extend(ids.iter().map(|s| (*s).to_owned()));
            host
        }

        fn is_connected(&self, id: &str) -> bool {
            self.connected.lock().contains(id)
        }

        fn add(&self, entry: &ServerEntry) -> Result<Vec<McpTool>, McpError> {
            if self.failing.lock().contains(&entry.id) {
                return Err(McpError::ServerNotFound {
                    server_id: entry.id.clone(),
                });
            }
            if !self.connected.lock().insert(entry.id.clone()) {
                return Err(McpError::ServerAlreadyConnected {
                    server_id: entry.id.clone(),
                });
            }
            Ok(vec![tool(&entry.id)])
        }

        fn remove(&self, server_id: &str) -> Result<(), McpError> {
            self.removed.lock().push(server_id.to_owned());
            if self.connected.lock().remove(server_id) {
                Ok(())
            } else {
                Err(McpError::ServerNotFound {
                    server_id: server_id.to_owned(),
                })
            }
        }
    }

    impl IdeMcpHost for FakeHost {
        fn add_server(
            &self,
            entry: &ServerEntry,
        ) -> impl Future<Output = Result<Vec<McpTool>, McpError>> + Send {
            std::future::ready(self.add(entry))
        }

        fn remove_server(
            &self,
            server_id: &str,
        ) -> impl Future<Output = Result<(), McpError>> + Send {
            std::future::ready(self.remove(server_id))
        }
    }

    fn tool(server: &str) -> McpTool {
        McpTool {
            server_id: server.into(),
            name: "t".to_owned(),
            description: String::new(),
            input_schema: serde_json::json!({}),
            output_schema: None,
            security_meta: zeph_mcp::tool::ToolSecurityMeta::default(),
        }
    }

    fn entry(id: &str) -> ServerEntry {
        ServerEntry {
            id: id.to_owned(),
            transport: zeph_mcp::McpTransport::Stdio {
                command: "unused".to_owned(),
                args: Vec::new(),
                env: HashMap::new(),
            },
            timeout: Duration::from_secs(1),
            trust_level: zeph_mcp::McpTrustLevel::Untrusted,
            tool_allowlist: None,
            allow_untrusted_without_allowlist: false,
            expected_tools: Vec::new(),
            roots: Vec::new(),
            tool_metadata: HashMap::new(),
            elicitation_enabled: false,
            elicitation_timeout_secs: 120,
            env_isolation: false,
            media_passthrough: false,
        }
    }

    async fn owner_of(registry: &IdeMcpRegistry, id: &str) -> Option<LeaseId> {
        registry
            .owners
            .lock()
            .await
            .get(&McpServerId::from(id))
            .copied()
    }

    async fn next_release(registry: &IdeMcpRegistry) -> ReleaseRequest {
        registry
            .release_rx
            .lock()
            .await
            .recv()
            .await
            .expect("a release must be queued")
    }

    #[test]
    fn scoped_id_uses_first_eight_session_chars() {
        assert_eq!(scoped_server_id("ide", "abcdef0123456789"), "ide-abcdef01");
        assert_eq!(scoped_server_id("ide", "abc"), "ide-abc");
    }

    #[test]
    fn duplicate_ide_ids_are_deduplicated_before_scoping() {
        use acp::schema::v1::{McpServer, McpServerStdio};

        let servers = vec![
            McpServer::Stdio(McpServerStdio::new("dup", "/bin/a")),
            McpServer::Stdio(McpServerStdio::new("dup", "/bin/b")),
            McpServer::Stdio(McpServerStdio::new("other", "/bin/c")),
        ];
        let entries = scoped_entries(&servers, "0123456789", 120);
        let ids: Vec<&str> = entries.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, ["dup-01234567", "other-01234567"]);
    }

    #[tokio::test]
    async fn attach_returns_tools_and_owns_the_ids() {
        let registry = IdeMcpRegistry::new();
        let host = FakeHost::default();
        let attachment = registry
            .attach(&host, vec![entry("a-1"), entry("b-1")])
            .await;
        assert_eq!(attachment.tools.len(), 2);
        let lease = attachment.lease.expect("servers were attached");
        assert_eq!(owner_of(&registry, "a-1").await, Some(lease.id));
        assert_eq!(owner_of(&registry, "b-1").await, Some(lease.id));
    }

    #[tokio::test]
    async fn dropped_lease_releases_its_servers() {
        let registry = IdeMcpRegistry::new();
        let host = FakeHost::default();
        let attachment = registry.attach(&host, vec![entry("a-1")]).await;
        assert!(host.is_connected("a-1"));

        drop(attachment.lease);
        let request = next_release(&registry).await;
        registry.release(&host, request).await;

        assert!(!host.is_connected("a-1"));
        assert_eq!(owner_of(&registry, "a-1").await, None);
    }

    #[tokio::test]
    async fn failed_server_is_not_owned_and_yields_no_lease() {
        let registry = IdeMcpRegistry::new();
        let host = FakeHost::default();
        host.failing.lock().insert("bad-1".to_owned());
        let attachment = registry.attach(&host, vec![entry("bad-1")]).await;
        assert!(attachment.tools.is_empty());
        assert!(attachment.lease.is_none());
        assert_eq!(owner_of(&registry, "bad-1").await, None);
    }

    #[tokio::test]
    async fn already_connected_server_is_removed_and_registered_again() {
        let registry = IdeMcpRegistry::new();
        let host = FakeHost::with_connected(&["ide-1"]);

        let attachment = registry.attach(&host, vec![entry("ide-1")]).await;

        assert_eq!(attachment.tools.len(), 1, "re-registration must succeed");
        assert_eq!(*host.removed.lock(), ["ide-1"]);
        assert!(host.is_connected("ide-1"));
        let lease = attachment.lease.expect("the new lease owns the id");
        assert_eq!(owner_of(&registry, "ide-1").await, Some(lease.id));
    }

    #[tokio::test]
    async fn stale_lease_release_is_skipped_when_a_newer_lease_owns_the_id() {
        let registry = IdeMcpRegistry::new();
        let host = FakeHost::with_connected(&["srv-1"]);
        registry
            .owners
            .lock()
            .await
            .insert(McpServerId::from("srv-1"), LeaseId(2));

        registry
            .release(
                &host,
                ReleaseRequest {
                    lease: LeaseId(1),
                    servers: vec![McpServerId::from("srv-1")],
                },
            )
            .await;

        assert_eq!(owner_of(&registry, "srv-1").await, Some(LeaseId(2)));
        assert!(host.is_connected("srv-1"));
        assert!(host.removed.lock().is_empty());
    }

    #[tokio::test]
    async fn close_then_load_same_session_keeps_new_server_after_old_release_drains() {
        let registry = IdeMcpRegistry::new();
        let host = FakeHost::default();

        // Generation 1 attaches; closing the session drops its lease and queues a release.
        let first = registry.attach(&host, vec![entry("ide-1")]).await;
        drop(first.lease);

        // Generation 2 (same scoped id) attaches before the drain task runs.
        let second = registry.attach(&host, vec![entry("ide-1")]).await;
        let second_lease = second.lease.expect("reload attaches the server again");
        assert!(host.is_connected("ide-1"));

        let request = next_release(&registry).await;
        registry.release(&host, request).await;

        assert!(
            host.is_connected("ide-1"),
            "the drained stale release must not remove the reloaded session's server"
        );
        assert_eq!(owner_of(&registry, "ide-1").await, Some(second_lease.id));
    }

    #[tokio::test]
    async fn release_loop_drains_until_cancelled() {
        let registry = Arc::new(IdeMcpRegistry::new());
        let host = Arc::new(FakeHost::default());
        let attachment = registry.attach(&*host, vec![entry("a-1")]).await;
        let cancel = tokio_util::sync::CancellationToken::new();

        let task = {
            let registry = Arc::clone(&registry);
            let host = Arc::clone(&host);
            let cancel = cancel.clone();
            tokio::spawn(async move { registry.run_release_loop(&*host, cancel).await })
        };
        drop(attachment.lease);
        tokio::time::timeout(Duration::from_secs(2), async {
            while host.is_connected("a-1") {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the drain task must release the server");

        cancel.cancel();
        task.await.unwrap();
    }
}
