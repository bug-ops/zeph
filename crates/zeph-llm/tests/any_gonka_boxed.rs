// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0
#![cfg(feature = "gonka")]
#![expect(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "Fixture construction failures should fail the test immediately."
)]

//! Acceptance test (tests-first) for #7147 (D2): `AnyProvider::Gonka` carries a boxed
//! `GonkaProvider`, so the variant no longer inflates `AnyProvider` and needs no feature-conditional
//! `expect`.

use std::sync::Arc;
use std::time::Duration;

use zeph_llm::any::AnyProvider;
use zeph_llm::gonka::endpoints::{EndpointPool, GonkaEndpoint};
use zeph_llm::gonka::{GonkaConfig, GonkaProvider, RequestSigner};
use zeph_llm::provider::LlmProvider;

#[test]
fn any_provider_gonka_variant_holds_boxed_provider() {
    let signer = Arc::new(
        RequestSigner::from_hex(
            "0101010101010101010101010101010101010101010101010101010101010101",
            "gonka",
        )
        .expect("valid secp256k1 private key"),
    );
    let pool = Arc::new(
        EndpointPool::new(vec![GonkaEndpoint {
            base_url: "https://node1.example.com".into(),
            address: "gonka1w508d6qejxtdg4y5r3zarvary0c5xw7k2gsyg6".into(),
        }])
        .unwrap(),
    );
    let provider = GonkaProvider::new(GonkaConfig {
        signer,
        pool,
        model: "gpt-4o".into(),
        max_tokens: 4096,
        embedding_model: None,
        timeout: Duration::from_secs(30),
    });

    let any = AnyProvider::Gonka(Box::new(provider));

    assert_eq!(any.name(), "gonka");
}
