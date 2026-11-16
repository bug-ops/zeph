// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Acceptance tests for guest-proxy upstream routing (#7079).
//!
//! Registered from `telegram.rs` with `#[cfg(test)] mod guest_proxy_route_tests;`.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use tokio::sync::mpsc;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{
    BodyLimit, GuestProxyState, TelegramAllowlist, UpstreamRoute, proxy_handler, route_upstream,
};

const BOT: &str = "bot123:ABC";

fn state_for(server: &MockServer) -> GuestProxyState {
    let (tx, _rx) = mpsc::channel(1);
    GuestProxyState {
        upstream_origin: server.uri(),
        bot_segment: BOT.to_owned(),
        client: reqwest::Client::new(),
        tx,
        allowlist: Arc::new(TelegramAllowlist::default()),
        body_limit: BodyLimit::from_const(1024),
    }
}

async fn send(server: &MockServer, uri: &str) -> StatusCode {
    let req = Request::builder()
        .uri(uri)
        .body(Body::empty())
        .expect("valid request");
    proxy_handler(State(state_for(server)), req).await.status()
}

#[test]
fn route_upstream_method_shape_returns_method() {
    let route = route_upstream("/bot123:ABC/bot123:ABC/getMe", BOT);
    assert!(matches!(route, Some(UpstreamRoute::Method("getMe"))));
}

#[test]
fn route_upstream_file_shape_returns_verbatim_encoded_path() {
    let route = route_upstream("/bot123:ABC/file/bot123:ABC/photos%2Ffile_1.jpg", BOT);
    assert!(matches!(
        route,
        Some(UpstreamRoute::File("photos%2Ffile_1.jpg"))
    ));
}

#[test]
fn route_upstream_rejects_malformed_shapes() {
    for p in [
        "/bot999:ZZZ/bot999:ZZZ/getMe",
        "/bot123:ABC/getMe",
        "/bot123:ABC/file/bot123:ABC/",
        "/bot123:ABC/bot123:ABC/",
        "/bot123:ABC/file/bot999:ZZZ/x.jpg",
        "/x",
        "",
    ] {
        assert!(route_upstream(p, BOT).is_none(), "must reject {p:?}");
    }
}

#[tokio::test]
async fn proxy_handler_method_request_hits_upstream_method_path() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/bot123:ABC/getMe"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    assert_eq!(
        send(&server, "/bot123:ABC/bot123:ABC/getMe").await,
        StatusCode::OK
    );
}

#[tokio::test]
async fn proxy_handler_file_request_hits_upstream_file_path_byte_exact() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/file/bot123:ABC/photos%2Ff.jpg"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![1u8, 2, 3]))
        .expect(1)
        .mount(&server)
        .await;
    assert_eq!(
        send(&server, "/bot123:ABC/file/bot123:ABC/photos%2Ff.jpg").await,
        StatusCode::OK
    );
}

#[tokio::test]
async fn proxy_handler_invalid_shapes_return_404_without_upstream_call() {
    let server = MockServer::start().await;
    Mock::given(wiremock::matchers::any())
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    for uri in [
        "/bot999:ZZZ/bot999:ZZZ/getMe",
        "/bot123:ABC/getMe",
        "/bot123:ABC/file/bot123:ABC/",
        "/x",
    ] {
        assert_eq!(send(&server, uri).await, StatusCode::NOT_FOUND, "{uri}");
    }
}
