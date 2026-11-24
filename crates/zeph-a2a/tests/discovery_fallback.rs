// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0
//! Acceptance tests for #6932: discovery tries the A2A 1.0.0 `agent-card.json` path and falls
//! back to the legacy `agent.json` only when no card was evaluated (4xx or a non-card 2xx).

use std::time::Duration;

use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};
use zeph_a2a::{AgentCardBuilder, AgentRegistry, CardTrustPolicy};

const NEW: &str = "/.well-known/agent-card.json";
const LEGACY: &str = "/.well-known/agent.json";

fn registry() -> AgentRegistry {
    AgentRegistry::new(reqwest::Client::new(), Duration::from_mins(5))
        .with_request_timeout(Duration::from_secs(5))
}

fn card_json(server: &MockServer, name: &str) -> serde_json::Value {
    serde_json::to_value(AgentCardBuilder::new(name, server.uri(), "1.0.0").build()).unwrap()
}

async fn serve(server: &MockServer, p: &str, resp: ResponseTemplate) {
    Mock::given(method("GET"))
        .and(path(p))
        .respond_with(resp)
        .mount(server)
        .await;
}

#[tokio::test]
async fn test_discover_uses_agent_card_json_when_served() {
    let server = MockServer::start().await;
    let body = card_json(&server, "modern");
    serve(&server, NEW, ResponseTemplate::new(200).set_body_json(body)).await;
    let card = registry().discover(&server.uri()).await.unwrap();
    assert_eq!(card.name, "modern");
}

#[tokio::test]
async fn test_discover_falls_back_to_legacy_path_on_404() {
    let server = MockServer::start().await;
    let body = card_json(&server, "legacy");
    serve(&server, NEW, ResponseTemplate::new(404)).await;
    serve(
        &server,
        LEGACY,
        ResponseTemplate::new(200).set_body_json(body),
    )
    .await;
    let card = registry().discover(&server.uri()).await.unwrap();
    assert_eq!(card.name, "legacy");
}

#[tokio::test]
async fn test_discover_falls_back_on_any_4xx() {
    let server = MockServer::start().await;
    let body = card_json(&server, "legacy");
    serve(&server, NEW, ResponseTemplate::new(405)).await;
    serve(
        &server,
        LEGACY,
        ResponseTemplate::new(200).set_body_json(body),
    )
    .await;
    assert_eq!(
        registry().discover(&server.uri()).await.unwrap().name,
        "legacy"
    );
}

#[tokio::test]
async fn test_discover_falls_back_when_2xx_body_is_not_a_card() {
    let server = MockServer::start().await;
    let body = card_json(&server, "legacy");
    serve(
        &server,
        NEW,
        ResponseTemplate::new(200).set_body_string("<html>spa</html>"),
    )
    .await;
    serve(
        &server,
        LEGACY,
        ResponseTemplate::new(200).set_body_json(body),
    )
    .await;
    assert_eq!(
        registry().discover(&server.uri()).await.unwrap().name,
        "legacy"
    );
}

#[tokio::test]
async fn test_discover_does_not_fall_back_on_5xx() {
    let server = MockServer::start().await;
    let body = card_json(&server, "legacy");
    serve(&server, NEW, ResponseTemplate::new(503)).await;
    serve(
        &server,
        LEGACY,
        ResponseTemplate::new(200).set_body_json(body),
    )
    .await;
    assert!(registry().discover(&server.uri()).await.is_err());
    let hits = server.received_requests().await.unwrap();
    assert!(hits.iter().all(|r| r.url.path() == NEW), "{hits:?}");
}

#[tokio::test]
async fn test_discover_does_not_fall_back_on_trust_failure() {
    let server = MockServer::start().await;
    let mut bad = card_json(&server, "mismatch");
    bad["url"] = serde_json::json!("http://other.invalid");
    let good = card_json(&server, "legacy");
    serve(&server, NEW, ResponseTemplate::new(200).set_body_json(bad)).await;
    serve(
        &server,
        LEGACY,
        ResponseTemplate::new(200).set_body_json(good),
    )
    .await;
    let reg = registry().with_trust(CardTrustPolicy::Require, vec![]);
    assert!(reg.discover(&server.uri()).await.is_err());
    let hits = server.received_requests().await.unwrap();
    assert!(hits.iter().all(|r| r.url.path() == NEW), "{hits:?}");
}
