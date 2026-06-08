// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `POST /deep-link` handler for the ACP HTTP transport.
//!
//! This endpoint parses and validates a `zeph://` URI supplied by an IDE, then returns
//! the extracted, validated fields as JSON. It is **stateless** and performs **no store
//! writes** — no session is created, no `create_acp_session` is called.
//!
//! # Advisory contract (MANDATORY)
//!
//! This endpoint is **advisory**. It validates and normalizes a `zeph://` URI and returns
//! the cleaned data for the caller to act on. The authoritative cwd-enforcement point for
//! the ACP runtime is the per-connection `session/new` / `session/load` boundary
//! (`args.cwd`), which is unchanged by this feature. A client that ignores the returned
//! `working_dir` and passes a different `args.cwd` to `session/new` is subject only to that
//! path's existing `file://` / `additional_directories` checks.

#![cfg(feature = "acp-http")]

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use zeph_common::deep_link::{parse_deep_link, validate_deep_link_cwd};

use crate::transport::http::AcpHttpState;

/// Maximum total URI length accepted before parsing.
///
/// `parse_deep_link` bounds only the decoded prompt (8192 bytes), not the raw URI.
/// A URI with a giant `cwd` or `profile` component could reach `canonicalize` without
/// this guard (M2 from the architecture review).
const MAX_URI_BYTES: usize = 65_536;

// ── Request / response types ──────────────────────────────────────────────────

/// Request body for `POST /deep-link`.
#[derive(Debug, Deserialize)]
pub struct DeepLinkRequest {
    /// The `zeph://` URI to parse and validate.
    pub uri: String,
}

/// Response body for a successful `POST /deep-link`.
///
/// All fields are optional: only the URI components present and validated are populated.
#[derive(Debug, Serialize)]
pub struct DeepLinkResponse {
    /// Canonicalized working directory extracted from the URI, or `null` if not present.
    pub working_dir: Option<PathBuf>,
    /// Decoded prompt text from the URI, or `null` if not present.
    pub prompt: Option<String>,
    /// Trust level of the inbound prompt. Always `"external_untrusted"` for URI-sourced
    /// prompts — this label is advisory for the IDE; enforcement is the caller's responsibility.
    pub prompt_trust_level: &'static str,
    /// Validated model/provider name from the URI, or `null` if not present.
    pub model: Option<String>,
}

// ── Error body ────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub(crate) struct ErrorBody {
    error: &'static str,
    detail: String,
}

fn bad_request(detail: impl Into<String>) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(ErrorBody {
            error: "bad_request",
            detail: detail.into(),
        }),
    )
        .into_response()
}

fn forbidden() -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(ErrorBody {
            error: "forbidden",
            // Generic body: avoids leaking whether the path exists or is merely outside the allowlist.
            detail: "working directory rejected".to_owned(),
        }),
    )
        .into_response()
}

fn unprocessable(detail: impl Into<String>) -> Response {
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(ErrorBody {
            error: "unprocessable_entity",
            detail: detail.into(),
        }),
    )
        .into_response()
}

fn internal_error(detail: impl Into<String>) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ErrorBody {
            error: "internal_server_error",
            detail: detail.into(),
        }),
    )
        .into_response()
}

// ── Handler ───────────────────────────────────────────────────────────────────

/// `POST /deep-link` — parse and validate a `zeph://` URI (stateless, advisory).
///
/// Accepts a JSON body with a `uri` field containing a `zeph://` URI, parses it,
/// validates the `cwd` against both the deep-link denylist and the ACP
/// `additional_directories` allowlist, validates the model name against the known
/// provider list, and returns the cleaned fields.
///
/// **This endpoint is advisory.** It does not create a session or write any state.
/// The authoritative cwd-enforcement point is `session/new` (`args.cwd`). See the
/// module-level doc for the full advisory contract.
///
/// # Status codes
///
/// | Code | Condition |
/// |------|-----------|
/// | 200  | URI parsed and validated successfully |
/// | 400  | Malformed URI, unknown host action, URI exceeds 65 536 bytes, NUL/C0 in cwd, prompt too long |
/// | 403  | `cwd` rejected by denylist or `additional_directories` allowlist |
/// | 422  | URI is valid but the model/provider name is unknown |
/// | 401  | Missing or invalid bearer token (enforced by `BearerAuthLayer`) |
///
/// # Errors
///
/// Returns non-200 status with a JSON `{"error": "...", "detail": "..."}` body on failure.
pub async fn deep_link_handler(
    State(state): State<AcpHttpState>,
    Json(req): Json<DeepLinkRequest>,
) -> Response {
    // M2: cap total URI length before any parsing to prevent a giant cwd from reaching canonicalize.
    if req.uri.len() > MAX_URI_BYTES {
        return bad_request(format!(
            "URI exceeds maximum length of {MAX_URI_BYTES} bytes"
        ));
    }

    // Parse the URI.
    let link = match parse_deep_link(&req.uri) {
        Ok(l) => l,
        Err(e) => return bad_request(e.to_string()),
    };

    let zeph_common::deep_link::DeepLink::NewSession(params) = link;

    // M3: check decoded cwd for NUL bytes and C0 control characters before PathBuf construction.
    // `parse_deep_link` already built a PathBuf, but we re-check the raw string value here
    // because PathBuf on some platforms silently strips or transforms NUL-containing paths.
    if let Some(ref cwd) = params.cwd {
        let cwd_str = cwd.to_string_lossy();
        if cwd_str
            .bytes()
            .any(|b| b == 0 || (b < 0x20 && !matches!(b, 0x09 | 0x0a | 0x0d)))
        {
            return bad_request("cwd contains disallowed control characters");
        }
    }

    // Validate model name against available providers if specified.
    if let Some(ref model) = params.model {
        let models = state.server_config.available_models.read();
        if !models.is_empty() && !models.iter().any(|m| m == model) {
            return unprocessable(format!("unknown model '{model}'"));
        }
    }

    // Validate and canonicalize cwd if present, applying two gates in order:
    // 1. deep-link INV-CWD denylist via `validate_deep_link_cwd`
    // 2. ACP `additional_directories` allowlist (default-deny: empty list rejects all cwd)
    let working_dir = if let Some(ref cwd) = params.cwd {
        let allowlist = &state.server_config.additional_directories;

        // Gate 2 (allowlist) checked first: empty allowlist means no directory is permitted.
        if allowlist.is_empty() {
            tracing::debug!(cwd = %cwd.display(), "deep-link cwd rejected: additional_directories allowlist is empty");
            return forbidden();
        }

        let allowed_roots: Vec<PathBuf> = allowlist
            .iter()
            .map(|d| d.as_path().to_path_buf())
            .collect();

        // Run both gates. Map all failure variants to 403 with a generic body to
        // avoid leaking filesystem-existence information via distinct status codes.
        let join_result = tokio::task::spawn_blocking({
            let cwd = cwd.clone();
            move || validate_deep_link_cwd(&cwd, &allowed_roots)
        })
        .await;

        let validation_result = match join_result {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "spawn_blocking panicked in validate_deep_link_cwd");
                return internal_error("internal validation error");
            }
        };

        match validation_result {
            Ok(canonical) => Some(canonical),
            Err(e) => {
                tracing::debug!(error = %e, cwd = %cwd.display(), "deep-link cwd validation rejected");
                return forbidden();
            }
        }
    } else {
        None
    };

    Json(DeepLinkResponse {
        working_dir,
        prompt: params.prompt,
        prompt_trust_level: "external_untrusted",
        model: params.model,
    })
    .into_response()
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::pin::Pin;
    use std::sync::Arc;

    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::post;
    use tower::ServiceExt as _;
    use zeph_core::channel::LoopbackChannel;

    use super::*;
    use crate::agent::{AcpContext, SendAgentSpawner, SessionContext};
    use crate::transport::{AcpServerConfig, SharedAvailableModels};

    fn shared_models_empty() -> SharedAvailableModels {
        Arc::new(parking_lot::RwLock::new(vec![]))
    }

    fn shared_models_with(names: &[&str]) -> SharedAvailableModels {
        Arc::new(parking_lot::RwLock::new(
            names.iter().map(std::string::ToString::to_string).collect(),
        ))
    }

    fn noop_spawner() -> SendAgentSpawner {
        Arc::new(
            |_ch: LoopbackChannel, _ctx: Option<AcpContext>, _sess: SessionContext| {
                Box::pin(async {})
                    as Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>
            },
        )
    }

    fn build_router(state: AcpHttpState) -> Router {
        Router::new()
            .route("/deep-link", post(deep_link_handler))
            .with_state(state)
    }

    fn make_state(models: SharedAvailableModels) -> AcpHttpState {
        make_state_with_config(
            models,
            AcpServerConfig {
                agent_name: "test".into(),
                agent_version: "0.0.1".into(),
                ..AcpServerConfig::default()
            },
        )
    }

    fn make_state_with_config(
        models: SharedAvailableModels,
        mut config: AcpServerConfig,
    ) -> AcpHttpState {
        config.available_models = models;
        AcpHttpState::new(noop_spawner(), config).with_ready(true)
    }

    async fn post_deep_link(app: Router, body: serde_json::Value) -> axum::response::Response {
        app.oneshot(
            Request::builder()
                .method("POST")
                .uri("/deep-link")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn malformed_uri_returns_400() {
        let app = build_router(make_state(shared_models_empty()));
        let resp = post_deep_link(app, serde_json::json!({ "uri": "not-a-uri" })).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn wrong_scheme_returns_400() {
        let app = build_router(make_state(shared_models_empty()));
        let resp = post_deep_link(app, serde_json::json!({ "uri": "http://example.com" })).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn unknown_host_returns_400() {
        let app = build_router(make_state(shared_models_empty()));
        let resp = post_deep_link(app, serde_json::json!({ "uri": "zeph://unknown-action" })).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn oversized_uri_returns_400() {
        let app = build_router(make_state(shared_models_empty()));
        let giant_uri = format!("zeph://new-session?cwd=/{}", "a".repeat(MAX_URI_BYTES));
        let resp = post_deep_link(app, serde_json::json!({ "uri": giant_uri })).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn unknown_model_returns_422() {
        let app = build_router(make_state(shared_models_with(&["fast", "quality"])));
        let resp = post_deep_link(
            app,
            serde_json::json!({ "uri": "zeph://new-session?model=nonexistent" }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn cwd_outside_empty_allowlist_returns_403() {
        // additional_directories is empty → any non-None cwd must be rejected (default-deny).
        let app = build_router(make_state(shared_models_empty()));
        let tmp = std::env::temp_dir();
        let uri = format!("zeph://new-session?cwd={}", tmp.display());
        let resp = post_deep_link(app, serde_json::json!({ "uri": uri })).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn no_cwd_in_uri_returns_200() {
        let app = build_router(make_state(shared_models_empty()));
        let resp = post_deep_link(
            app,
            serde_json::json!({ "uri": "zeph://new-session?prompt=hi" }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["prompt"], "hi");
        assert_eq!(body["prompt_trust_level"], "external_untrusted");
        assert!(body["working_dir"].is_null());
    }

    #[tokio::test]
    async fn bare_new_session_returns_200_with_nulls() {
        let app = build_router(make_state(shared_models_empty()));
        let resp = post_deep_link(app, serde_json::json!({ "uri": "zeph://new-session" })).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(body["working_dir"].is_null());
        assert!(body["prompt"].is_null());
        assert_eq!(body["prompt_trust_level"], "external_untrusted");
        assert!(body["model"].is_null());
    }

    #[tokio::test]
    async fn known_model_with_empty_list_returns_200() {
        // When available_models list is empty, model validation is skipped.
        let app = build_router(make_state(shared_models_empty()));
        let resp = post_deep_link(
            app,
            serde_json::json!({ "uri": "zeph://new-session?model=anything" }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["model"], "anything");
    }

    #[tokio::test]
    async fn cwd_outside_nonempty_allowlist_returns_403() {
        use zeph_core::config::AdditionalDir;

        // Create two distinct real temp dirs so AdditionalDir::parse (which canonicalizes) succeeds.
        let allowed_dir = tempfile::TempDir::new().unwrap();
        let cwd_dir = tempfile::TempDir::new().unwrap();

        // The allowed root is `allowed_dir`; cwd points to `cwd_dir` which is outside it.
        let config = AcpServerConfig {
            agent_name: "test".into(),
            agent_version: "0.0.1".into(),
            additional_directories: vec![AdditionalDir::parse(allowed_dir.path()).unwrap()],
            ..AcpServerConfig::default()
        };
        let app = build_router(make_state_with_config(shared_models_empty(), config));
        let uri = format!("zeph://new-session?cwd={}", cwd_dir.path().display());
        let resp = post_deep_link(app, serde_json::json!({ "uri": uri })).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn nul_byte_in_cwd_returns_400() {
        use zeph_core::config::AdditionalDir;

        // Provide a non-empty allowlist so the empty-allowlist gate does not fire first.
        let allowed_dir = tempfile::TempDir::new().unwrap();
        let config = AcpServerConfig {
            agent_name: "test".into(),
            agent_version: "0.0.1".into(),
            additional_directories: vec![AdditionalDir::parse(allowed_dir.path()).unwrap()],
            ..AcpServerConfig::default()
        };
        let app = build_router(make_state_with_config(shared_models_empty(), config));
        // `%2F%00evil` decodes to `/\x00evil` — NUL byte in cwd triggers the M3 guard.
        let resp = post_deep_link(
            app,
            serde_json::json!({ "uri": "zeph://new-session?cwd=%2F%00evil" }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }
}
