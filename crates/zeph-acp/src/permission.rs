use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use acp::Client as _;
use agent_client_protocol as acp;
use tokio::sync::{mpsc, oneshot};

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
    pub fn new<C>(conn: std::rc::Rc<C>) -> (Self, impl std::future::Future<Output = ()>)
    where
        C: acp::Client + 'static,
    {
        let (tx, rx) = mpsc::unbounded_channel::<PermissionRequest>();
        let cache: Arc<RwLock<HashMap<String, PermissionDecision>>> =
            Arc::new(RwLock::new(HashMap::new()));
        let cache_clone = Arc::clone(&cache);

        let handler = async move { run_permission_handler(conn, rx, cache_clone).await };

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
        let cache_key = format!("{session_id}:{tool_name}");

        // Fast path: cached decision.
        if let Ok(guard) = self.cache.read() {
            match guard.get(cache_key.as_str()) {
                Some(PermissionDecision::AllowAlways) => return Ok(true),
                Some(PermissionDecision::RejectAlways) => return Ok(false),
                None => {}
            }
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
        let cache_key = format!("{session_id}:{tool_name}");
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
                    if let (Some(d), Ok(mut guard)) = (decision, cache.write()) {
                        guard.insert(cache_key, d);
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

    fn make_tool_call(id: &str) -> acp::ToolCallUpdate {
        acp::ToolCallUpdate::new(id.to_owned(), acp::ToolCallUpdateFields::default())
    }

    #[tokio::test]
    async fn allow_once_returns_true() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let conn = Rc::new(AlwaysAllowClient);
                let (gate, handler) = AcpPermissionGate::new(conn);
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
                let (gate, handler) = AcpPermissionGate::new(conn);
                tokio::task::spawn_local(handler);

                let sid = acp::SessionId::new("s1");
                let tc = make_tool_call("tc2");
                let result = gate.check_permission(sid, tc).await.unwrap();
                assert!(!result);
            })
            .await;
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

    #[tokio::test]
    async fn allow_always_is_cached() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let conn = Rc::new(AllowAlwaysClient);
                let (gate, handler) = AcpPermissionGate::new(conn);
                tokio::task::spawn_local(handler);

                let sid = acp::SessionId::new("s1");
                let tc = make_tool_call("tc-aa");
                // First call — round-trips to IDE, caches AllowAlways.
                let first = gate
                    .check_permission(sid.clone(), tc.clone())
                    .await
                    .unwrap();
                assert!(first);
                // Second call — served from cache without IDE round-trip.
                let second = gate.check_permission(sid, tc).await.unwrap();
                assert!(second);
                // Verify cache entry uses "session_id:tool_call_id" key (title is absent).
                let guard = gate.cache.read().unwrap();
                assert!(matches!(
                    guard.get("s1:tc-aa"),
                    Some(PermissionDecision::AllowAlways)
                ));
            })
            .await;
    }

    #[tokio::test]
    async fn reject_always_is_cached() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let conn = Rc::new(RejectAlwaysClient);
                let (gate, handler) = AcpPermissionGate::new(conn);
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
                let guard = gate.cache.read().unwrap();
                assert!(matches!(
                    guard.get("s1:tc-ra"),
                    Some(PermissionDecision::RejectAlways)
                ));
            })
            .await;
    }

    #[tokio::test]
    async fn cancelled_returns_error() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let conn = Rc::new(CancelledClient);
                let (gate, handler) = AcpPermissionGate::new(conn);
                tokio::task::spawn_local(handler);

                let sid = acp::SessionId::new("s1");
                let tc = make_tool_call("tc-cancel");
                let result = gate.check_permission(sid, tc).await;
                assert!(result.is_err());
                assert!(result.unwrap_err().to_string().contains("cancelled"));
            })
            .await;
    }
}
