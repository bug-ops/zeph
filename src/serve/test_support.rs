// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Router-level test harness for `zeph serve-sessions` (#5435).
//!
//! Every prior test in this module (`handlers.rs`) called handlers directly, never driving a
//! real `axum::Router` — so a route wired to the wrong method, a missing auth layer, or a broken
//! `Router::merge` would not be caught. [`ServeTestHarness`] wraps a real [`super::AppState`] and
//! exposes the actual [`super::router::build_router`] output, so tests drive it end-to-end via
//! `tower::ServiceExt::oneshot`.

use std::sync::Arc;

use axum::Router;
use parking_lot::RwLock;
use tempfile::TempDir;
use zeph_common::SessionId;
use zeph_core::serve::{LiveSessionRegistry, SessionActorHandle, SessionCommand, SessionOutput};
use zeph_llm::any::AnyProvider;
use zeph_memory::semantic::SemanticMemory;

use super::AppState;
use super::deps::ServeAgentDeps;

/// A condenser whose summarization threshold is never crossed — these router tests only care
/// about routing/auth/serialization, not summarization behavior. Mirrors
/// `handlers::tests::make_test_condenser` / `agent_factory::tests::make_test_condenser`.
fn make_test_condenser() -> (
    zeph_session::LlmCondenser,
    zeph_agent_context::memory_backend::TokenCounterAdapter,
) {
    let deps = zeph_context::summarization::SummarizationDeps {
        provider: AnyProvider::Mock(zeph_llm::mock::MockProvider::default()),
        llm_timeout: std::time::Duration::from_secs(5),
        token_counter: Arc::new(
            zeph_agent_context::memory_backend::TokenCounterAdapter::new(Arc::new(
                zeph_memory::TokenCounter::new(),
            )),
        ),
        structured_summaries: true,
        on_anchored_summary: None,
    };
    let condenser = zeph_session::LlmCondenser::new(deps, 1.0, 1);
    let token_counter_adapter = zeph_agent_context::memory_backend::TokenCounterAdapter::new(
        Arc::new(zeph_memory::TokenCounter::new()),
    );
    (condenser, token_counter_adapter)
}

/// Router-level test harness: a real [`AppState`] (mock provider, `:memory:` `SemanticMemory`)
/// plus the actual production [`super::router::build_router`] output.
///
/// Holds a per-instance [`TempDir`] for session persistence (M2, critic round 2): a fixed
/// relative `data_dir` would flock-collide across nextest's parallel test execution and pollute
/// the repo tree — mirrors `agent_factory.rs`'s `hydrate_session_sink_links_conversation_id`.
pub(crate) struct ServeTestHarness {
    state: AppState,
    /// Kept alive for the harness's lifetime: `state`'s `session_persistence_config.data_dir`
    /// points inside this directory.
    _session_dir: Option<TempDir>,
    auth_token: Option<String>,
    require_auth: bool,
}

impl ServeTestHarness {
    /// Build a harness with session persistence enabled against a per-instance tempdir.
    pub(crate) async fn new() -> Self {
        Self::build(true).await
    }

    /// Build a harness with session persistence disabled — no disk I/O at all.
    pub(crate) async fn new_no_persistence() -> Self {
        Self::build(false).await
    }

    async fn build(persistence_enabled: bool) -> Self {
        let memory = Arc::new(
            SemanticMemory::new(
                ":memory:",
                "http://127.0.0.1:1",
                None,
                AnyProvider::Mock(zeph_llm::mock::MockProvider::default()),
                "test-model",
            )
            .await
            .unwrap(),
        );

        let (session_dir, session_persistence_config) = if persistence_enabled {
            let dir = tempfile::tempdir().unwrap();
            let cfg = zeph_config::SessionConfig {
                enabled: true,
                data_dir: dir.path().to_string_lossy().into_owned(),
                ..Default::default()
            };
            (Some(dir), cfg)
        } else {
            let cfg = zeph_config::SessionConfig {
                enabled: false,
                ..Default::default()
            };
            (None, cfg)
        };

        let (resume_condenser, resume_token_counter) = make_test_condenser();
        let deps = ServeAgentDeps {
            provider: AnyProvider::Mock(zeph_llm::mock::MockProvider::default()),
            embedding_provider: AnyProvider::Mock(zeph_llm::mock::MockProvider::default()),
            registry: Arc::new(RwLock::new(zeph_skills::registry::SkillRegistry::empty())),
            matcher: None,
            max_active_skills: 0,
            skill_disambiguation_threshold: 0.2,
            skill_two_stage_matching: false,
            skill_confusability_threshold: 0.0,
            skill_group_structured: false,
            skill_support_similarity_threshold: 0.50,
            skill_min_injection_score: 0.20,
            skill_generation_provider: String::new(),
            skill_disambiguate_provider: String::new(),
            semantic_scan: false,
            semantic_scan_provider: String::new(),
            trust_config: zeph_core::config::TrustConfig::default(),
            rl_routing_enabled: false,
            rl_learning_rate: 0.0,
            rl_weight: 0.0,
            rl_persist_interval: 0,
            rl_warmup_updates: 0,
            rl_head: None,
            tool_executor: Arc::new(zeph_tools::SetCwdExecutor),
            capability_scopes_config: zeph_config::CapabilityScopesConfig::default(),
            permission_policy: zeph_tools::PermissionPolicy::default(),
            audit_logger: None,
            policy_gate_pieces: crate::agent_setup::PolicyGatePieces::default(),
            memory,
            history_limit: 50,
            recall_limit: 5,
            summarization_threshold: 100,
            session_config: zeph_core::AgentSessionConfig::from_config(
                &zeph_core::config::Config::default(),
                100_000,
            ),
            session_persistence_config,
            resume_condenser: Arc::new(resume_condenser),
            resume_token_counter: Arc::new(resume_token_counter),
            provider_pool: Vec::new(),
            provider_config_snapshot: zeph_core::ProviderConfigSnapshot::default(),
            shadow_sentinel_config: zeph_config::ShadowSentinelConfig::default(),
            shadow_sentinel_probe_provider: AnyProvider::Mock(
                zeph_llm::mock::MockProvider::default(),
            ),
            trajectory_sentinel_config: zeph_config::TrajectorySentinelConfig::default(),
            quality_pipeline: None,
        };

        let state = AppState {
            registry: Arc::new(LiveSessionRegistry::new()),
            started_at: std::time::Instant::now(),
            supervisor: zeph_common::task_supervisor::TaskSupervisor::new(
                tokio_util::sync::CancellationToken::new(),
            ),
            deps,
            mailbox_capacity: 8,
            max_sessions: 8,
            sanitizer: zeph_core::ContentSanitizer::new(
                &zeph_core::ContentIsolationConfig::default(),
            ),
        };

        Self {
            state,
            _session_dir: session_dir,
            auth_token: None,
            require_auth: false,
        }
    }

    /// Require a bearer token on every `/sessions*` route (mirrors `[serve] require_auth`).
    #[must_use]
    pub(crate) fn with_auth(mut self, token: &str) -> Self {
        self.auth_token = Some(token.to_owned());
        self.require_auth = true;
        self
    }

    /// Build the real production `axum::Router` — the actual object under test.
    pub(crate) fn router(&self) -> Router {
        super::router::build_router(
            self.state.clone(),
            self.auth_token.as_deref(),
            self.require_auth,
        )
    }

    /// Register a live session directly in the registry, bypassing `SessionActor::spawn` —
    /// mirrors `handlers::tests::insert_live_session`. Returns the receiving half of the
    /// session's command mailbox and the sender half of its output broadcast, so a test can both
    /// inspect what a handler sends (`prompt`) and push synthetic events for `events` to stream.
    pub(crate) fn insert_live_session(
        &self,
        id: &str,
    ) -> (
        tokio::sync::mpsc::Receiver<SessionCommand>,
        tokio::sync::broadcast::Sender<SessionOutput>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        let (tx_out, _sub) = tokio::sync::broadcast::channel(4);
        self.state.registry.insert(
            SessionId::new(id),
            SessionActorHandle {
                tx,
                tx_out: tx_out.clone(),
                last_active: std::time::Instant::now(),
                cancel: tokio_util::sync::CancellationToken::new(),
            },
        );
        (rx, tx_out)
    }
}

/// #5420 (N5, MUST — critic round 2): calls the actual production
/// `crate::acp::build_combined_deps` against a mock-provider `AppBuilder::for_test`, rather than
/// hand-reassembling a `ServeAgentDeps`/`SharedAgentDeps` pair from a shared core. Asserting
/// `Arc::ptr_eq` against a test-only re-assembly would be a tautology that only fails if someone
/// edits this same helper — calling the real production function proves `run_serve_with_acp`
/// actually shares one pool (critic round 1 S3 finding).
#[cfg(feature = "acp-http")]
pub(crate) async fn build_shared_pair() -> (super::deps::ServeAgentDeps, crate::acp::SharedAgentDeps)
{
    let mut config = zeph_core::config::Config::load(std::path::Path::new("/nonexistent")).unwrap();
    config.llm.providers = vec![zeph_core::config::ProviderEntry {
        provider_type: zeph_core::config::ProviderKind::Ollama,
        base_url: Some("http://127.0.0.1:1".to_owned()),
        model: Some("test-model".to_owned()),
        ..Default::default()
    }];
    config.memory.sqlite_path = ":memory:".to_owned();

    let app = crate::bootstrap::AppBuilder::for_test(config);
    let cancel = tokio_util::sync::CancellationToken::new();
    let supervisor = Arc::new(zeph_common::task_supervisor::TaskSupervisor::new(cancel));

    let (serve_deps, acp_deps, _keepalive) = crate::acp::build_combined_deps(&app, &supervisor)
        .await
        .expect("build_combined_deps must succeed against a mock-provider AppBuilder");
    (serve_deps, acp_deps)
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use super::*;

    /// Collects a response body into a `serde_json::Value`, panicking on malformed JSON — keeps
    /// every endpoint test's assertion focused on status/shape rather than parsing boilerplate.
    async fn json_body(response: axum::response::Response) -> serde_json::Value {
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn health_returns_200_with_no_auth_required() {
        let harness = ServeTestHarness::new_no_persistence().await;
        let response = harness
            .router()
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = json_body(response).await;
        assert_eq!(body["status"], "ok");
    }

    #[tokio::test]
    async fn create_session_returns_201_with_session_id() {
        let harness = ServeTestHarness::new().await;
        let response = harness
            .router()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/sessions")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let body = json_body(response).await;
        assert!(body["session_id"].as_str().is_some_and(|s| !s.is_empty()));
        assert!(body["conversation_id"].is_number());
    }

    #[tokio::test]
    async fn list_sessions_reflects_live_registry() {
        let harness = ServeTestHarness::new_no_persistence().await;
        harness.insert_live_session("s1");

        let response = harness
            .router()
            .oneshot(
                Request::builder()
                    .uri("/sessions")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = json_body(response).await;
        assert_eq!(body["sessions"].as_array().unwrap().len(), 1);
        assert_eq!(body["sessions"][0], "s1");
    }

    #[tokio::test]
    async fn get_session_unknown_id_returns_404() {
        let harness = ServeTestHarness::new_no_persistence().await;
        let response = harness
            .router()
            .oneshot(
                Request::builder()
                    .uri("/sessions/does-not-exist")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn get_session_after_create_returns_metadata_live_true() {
        let harness = ServeTestHarness::new().await;
        let router = harness.router();
        let create_response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/sessions")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let created = json_body(create_response).await;
        let session_id = created["session_id"].as_str().unwrap().to_owned();

        // Retry briefly: SessionStore::create runs on the actor's dedicated thread, so there is
        // a narrow window between the registry insert and the durable row landing (documented in
        // `get_session_handler`).
        let mut last_status = StatusCode::NOT_FOUND;
        let mut last_body = serde_json::Value::Null;
        for _ in 0..20 {
            let response = router
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(format!("/sessions/{session_id}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            last_status = response.status();
            if last_status == StatusCode::OK {
                last_body = json_body(response).await;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(last_status, StatusCode::OK);
        assert_eq!(last_body["live"], true);
    }

    #[tokio::test]
    async fn delete_session_then_404_on_second_delete() {
        let harness = ServeTestHarness::new_no_persistence().await;
        harness.insert_live_session("s1");
        let router = harness.router();

        let first = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/sessions/s1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::NO_CONTENT);

        let second = router
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/sessions/s1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(second.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn prompt_session_queues_sanitized_command() {
        let harness = ServeTestHarness::new_no_persistence().await;
        let (mut rx, _tx_out) = harness.insert_live_session("s1");

        let response = harness
            .router()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/sessions/s1/prompt")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"text":"hello there"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);

        let SessionCommand::Prompt { text } = rx.recv().await.unwrap() else {
            panic!("expected SessionCommand::Prompt");
        };
        assert!(text.contains("hello there"));
    }

    /// #5898 end-to-end: a recognized command posted over HTTP reaches the session mailbox
    /// raw, not wrapped in the sanitizer's `<external-data>` delimiter, so the agent's turn
    /// loop can dispatch it locally instead of silently falling through to a full chat turn.
    #[tokio::test]
    async fn prompt_session_forwards_recognized_command_raw() {
        let harness = ServeTestHarness::new_no_persistence().await;
        let (mut rx, _tx_out) = harness.insert_live_session("s1");

        let response = harness
            .router()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/sessions/s1/prompt")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"text":"/status"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);

        let SessionCommand::Prompt { text } = rx.recv().await.unwrap() else {
            panic!("expected SessionCommand::Prompt");
        };
        assert_eq!(text, "/status");
    }

    #[tokio::test]
    async fn prompt_session_unknown_id_returns_404() {
        let harness = ServeTestHarness::new_no_persistence().await;
        let response = harness
            .router()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/sessions/does-not-exist/prompt")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"text":"hello"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn events_session_streams_pushed_output() {
        let harness = ServeTestHarness::new_no_persistence().await;
        let (_rx, tx_out) = harness.insert_live_session("s1");

        let response = harness
            .router()
            .oneshot(
                Request::builder()
                    .uri("/sessions/s1/events")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        tx_out.send(SessionOutput::TurnComplete).unwrap();

        let mut stream = response.into_body().into_data_stream();
        let frame = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            use futures::StreamExt;
            stream.next().await
        })
        .await
        .expect("SSE frame must arrive before timeout")
        .expect("stream must yield at least one frame")
        .expect("frame read must not error");
        let text = String::from_utf8_lossy(&frame);
        assert!(
            text.contains("turn_complete"),
            "expected turn_complete SSE frame, got: {text}"
        );
    }

    #[tokio::test]
    async fn auth_required_rejects_missing_bearer_token() {
        let harness = ServeTestHarness::new_no_persistence()
            .await
            .with_auth("s3cr3t");

        let response = harness
            .router()
            .oneshot(
                Request::builder()
                    .uri("/sessions")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn auth_required_accepts_valid_bearer_token() {
        let harness = ServeTestHarness::new_no_persistence()
            .await
            .with_auth("s3cr3t");

        let response = harness
            .router()
            .oneshot(
                Request::builder()
                    .uri("/sessions")
                    .header(header::AUTHORIZATION, "Bearer s3cr3t")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn health_is_never_gated_by_auth() {
        let harness = ServeTestHarness::new_no_persistence()
            .await
            .with_auth("s3cr3t");

        let response = harness
            .router()
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[cfg(feature = "acp-http")]
    #[tokio::test]
    async fn build_combined_deps_shares_one_memory_pool() {
        let (serve_deps, acp_deps) = Box::pin(build_shared_pair()).await;
        assert!(
            std::sync::Arc::ptr_eq(&serve_deps.memory, &acp_deps.memory),
            "zeph serve-sessions --acp must share one SemanticMemory/SQLite pool between the \
             HTTP and ACP-HTTP transports, not build two independent ones"
        );
    }
}
