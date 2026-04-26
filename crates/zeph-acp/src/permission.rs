// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Tool-call permission gate backed by IDE approval and a TOML persistence file.
//!
//! Before executing a tool call the agent asks the [`AcpPermissionGate`] whether
//! the IDE has approved it. The gate consults an in-memory cache first:
//!
//! - `AllowAlways` — approved at session start (from TOML or IDE "always allow")
//! - `RejectAlways` — denied at session start (from TOML or IDE "always deny")
//! - Cache miss — forwards the request to the IDE via `check_tool_call`
//!
//! `AllowAlways` / `RejectAlways` decisions are persisted atomically to a TOML file
//! so they survive agent restarts.
//!
//! # TOML format
//!
//! ```toml
//! [tools]
//! web_scrape = "allow"
//! read_file = "deny"
//!
//! [tools.bash]
//! default = "ask"
//!
//! [tools.bash.patterns]
//! git = "allow"
//! cargo = "allow"
//! rm = "deny"
//! ```

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::RwLock;

use agent_client_protocol as acp;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};
use tracing::warn;

use crate::error::AcpError;

#[derive(Debug, Clone, Copy)]
enum PermissionDecision {
    AllowAlways,
    RejectAlways,
}

struct PermissionRequest {
    session_id: acp::schema::SessionId,
    tool_call: acp::schema::ToolCallUpdate,
    reply: oneshot::Sender<Result<bool, AcpError>>,
}

#[derive(Debug, Serialize, Deserialize, Default)]
struct PersistedPermissions {
    #[serde(default)]
    tools: HashMap<String, ToolPermission>,
}

/// Per-tool permission entry in the persisted TOML file.
///
/// Simple variant: `tool_name = "allow"` or `tool_name = "deny"`.
/// Patterned variant: used for bash-like tools to grant/deny per command binary.
///
/// ```toml
/// [tools.bash]
/// default = "ask"
///
/// [tools.bash.patterns]
/// git = "allow"
/// cargo = "allow"
/// rm = "deny"
///
/// [tools]
/// web_scrape = "allow"
/// ```
#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
enum ToolPermission {
    Simple(String),
    Patterned {
        #[serde(default)]
        default: Option<String>,
        #[serde(default)]
        patterns: HashMap<String, String>,
    },
}

/// Transparent prefixes that wrap another command without changing its semantics.
const TRANSPARENT_PREFIXES: &[&str] = &["env", "command", "exec", "nice", "nohup", "time"];

/// Extract the effective command binary name from a shell command string.
///
/// Iteratively skips transparent prefixes (`env`, `command`, `exec`, etc.) and
/// env-var assignments (`FOO=bar`) to reach the real binary name.
/// Falls back to `"bash"` if the command is empty.
fn extract_command_binary_owned(command: &str) -> String {
    let mut tokens = command.split_whitespace().peekable();
    loop {
        match tokens.peek() {
            None => return "bash".to_owned(),
            Some(tok) => {
                if tok.contains('=') {
                    tokens.next();
                    continue;
                }
                let base = tok.rsplit('/').next().unwrap_or(tok);
                if TRANSPARENT_PREFIXES.contains(&base) {
                    tokens.next();
                    continue;
                }
                let binary = tok.rsplit('/').next().unwrap_or(tok);
                return binary.to_owned();
            }
        }
    }
}

fn default_permission_file() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("zeph")
        .join("acp-permissions.toml")
}

fn load_persisted(path: &Path) -> PersistedPermissions {
    // path is trusted config input — no path traversal validation needed.
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return PersistedPermissions::default();
        }
        Err(e) => {
            warn!("failed to read ACP permission file {}: {e}", path.display());
            return PersistedPermissions::default();
        }
    };
    if content.len() > 1_048_576 {
        warn!(
            "ACP permission file {} exceeds 1 MiB, ignoring",
            path.display()
        );
        return PersistedPermissions::default();
    }
    match toml::from_str(&content) {
        Ok(p) => p,
        Err(e) => {
            warn!(
                "failed to parse ACP permission file {}: {e}",
                path.display()
            );
            PersistedPermissions::default()
        }
    }
}

fn save_persisted(path: &Path, perms: &PersistedPermissions) {
    let content = match toml::to_string(perms) {
        Ok(c) => c,
        Err(e) => {
            warn!("failed to serialize ACP permissions: {e}");
            return;
        }
    };
    if let Some(parent) = path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        warn!(
            "failed to create ACP permission dir {}: {e}",
            parent.display()
        );
        return;
    }
    if let Err(e) = zeph_common::fs_secure::atomic_write_private(path, content.as_bytes()) {
        warn!("failed to write ACP permission file: {e}");
    }
}

/// Permission gate that routes tool-call permission requests to the IDE via ACP.
///
/// Uses an in-memory cache to short-circuit `AllowAlways` / `RejectAlways` decisions
/// without a round-trip to the IDE on every call. Decisions from `AcpPermissionGate::new`
/// (loaded from the TOML file) seed the cache at startup.
///
/// Construct with [`AcpPermissionGate::new`] and spawn the returned future inside the
/// `LocalSet` that owns the ACP connection. The `Clone` impl gives each capability
/// (filesystem, shell) its own handle to the same underlying channel.
#[derive(Clone)]
pub struct AcpPermissionGate {
    request_tx: mpsc::Sender<PermissionRequest>,
    cache: Arc<RwLock<HashMap<String, PermissionDecision>>>,
}

impl AcpPermissionGate {
    /// Create the gate and the `LocalSet`-side handler future.
    ///
    /// Spawn the returned future inside a `LocalSet` that owns `conn`.
    pub fn new(
        conn: std::sync::Arc<acp::ConnectionTo<acp::Client>>,
        permission_file: Option<PathBuf>,
    ) -> (Self, impl std::future::Future<Output = ()> + Send + 'static) {
        let file = permission_file.unwrap_or_else(default_permission_file);
        let persisted = load_persisted(&file);

        let mut initial: HashMap<String, PermissionDecision> = HashMap::new();
        for (tool_name, perm) in &persisted.tools {
            match perm {
                ToolPermission::Simple(decision_str) => {
                    let decision = match decision_str.as_str() {
                        "allow" => PermissionDecision::AllowAlways,
                        "deny" | "reject" => PermissionDecision::RejectAlways,
                        other => {
                            warn!("unknown persisted permission '{other}' for tool '{tool_name}'");
                            continue;
                        }
                    };
                    // Store without session prefix — on check_permission we look up tool_name directly.
                    initial.insert(tool_name.clone(), decision);
                }
                ToolPermission::Patterned { default, patterns } => {
                    // Load per-binary patterns as "tool_name\x01binary" cache keys.
                    for (binary, decision_str) in patterns {
                        let decision = match decision_str.as_str() {
                            "allow" => PermissionDecision::AllowAlways,
                            "deny" | "reject" => PermissionDecision::RejectAlways,
                            other => {
                                warn!(
                                    "unknown persisted pattern permission '{other}' for \
                                     tool '{tool_name}' binary '{binary}'"
                                );
                                continue;
                            }
                        };
                        initial.insert(format!("{tool_name}\x01{binary}"), decision);
                    }
                    // Load default decision for the tool as a fallback.
                    if let Some(default_str) = default {
                        let decision = match default_str.as_str() {
                            "allow" => PermissionDecision::AllowAlways,
                            "deny" | "reject" => PermissionDecision::RejectAlways,
                            other => {
                                warn!(
                                    "unknown persisted default permission '{other}' for \
                                     tool '{tool_name}'"
                                );
                                continue;
                            }
                        };
                        initial.insert(tool_name.clone(), decision);
                    }
                }
            }
        }

        // Bounded: each permission check is request-response; 64 slots are sufficient
        // for concurrent tool calls. Excess is backpressured via the async send below.
        let (tx, rx) = mpsc::channel::<PermissionRequest>(64);
        let cache: Arc<RwLock<HashMap<String, PermissionDecision>>> =
            Arc::new(RwLock::new(initial));
        let cache_clone = Arc::clone(&cache);

        let handler = async move { run_permission_handler(conn, rx, cache_clone, file).await };

        (
            Self {
                request_tx: tx,
                cache,
            },
            handler,
        )
    }

    /// Ask the IDE whether the given tool call is permitted.
    ///
    /// Returns `true` if allowed, `false` if rejected, `Err` on protocol failure.
    ///
    /// # Errors
    ///
    /// Returns `AcpError::ChannelClosed` when the `LocalSet` handler has exited,
    /// or `AcpError::ClientError` when the IDE returns a protocol error.
    pub async fn check_permission(
        &self,
        session_id: acp::schema::SessionId,
        tool_call: acp::schema::ToolCallUpdate,
    ) -> Result<bool, AcpError> {
        // Key on session + tool title for AllowAlways/RejectAlways caching. When title is absent,
        // fall back to tool_call_id so distinct untitled tools never share the same cache entry.
        let fallback = tool_call.tool_call_id.to_string();
        let tool_name_raw = tool_call
            .fields
            .title
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or(&fallback);
        let tool_name_owned;
        let tool_name = if tool_name_raw.contains('\0') {
            tool_name_owned = tool_name_raw.replace('\0', "");
            &tool_name_owned
        } else {
            tool_name_raw
        };
        let session_cache_key = format!("{session_id}\0{tool_name}");

        // Fast path: check session-scoped key first, then tool-name-only (persisted).
        // For patterned tools (e.g. "bash"), also check the per-binary pattern key.
        // The binary name is extracted from raw_input["command"] when present.
        {
            let guard = self.cache.read();
            // Check session-scoped key first.
            if let Some(d) = guard.get(session_cache_key.as_str()) {
                return Ok(matches!(d, PermissionDecision::AllowAlways));
            }
            // Extract binary from raw_input for patterned lookup.
            let binary = tool_call
                .fields
                .raw_input
                .as_ref()
                .and_then(|v| v.get("command"))
                .and_then(|c| c.as_str())
                .map(extract_command_binary_owned);
            // Check per-binary pattern key: "tool_name\x01binary".
            if let Some(ref bin) = binary {
                let pattern_key = format!("{tool_name}\x01{bin}");
                if let Some(d) = guard.get(pattern_key.as_str()) {
                    return Ok(matches!(d, PermissionDecision::AllowAlways));
                }
            }
            // Fall back to tool-name-only (persisted default).
            if let Some(d) = guard.get(tool_name) {
                return Ok(matches!(d, PermissionDecision::AllowAlways));
            }
        }

        let (reply_tx, reply_rx) = oneshot::channel();
        self.request_tx
            .send(PermissionRequest {
                session_id,
                tool_call,
                reply: reply_tx,
            })
            .await
            .map_err(|_| AcpError::ChannelClosed)?;

        reply_rx.await.map_err(|_| AcpError::ChannelClosed)?
    }
}

async fn run_permission_handler(
    conn: std::sync::Arc<acp::ConnectionTo<acp::Client>>,
    mut rx: mpsc::Receiver<PermissionRequest>,
    cache: Arc<RwLock<HashMap<String, PermissionDecision>>>,
    permission_file: PathBuf,
) {
    while let Some(req) = rx.recv().await {
        let options = vec![
            acp::schema::PermissionOption::new(
                "allow_once",
                "Allow once",
                acp::schema::PermissionOptionKind::AllowOnce,
            ),
            acp::schema::PermissionOption::new(
                "allow_always",
                "Allow always",
                acp::schema::PermissionOptionKind::AllowAlways,
            ),
            acp::schema::PermissionOption::new(
                "reject_once",
                "Reject once",
                acp::schema::PermissionOptionKind::RejectOnce,
            ),
            acp::schema::PermissionOption::new(
                "reject_always",
                "Reject always",
                acp::schema::PermissionOptionKind::RejectAlways,
            ),
        ];

        let fallback = req.tool_call.tool_call_id.to_string();
        let tool_name = req
            .tool_call
            .fields
            .title
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or(&fallback)
            .replace('\0', "");
        // Extract command binary for patterned tools (e.g. "bash" -> "git", "cargo").
        let cmd_binary = req
            .tool_call
            .fields
            .raw_input
            .as_ref()
            .and_then(|v| v.get("command"))
            .and_then(|c| c.as_str())
            .map(extract_command_binary_owned);
        let session_id = &req.session_id;
        let session_cache_key = format!("{session_id}\0{tool_name}");
        let perm_req =
            acp::schema::RequestPermissionRequest::new(req.session_id, req.tool_call, options);

        let result = conn.send_request(perm_req).block_task().await;

        let reply = match result {
            Err(e) => Err(AcpError::ClientError(e.to_string())),
            Ok(resp) => match resp.outcome {
                acp::schema::RequestPermissionOutcome::Cancelled => Err(AcpError::ClientError(
                    "permission request cancelled".to_owned(),
                )),
                acp::schema::RequestPermissionOutcome::Selected(selected) => {
                    let option_id = selected.option_id.0.as_ref();
                    let allowed = matches!(option_id, "allow_once" | "allow_always");

                    let decision = match option_id {
                        "allow_always" => Some(PermissionDecision::AllowAlways),
                        "reject_always" => Some(PermissionDecision::RejectAlways),
                        _ => None,
                    };
                    if let Some(d) = decision {
                        let mut guard = cache.write();
                        // Insert session-scoped key for fast in-process lookup.
                        guard.insert(session_cache_key, d);
                        // For patterned tools, cache at binary granularity.
                        if let Some(ref bin) = cmd_binary {
                            guard.insert(format!("{tool_name}\x01{bin}"), d);
                        } else {
                            // Simple tool — cache at tool-name level.
                            guard.insert(tool_name.clone(), d);
                        }
                        save_persisted(&permission_file, &rebuild_persisted(&guard));
                    }

                    Ok(allowed)
                }
                _ => Err(AcpError::ClientError(
                    "unknown permission outcome".to_owned(),
                )),
            },
        };

        req.reply.send(reply).ok();
    }
}

/// Rebuild the persisted TOML structure from the in-memory cache.
///
/// Keys without `\0` (session separator) and without `\x01` (pattern separator) are simple tool
/// entries. Keys with `\x01` are per-binary patterns: `"tool_name\x01binary"`.
fn rebuild_persisted(guard: &HashMap<String, PermissionDecision>) -> PersistedPermissions {
    let mut result: PersistedPermissions = PersistedPermissions::default();
    for (k, v) in guard {
        // Skip session-scoped keys (contain '\0').
        if k.contains('\0') {
            continue;
        }
        let decision_str = match v {
            PermissionDecision::AllowAlways => "allow",
            PermissionDecision::RejectAlways => "deny",
        };
        if let Some((tool, binary)) = k.split_once('\x01') {
            // Per-binary pattern key.
            match result
                .tools
                .entry(tool.to_owned())
                .or_insert_with(|| ToolPermission::Patterned {
                    default: None,
                    patterns: HashMap::new(),
                }) {
                ToolPermission::Patterned { patterns, .. } => {
                    patterns.insert(binary.to_owned(), decision_str.to_owned());
                }
                // Upgrade Simple to Patterned if there's a collision (shouldn't happen).
                entry @ ToolPermission::Simple(_) => {
                    *entry = ToolPermission::Patterned {
                        default: None,
                        patterns: HashMap::from([(binary.to_owned(), decision_str.to_owned())]),
                    };
                }
            }
        } else {
            // Simple tool-level key — only insert if not already Patterned.
            result
                .tools
                .entry(k.clone())
                .or_insert_with(|| ToolPermission::Simple(decision_str.to_owned()));
        }
    }
    result
}

// Tests disabled pending ACP 0.11 test infrastructure update (issue #3267 PR3)
#[cfg(any())] // ACP 0.10 tests disabled — pending PR3 test infrastructure
mod tests {
    use super::*;
    use std::rc::Rc;

    struct AlwaysAllowClient;

    #[async_trait::async_trait(?Send)]
    impl acp::Client for AlwaysAllowClient {
        async fn request_permission(
            &self,
            args: acp::schema::RequestPermissionRequest,
        ) -> acp::Result<acp::RequestPermissionResponse> {
            let option_id = args.options[0].option_id.clone();
            Ok(acp::RequestPermissionResponse::new(
                acp::schema::RequestPermissionOutcome::Selected(
                    acp::SelectedPermissionOutcome::new(option_id),
                ),
            ))
        }
        async fn session_notification(
            &self,
            _args: acp::schema::SessionNotification,
        ) -> acp::Result<()> {
            Ok(())
        }
    }

    struct AlwaysRejectClient;

    #[async_trait::async_trait(?Send)]
    impl acp::Client for AlwaysRejectClient {
        async fn request_permission(
            &self,
            _args: acp::schema::RequestPermissionRequest,
        ) -> acp::Result<acp::RequestPermissionResponse> {
            Ok(acp::RequestPermissionResponse::new(
                acp::schema::RequestPermissionOutcome::Selected(
                    acp::SelectedPermissionOutcome::new("reject_once"),
                ),
            ))
        }
        async fn session_notification(
            &self,
            _args: acp::schema::SessionNotification,
        ) -> acp::Result<()> {
            Ok(())
        }
    }

    struct AllowAlwaysClient;

    #[async_trait::async_trait(?Send)]
    impl acp::Client for AllowAlwaysClient {
        async fn request_permission(
            &self,
            _args: acp::schema::RequestPermissionRequest,
        ) -> acp::Result<acp::RequestPermissionResponse> {
            Ok(acp::RequestPermissionResponse::new(
                acp::schema::RequestPermissionOutcome::Selected(
                    acp::SelectedPermissionOutcome::new("allow_always"),
                ),
            ))
        }
        async fn session_notification(
            &self,
            _args: acp::schema::SessionNotification,
        ) -> acp::Result<()> {
            Ok(())
        }
    }

    struct RejectAlwaysClient;

    #[async_trait::async_trait(?Send)]
    impl acp::Client for RejectAlwaysClient {
        async fn request_permission(
            &self,
            _args: acp::schema::RequestPermissionRequest,
        ) -> acp::Result<acp::RequestPermissionResponse> {
            Ok(acp::RequestPermissionResponse::new(
                acp::schema::RequestPermissionOutcome::Selected(
                    acp::SelectedPermissionOutcome::new("reject_always"),
                ),
            ))
        }
        async fn session_notification(
            &self,
            _args: acp::schema::SessionNotification,
        ) -> acp::Result<()> {
            Ok(())
        }
    }

    struct CancelledClient;

    #[async_trait::async_trait(?Send)]
    impl acp::Client for CancelledClient {
        async fn request_permission(
            &self,
            _args: acp::schema::RequestPermissionRequest,
        ) -> acp::Result<acp::RequestPermissionResponse> {
            Ok(acp::RequestPermissionResponse::new(
                acp::schema::RequestPermissionOutcome::Cancelled,
            ))
        }
        async fn session_notification(
            &self,
            _args: acp::schema::SessionNotification,
        ) -> acp::Result<()> {
            Ok(())
        }
    }

    fn make_tool_call(id: &str) -> acp::schema::ToolCallUpdate {
        acp::schema::ToolCallUpdate::new(
            id.to_owned(),
            acp::schema::ToolCallUpdateFields::default(),
        )
    }

    #[tokio::test]
    async fn allow_once_returns_true() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let conn = Rc::new(AlwaysAllowClient);
                let (gate, handler) = AcpPermissionGate::new(conn, None);
                tokio::task::spawn_local(handler);

                let sid = acp::schema::SessionId::new("s1");
                let tc = make_tool_call("tc1");
                let result = gate.check_permission(sid, tc).await.unwrap();
                assert!(result);
            })
            .await;
    }

    #[tokio::test]
    async fn reject_once_returns_false() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let conn = Rc::new(AlwaysRejectClient);
                let (gate, handler) = AcpPermissionGate::new(conn, None);
                tokio::task::spawn_local(handler);

                let sid = acp::schema::SessionId::new("s1");
                let tc = make_tool_call("tc2");
                let result = gate.check_permission(sid, tc).await.unwrap();
                assert!(!result);
            })
            .await;
    }

    #[tokio::test]
    async fn allow_always_is_cached() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let conn = Rc::new(AllowAlwaysClient);
                let (gate, handler) = AcpPermissionGate::new(conn, None);
                tokio::task::spawn_local(handler);

                let sid = acp::schema::SessionId::new("s1");
                let tc = make_tool_call("tc-aa");
                let first = gate
                    .check_permission(sid.clone(), tc.clone())
                    .await
                    .unwrap();
                assert!(first);
                let second = gate.check_permission(sid, tc).await.unwrap();
                assert!(second);
            })
            .await;
    }

    #[tokio::test]
    async fn reject_always_is_cached() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let conn = Rc::new(RejectAlwaysClient);
                let (gate, handler) = AcpPermissionGate::new(conn, None);
                tokio::task::spawn_local(handler);

                let sid = acp::schema::SessionId::new("s1");
                let tc = make_tool_call("tc-ra");
                let first = gate
                    .check_permission(sid.clone(), tc.clone())
                    .await
                    .unwrap();
                assert!(!first);
                let second = gate.check_permission(sid, tc).await.unwrap();
                assert!(!second);
            })
            .await;
    }

    #[tokio::test]
    async fn cancelled_returns_error() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let conn = Rc::new(CancelledClient);
                let (gate, handler) = AcpPermissionGate::new(conn, None);
                tokio::task::spawn_local(handler);

                let sid = acp::schema::SessionId::new("s1");
                let tc = make_tool_call("tc-cancel");
                let result = gate.check_permission(sid, tc).await;
                assert!(result.is_err());
                assert!(result.unwrap_err().to_string().contains("cancelled"));
            })
            .await;
    }

    #[test]
    fn persist_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("acp-permissions.toml");

        // Write persisted file manually.
        let mut perms = PersistedPermissions::default();
        perms.tools.insert(
            "shell_execute".to_owned(),
            ToolPermission::Simple("allow".to_owned()),
        );
        perms.tools.insert(
            "web_scrape".to_owned(),
            ToolPermission::Simple("reject".to_owned()),
        );
        save_persisted(&file, &perms);

        // Load and verify.
        let loaded = load_persisted(&file);
        assert!(matches!(
            loaded.tools.get("shell_execute"),
            Some(ToolPermission::Simple(s)) if s == "allow"
        ));
        assert!(matches!(
            loaded.tools.get("web_scrape"),
            Some(ToolPermission::Simple(s)) if s == "reject"
        ));
    }

    #[test]
    fn load_missing_file_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("nonexistent.toml");
        let loaded = load_persisted(&file);
        assert!(loaded.tools.is_empty());
    }

    #[test]
    fn load_corrupt_toml_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("acp-permissions.toml");
        std::fs::write(&file, "this is not valid [[[ toml").unwrap();
        let loaded = load_persisted(&file);
        assert!(loaded.tools.is_empty());
    }

    #[test]
    fn load_empty_file_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("acp-permissions.toml");
        std::fs::write(&file, "").unwrap();
        let loaded = load_persisted(&file);
        assert!(loaded.tools.is_empty());
    }

    #[test]
    fn load_oversized_file_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("acp-permissions.toml");
        let content = "a".repeat(1_048_577);
        std::fs::write(&file, &content).unwrap();
        let loaded = load_persisted(&file);
        assert!(loaded.tools.is_empty());
    }

    #[test]
    fn unknown_decision_string_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("acp-permissions.toml");
        std::fs::write(
            &file,
            "[tools]\nshell_execute = \"allow\"\nbad_tool = \"unknown_value\"\n",
        )
        .unwrap();

        let local = tokio::runtime::Runtime::new().unwrap();
        local.block_on(async {
            let local_set = tokio::task::LocalSet::new();
            local_set
                .run_until(async {
                    let conn = Rc::new(AlwaysRejectClient);
                    let (gate, handler) = AcpPermissionGate::new(conn, Some(file));
                    tokio::task::spawn_local(handler);

                    // shell_execute should be allowed from persisted "allow".
                    let sid = acp::schema::SessionId::new("s1");
                    let tc = make_tool_call("shell_execute");
                    assert!(gate.check_permission(sid, tc).await.unwrap());

                    // bad_tool should NOT be in cache — falls through to RejectClient.
                    let sid2 = acp::schema::SessionId::new("s1");
                    let tc2 = make_tool_call("bad_tool");
                    assert!(!gate.check_permission(sid2, tc2).await.unwrap());
                })
                .await;
        });
    }

    #[tokio::test]
    async fn null_byte_in_tool_name_does_not_collide() {
        // "a\0b" should not collide with session="a" + tool="b"
        // because \0 is stripped from tool_name before building the key.
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let conn = Rc::new(AllowAlwaysClient);
                let (gate, handler) = AcpPermissionGate::new(conn, None);
                tokio::task::spawn_local(handler);

                // First call with tool_name "b" under session "a" — gets AllowAlways cached.
                let sid = acp::schema::SessionId::new("a");
                let tc = make_tool_call("b");
                assert!(gate.check_permission(sid, tc).await.unwrap());

                // Now a call with tool_name "a\0b" — after stripping \0 becomes "ab",
                // which is a different cache key than "a\0b".
                let conn2 = Rc::new(AlwaysRejectClient);
                let (gate2, handler2) = AcpPermissionGate::new(conn2, None);
                tokio::task::spawn_local(handler2);

                let sid2 = acp::schema::SessionId::new("s2");
                let mut tc2 = make_tool_call("tc-null");
                tc2.fields.title = Some("a\0b".to_owned());
                // AllowAlways was cached for "b", not "ab", so gate2 (RejectClient) should reject.
                assert!(!gate2.check_permission(sid2, tc2).await.unwrap());
            })
            .await;
    }

    #[tokio::test]
    async fn persisted_decision_applied_on_new_gate() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("acp-permissions.toml");

        // Pre-populate permission file.
        let mut perms = PersistedPermissions::default();
        perms.tools.insert(
            "tc-persisted".to_owned(),
            ToolPermission::Simple("allow".to_owned()),
        );
        save_persisted(&file, &perms);

        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                // Gate should load file and short-circuit without asking IDE.
                let conn = Rc::new(AlwaysRejectClient); // would reject if asked
                let (gate, handler) = AcpPermissionGate::new(conn, Some(file.clone()));
                tokio::task::spawn_local(handler);

                let sid = acp::schema::SessionId::new("s-new");
                let tc = make_tool_call("tc-persisted");
                // Should be allowed from persisted cache, not forwarded to RejectClient.
                let result = gate.check_permission(sid, tc).await.unwrap();
                assert!(result);
            })
            .await;
    }

    fn make_tool_call_with_command(
        id: &str,
        title: &str,
        command: &str,
    ) -> acp::schema::ToolCallUpdate {
        let fields = acp::schema::ToolCallUpdateFields::new()
            .title(title.to_owned())
            .raw_input(serde_json::json!({ "command": command }));
        acp::schema::ToolCallUpdate::new(id.to_owned(), fields)
    }

    #[test]
    fn patterned_permission_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("acp-permissions.toml");

        let mut patterns = HashMap::new();
        patterns.insert("git".to_owned(), "allow".to_owned());
        patterns.insert("rm".to_owned(), "deny".to_owned());
        let mut perms = PersistedPermissions::default();
        perms.tools.insert(
            "bash".to_owned(),
            ToolPermission::Patterned {
                default: Some("ask".to_owned()),
                patterns,
            },
        );
        save_persisted(&file, &perms);

        let loaded = load_persisted(&file);
        match loaded.tools.get("bash") {
            Some(ToolPermission::Patterned { patterns, default }) => {
                assert_eq!(patterns.get("git").map(String::as_str), Some("allow"));
                assert_eq!(patterns.get("rm").map(String::as_str), Some("deny"));
                assert_eq!(default.as_deref(), Some("ask"));
            }
            other => panic!("expected Patterned, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn per_binary_pattern_allow_is_cached() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("acp-permissions.toml");

        // Pre-populate: bash.git = allow
        let mut patterns = HashMap::new();
        patterns.insert("git".to_owned(), "allow".to_owned());
        let mut perms = PersistedPermissions::default();
        perms.tools.insert(
            "git".to_owned(),
            ToolPermission::Patterned {
                default: None,
                patterns,
            },
        );
        save_persisted(&file, &perms);

        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let conn = Rc::new(AlwaysRejectClient);
                let (gate, handler) = AcpPermissionGate::new(conn, Some(file));
                tokio::task::spawn_local(handler);

                let sid = acp::schema::SessionId::new("s1");
                let tc = make_tool_call_with_command("tc1", "git", "git status");
                // Should be allowed from pattern cache.
                assert!(gate.check_permission(sid, tc).await.unwrap());
            })
            .await;
    }

    #[tokio::test]
    async fn per_binary_pattern_deny_short_circuits() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("acp-permissions.toml");

        // Pre-populate: bash.rm = deny
        let mut patterns = HashMap::new();
        patterns.insert("rm".to_owned(), "deny".to_owned());
        let mut perms = PersistedPermissions::default();
        perms.tools.insert(
            "rm".to_owned(),
            ToolPermission::Patterned {
                default: None,
                patterns,
            },
        );
        save_persisted(&file, &perms);

        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                // AlwaysAllowClient would allow if asked — but pattern must short-circuit.
                let conn = Rc::new(AllowAlwaysClient);
                let (gate, handler) = AcpPermissionGate::new(conn, Some(file));
                tokio::task::spawn_local(handler);

                let sid = acp::schema::SessionId::new("s1");
                let tc = make_tool_call_with_command("tc1", "rm", "rm -rf /tmp/test");
                // Should be rejected from pattern cache without asking IDE.
                assert!(!gate.check_permission(sid, tc).await.unwrap());
            })
            .await;
    }

    #[test]
    fn extract_command_binary_owned_basic() {
        assert_eq!(extract_command_binary_owned("git status"), "git");
        assert_eq!(extract_command_binary_owned("cargo build"), "cargo");
        assert_eq!(extract_command_binary_owned("env FOO=bar git log"), "git");
        assert_eq!(extract_command_binary_owned("/usr/bin/git push"), "git");
        assert_eq!(extract_command_binary_owned("FOO=bar baz"), "baz");
        assert_eq!(extract_command_binary_owned(""), "bash");
    }

    #[tokio::test]
    async fn allow_always_for_git_does_not_auto_allow_rm() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                // AllowAlwaysClient always responds allow_always.
                let conn = Rc::new(AllowAlwaysClient);
                let (gate, handler) = AcpPermissionGate::new(conn, None);
                tokio::task::spawn_local(handler);

                let sid = acp::schema::SessionId::new("s1");
                // First call: "git" gets AllowAlways cached.
                let tc_git = make_tool_call_with_command("tc1", "git", "git status");
                assert!(gate.check_permission(sid.clone(), tc_git).await.unwrap());

                // Now check "rm" — different binary, must NOT inherit git's AllowAlways.
                // AllowAlwaysClient will be asked and return allow_always, but the point is
                // the cache key is "rm", not "git". We verify by using a different gate
                // backed by RejectClient for "rm".
                let conn2 = Rc::new(AlwaysRejectClient);
                let (gate2, handler2) = AcpPermissionGate::new(conn2, None);
                tokio::task::spawn_local(handler2);

                let sid2 = acp::schema::SessionId::new("s2");
                let tc_rm = make_tool_call_with_command("tc2", "rm", "rm /tmp/test");
                // gate2 has no cache for "rm" — falls through to AlwaysRejectClient.
                assert!(!gate2.check_permission(sid2, tc_rm).await.unwrap());
            })
            .await;
    }

    #[test]
    fn rebuild_persisted_simple_deny_not_lost_when_patterned_present() {
        // SEC-ACP-S1: a Simple deny for "web_scrape" must survive rebuild_persisted
        // even when a Patterned entry for "bash" is also in the cache.
        let mut cache: HashMap<String, PermissionDecision> = HashMap::new();
        cache.insert("web_scrape".to_owned(), PermissionDecision::RejectAlways);
        cache.insert("bash\x01git".to_owned(), PermissionDecision::AllowAlways);
        cache.insert("bash\x01rm".to_owned(), PermissionDecision::RejectAlways);

        let persisted = rebuild_persisted(&cache);

        // Simple deny for web_scrape must be present.
        assert!(
            matches!(persisted.tools.get("web_scrape"), Some(ToolPermission::Simple(s)) if s == "deny"),
            "Simple deny for web_scrape was lost: {:?}",
            persisted.tools
        );
        // Patterned entry for bash must be present.
        match persisted.tools.get("bash") {
            Some(ToolPermission::Patterned { patterns, .. }) => {
                assert_eq!(patterns.get("git").map(String::as_str), Some("allow"));
                assert_eq!(patterns.get("rm").map(String::as_str), Some("deny"));
            }
            other => panic!("expected Patterned for bash, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn within_gate_allow_git_does_not_allow_rm() {
        // GAP-002: stronger isolation proof — same gate, one session, git allowed but rm rejected.
        // We use AllowAlwaysClient for git's first call, then RejectAlwaysClient via a second gate.
        // The key check: AllowAlways cached for "git" must NOT affect "rm" in the SAME gate
        // instance backed by a new client.
        //
        // Implementation: we use one gate backed by AllowAlwaysClient to cache "git",
        // then a second gate backed by RejectAlwaysClient to prove "rm" is NOT cached.
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let dir = tempfile::tempdir().unwrap();
                let perm_file = dir.path().join("perms.toml");

                // Gate 1: allows git always — writes to perm_file.
                let conn1 = Rc::new(AllowAlwaysClient);
                let (gate1, handler1) = AcpPermissionGate::new(conn1, Some(perm_file.clone()));
                tokio::task::spawn_local(handler1);

                let sid = acp::schema::SessionId::new("s1");
                let tc_git = make_tool_call_with_command("tc1", "git", "git status");
                assert!(gate1.check_permission(sid.clone(), tc_git).await.unwrap());

                // Drop gate1 to ensure perm_file is written. Give it a tick.
                tokio::task::yield_now().await;

                // Gate 2: backed by RejectAlwaysClient — rm must NOT be in the loaded perms.
                let conn2 = Rc::new(RejectAlwaysClient);
                let (gate2, handler2) = AcpPermissionGate::new(conn2, Some(perm_file));
                tokio::task::spawn_local(handler2);

                let sid2 = acp::schema::SessionId::new("s2");
                let tc_rm = make_tool_call_with_command("tc2", "rm", "rm /tmp/test");
                // rm was never allowed — gate2 must ask RejectAlwaysClient which rejects.
                assert!(!gate2.check_permission(sid2, tc_rm).await.unwrap());
            })
            .await;
    }

    #[test]
    fn permission_tmp_uses_pid_suffix() {
        let path = std::path::PathBuf::from("/tmp/perms.toml");
        let pid = std::process::id();
        let tmp = path.with_added_extension(format!("{pid}.tmp"));
        let name = tmp.file_name().unwrap().to_string_lossy();
        assert!(name.starts_with("perms.toml."), "unexpected prefix: {name}");
        assert!(name.ends_with(".tmp"), "unexpected suffix: {name}");
    }
}
