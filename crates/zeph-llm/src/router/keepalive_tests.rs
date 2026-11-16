// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Backend selection of the router's `keepalive_ping` (spec 091 FR-005).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::RouterProvider;
use crate::any::AnyProvider;
use crate::mock::MockProvider;
use crate::provider::{
    KeepaliveOutcome, KeepaliveRequest, LlmProvider, Message, PrefixCache, ResponseUsage, Role,
};

type Recorded = Arc<Mutex<Vec<KeepaliveRequest>>>;

fn cached(name: &str) -> (AnyProvider, Recorded) {
    let (mock, recorded) = MockProvider::default()
        .with_name(name)
        .with_prefix_cache(PrefixCache::Prefix {
            ttl: Some(Duration::from_secs(300)),
        })
        .with_keepalive(vec![ResponseUsage::default()]);
    (AnyProvider::Mock(mock), recorded)
}

fn uncached(name: &str) -> AnyProvider {
    AnyProvider::Mock(MockProvider::default().with_name(name))
}

fn request(router: RouterProvider, target: Option<&str>) -> KeepaliveRequest {
    KeepaliveRequest {
        provider: AnyProvider::Router(Box::new(router)),
        messages: Arc::from(vec![Message::from_legacy(Role::User, "hi")]),
        tools: Arc::from(Vec::new()),
        target: target.map(zeph_common::ProviderName::new),
    }
}

async fn ping(router: RouterProvider, target: Option<&str>) -> KeepaliveOutcome {
    let req = request(router, target);
    req.provider.keepalive_ping(&req).await.unwrap()
}

#[tokio::test]
async fn target_names_the_backend_that_is_pinged() {
    let (a, seen_a) = cached("a");
    let (b, seen_b) = cached("b");
    let outcome = ping(RouterProvider::new(vec![a, b]), Some("b")).await;
    assert!(matches!(outcome, KeepaliveOutcome::Pinged(_)));
    assert!(seen_a.lock().unwrap().is_empty());
    assert_eq!(seen_b.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn the_ping_runs_on_the_backend_clone_not_the_router() {
    let (a, seen) = cached("a");
    ping(RouterProvider::new(vec![a]), None).await;
    let recorded = seen.lock().unwrap();
    assert_eq!(recorded[0].provider.name(), "a");
}

#[tokio::test]
async fn without_target_the_only_prefix_caching_backend_is_pinged() {
    let (b, seen) = cached("b");
    let outcome = ping(RouterProvider::new(vec![uncached("a"), b]), None).await;
    assert!(matches!(outcome, KeepaliveOutcome::Pinged(_)));
    assert_eq!(seen.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn without_target_and_several_caching_backends_the_ping_is_unsupported() {
    let (a, seen_a) = cached("a");
    let (b, seen_b) = cached("b");
    let outcome = ping(RouterProvider::new(vec![a, b]), None).await;
    assert!(matches!(outcome, KeepaliveOutcome::Unsupported));
    assert!(seen_a.lock().unwrap().is_empty() && seen_b.lock().unwrap().is_empty());
}

#[tokio::test]
async fn no_caching_backend_is_unsupported() {
    let outcome = ping(RouterProvider::new(vec![uncached("a")]), None).await;
    assert!(matches!(outcome, KeepaliveOutcome::Unsupported));
}

#[tokio::test]
async fn unknown_target_falls_back_to_the_only_caching_backend() {
    let (a, seen) = cached("a");
    let outcome = ping(RouterProvider::new(vec![a, uncached("b")]), Some("missing")).await;
    assert!(matches!(outcome, KeepaliveOutcome::Pinged(_)));
    assert_eq!(seen.lock().unwrap().len(), 1);
}
