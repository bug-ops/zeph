// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use acp::Client as _;
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
    session_id: acp::SessionId,
    tool_call: acp::ToolCallUpdate,
    reply: oneshot::Sender<Result<bool, AcpError>>,
}

#[derive(Debug, Serialize, Deserialize, Default)]
struct PersistedPermissions {
    #[serde(default)]
    tools: HashMap<String, String>,
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
    let tmp = path.with_extension(format!("toml.{}.tmp", std::process::id()));
    if let Err(e) = std::fs::write(&tmp, &content) {
        warn!("failed to write ACP permission tmp file: {e}");
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        warn!("failed to rename ACP permission file: {e}");
    }
}

/// Permission gate that routes tool-call permission requests to the IDE via ACP.
///
/// Uses a cache to short-circuit `AllowAlways` / `RejectAlways` decisions without
/// round-tripping to the IDE on every call.
#[derive(Clone)]
pub struct AcpPermissionGate {
    request_tx: mpsc::UnboundedSender<PermissionRequest>,
    cache: Arc<RwLock<HashMap<String, PermissionDecision>>>,
}

impl AcpPermissionGate {
    /// Create the gate and the `LocalSet`-side handler future.
    ///
    /// Spawn the returned future inside a `LocalSet` that owns `conn`.
    pub fn new<C>(
        conn: std::rc::Rc<C>,
        permission_file: Option<PathBuf>,
    ) -> (Self, impl std::future::Future<Output = ()>)
    where
        C: acp::Client + 'static,
    {
        let file = permission_file.unwrap_or_else(default_permission_file);
        let persisted = load_persisted(&file);

        let mut initial: HashMap<String, PermissionDecision> = HashMap::new();
        for (tool_name, decision_str) in &persisted.tools {
            let decision = match decision_str.as_str() {
                "allow" => PermissionDecision::AllowAlways,
                "reject" => PermissionDecision::RejectAlways,
                other => {
                    warn!("unknown persisted permission decision '{other}' for tool '{tool_name}'");
                    continue;
                }
            };
            // Store without session prefix — on check_permission we look up tool_name directly.
            initial.insert(tool_name.clone(), decision);
        }

        let (tx, rx) = mpsc::unbounded_channel::<PermissionRequest>();
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
        session_id: acp::SessionId,
        tool_call: acp::ToolCallUpdate,
    ) -> Result<bool, AcpError> {
        // Key on session + tool title for AllowAlways/RejectAlways caching. When title is absent,
        // fall back to tool_call_id so distinct untitled tools never share the same cache entry.
        let fallback = tool_call.tool_call_id.to_string();
        let tool_name = tool_call
            .fields
            .title
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or(&fallback);
        let session_cache_key = format!("{session_id}\0{tool_name}");

        // Fast path: check session-scoped key first, then tool-name-only (persisted).
        if let Ok(guard) = self.cache.read()
            && let Some(d) = guard
                .get(session_cache_key.as_str())
                .or_else(|| guard.get(tool_name))
        {
            return Ok(matches!(d, PermissionDecision::AllowAlways));
        }

        let (reply_tx, reply_rx) = oneshot::channel();
        self.request_tx
            .send(PermissionRequest {
                session_id,
                tool_call,
                reply: reply_tx,
            })
            .map_err(|_| AcpError::ChannelClosed)?;

        reply_rx.await.map_err(|_| AcpError::ChannelClosed)?
    }
}

async fn run_permission_handler<C>(
    conn: std::rc::Rc<C>,
    mut rx: mpsc::UnboundedReceiver<PermissionRequest>,
    cache: Arc<RwLock<HashMap<String, PermissionDecision>>>,
    permission_file: PathBuf,
) where
    C: acp::Client,
{
    while let Some(req) = rx.recv().await {
        let options = vec![
            acp::PermissionOption::new(
                "allow_once",
                "Allow once",
                acp::PermissionOptionKind::AllowOnce,
            ),
            acp::PermissionOption::new(
                "allow_always",
                "Allow always",
                acp::PermissionOptionKind::AllowAlways,
            ),
            acp::PermissionOption::new(
                "reject_once",
                "Reject once",
                acp::PermissionOptionKind::RejectOnce,
            ),
            acp::PermissionOption::new(
                "reject_always",
                "Reject always",
                acp::PermissionOptionKind::RejectAlways,
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
            .to_owned();
        let session_id = &req.session_id;
        let session_cache_key = format!("{session_id}\0{tool_name}");
        let perm_req = acp::RequestPermissionRequest::new(req.session_id, req.tool_call, options);

        let result = conn.request_permission(perm_req).await;

        let reply = match result {
            Err(e) => Err(AcpError::ClientError(e.to_string())),
            Ok(resp) => match resp.outcome {
                acp::RequestPermissionOutcome::Cancelled => Err(AcpError::ClientError(
                    "permission request cancelled".to_owned(),
                )),
                acp::RequestPermissionOutcome::Selected(selected) => {
                    let option_id = selected.option_id.0.as_ref();
                    let allowed = matches!(option_id, "allow_once" | "allow_always");

                    let decision = match option_id {
                        "allow_always" => Some(PermissionDecision::AllowAlways),
                        "reject_always" => Some(PermissionDecision::RejectAlways),
                        _ => None,
                    };
                    if let Some(d) = decision {
                        let mut persisted = PersistedPermissions::default();
                        if let Ok(mut guard) = cache.write() {
                            // Insert session-scoped key for fast in-process lookup.
                            guard.insert(session_cache_key, d);
                            // Also insert tool-name-only key so other sessions benefit.
                            guard.insert(tool_name.clone(), d);
                            // Rebuild persisted map from all tool-name-only entries.
                            for (k, v) in guard.iter() {
                                if !k.contains('\0') {
                                    persisted.tools.insert(
                                        k.clone(),
                                        match v {
                                            PermissionDecision::AllowAlways => "allow".to_owned(),
                                            PermissionDecision::RejectAlways => "reject".to_owned(),
                                        },
                                    );
                                }
                            }
                        }
                        save_persisted(&permission_file, &persisted);
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::rc::Rc;

    struct AlwaysAllowClient;

    #[async_trait::async_trait(?Send)]
    impl acp::Client for AlwaysAllowClient {
        async fn request_permission(
            &self,
            args: acp::RequestPermissionRequest,
        ) -> acp::Result<acp::RequestPermissionResponse> {
            let option_id = args.options[0].option_id.clone();
            Ok(acp::RequestPermissionResponse::new(
                acp::RequestPermissionOutcome::Selected(acp::SelectedPermissionOutcome::new(
                    option_id,
                )),
            ))
        }
        async fn session_notification(&self, _args: acp::SessionNotification) -> acp::Result<()> {
            Ok(())
        }
    }

    struct AlwaysRejectClient;

    #[async_trait::async_trait(?Send)]
    impl acp::Client for AlwaysRejectClient {
        async fn request_permission(
            &self,
            _args: acp::RequestPermissionRequest,
        ) -> acp::Result<acp::RequestPermissionResponse> {
            Ok(acp::RequestPermissionResponse::new(
                acp::RequestPermissionOutcome::Selected(acp::SelectedPermissionOutcome::new(
                    "reject_once",
                )),
            ))
        }
        async fn session_notification(&self, _args: acp::SessionNotification) -> acp::Result<()> {
            Ok(())
        }
    }

    struct AllowAlwaysClient;

    #[async_trait::async_trait(?Send)]
    impl acp::Client for AllowAlwaysClient {
        async fn request_permission(
            &self,
            _args: acp::RequestPermissionRequest,
        ) -> acp::Result<acp::RequestPermissionResponse> {
            Ok(acp::RequestPermissionResponse::new(
                acp::RequestPermissionOutcome::Selected(acp::SelectedPermissionOutcome::new(
                    "allow_always",
                )),
            ))
        }
        async fn session_notification(&self, _args: acp::SessionNotification) -> acp::Result<()> {
            Ok(())
        }
    }

    struct RejectAlwaysClient;

    #[async_trait::async_trait(?Send)]
    impl acp::Client for RejectAlwaysClient {
        async fn request_permission(
            &self,
            _args: acp::RequestPermissionRequest,
        ) -> acp::Result<acp::RequestPermissionResponse> {
            Ok(acp::RequestPermissionResponse::new(
                acp::RequestPermissionOutcome::Selected(acp::SelectedPermissionOutcome::new(
                    "reject_always",
                )),
            ))
        }
        async fn session_notification(&self, _args: acp::SessionNotification) -> acp::Result<()> {
            Ok(())
        }
    }

    struct CancelledClient;

    #[async_trait::async_trait(?Send)]
    impl acp::Client for CancelledClient {
        async fn request_permission(
            &self,
            _args: acp::RequestPermissionRequest,
        ) -> acp::Result<acp::RequestPermissionResponse> {
            Ok(acp::RequestPermissionResponse::new(
                acp::RequestPermissionOutcome::Cancelled,
            ))
        }
        async fn session_notification(&self, _args: acp::SessionNotification) -> acp::Result<()> {
            Ok(())
        }
    }

    fn make_tool_call(id: &str) -> acp::ToolCallUpdate {
        acp::ToolCallUpdate::new(id.to_owned(), acp::ToolCallUpdateFields::default())
    }

    #[tokio::test]
    async fn allow_once_returns_true() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let conn = Rc::new(AlwaysAllowClient);
                let (gate, handler) = AcpPermissionGate::new(conn, None);
                tokio::task::spawn_local(handler);

                let sid = acp::SessionId::new("s1");
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

                let sid = acp::SessionId::new("s1");
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

                let sid = acp::SessionId::new("s1");
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

                let sid = acp::SessionId::new("s1");
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

                let sid = acp::SessionId::new("s1");
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
        perms
            .tools
            .insert("shell_execute".to_owned(), "allow".to_owned());
        perms
            .tools
            .insert("web_scrape".to_owned(), "reject".to_owned());
        save_persisted(&file, &perms);

        // Load and verify.
        let loaded = load_persisted(&file);
        assert_eq!(
            loaded.tools.get("shell_execute").map(String::as_str),
            Some("allow")
        );
        assert_eq!(
            loaded.tools.get("web_scrape").map(String::as_str),
            Some("reject")
        );
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
                    let sid = acp::SessionId::new("s1");
                    let tc = make_tool_call("shell_execute");
                    assert!(gate.check_permission(sid, tc).await.unwrap());

                    // bad_tool should NOT be in cache — falls through to RejectClient.
                    let sid2 = acp::SessionId::new("s1");
                    let tc2 = make_tool_call("bad_tool");
                    assert!(!gate.check_permission(sid2, tc2).await.unwrap());
                })
                .await;
        });
    }

    #[tokio::test]
    async fn persisted_decision_applied_on_new_gate() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("acp-permissions.toml");

        // Pre-populate permission file.
        let mut perms = PersistedPermissions::default();
        perms
            .tools
            .insert("tc-persisted".to_owned(), "allow".to_owned());
        save_persisted(&file, &perms);

        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                // Gate should load file and short-circuit without asking IDE.
                let conn = Rc::new(AlwaysRejectClient); // would reject if asked
                let (gate, handler) = AcpPermissionGate::new(conn, Some(file.clone()));
                tokio::task::spawn_local(handler);

                let sid = acp::SessionId::new("s-new");
                let tc = make_tool_call("tc-persisted");
                // Should be allowed from persisted cache, not forwarded to RejectClient.
                let result = gate.check_permission(sid, tc).await.unwrap();
                assert!(result);
            })
            .await;
    }
}
