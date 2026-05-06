// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Live testnet integration test for the Gonka provider.
//!
//! Skipped by default — requires a running Gonka testnet node and a funded wallet.
//! Run with:
//! ```shell
//! ZEPH_GONKA_PRIVATE_KEY=<hex> cargo nextest run -p zeph-llm -- --ignored gonka_live
//! ```

#[cfg(feature = "gonka")]
mod live {
    use std::sync::Arc;
    use std::time::Duration;

    use zeph_llm::gonka::endpoints::{EndpointPool, GonkaEndpoint};
    use zeph_llm::gonka::{GonkaProvider, RequestSigner};
    use zeph_llm::provider::{LlmProvider, Message, Role};

    #[tokio::test]
    #[ignore = "requires ZEPH_GONKA_PRIVATE_KEY env var and live Gonka testnet access"]
    async fn gonka_live_chat_round_trip() {
        let priv_key = match std::env::var("ZEPH_GONKA_PRIVATE_KEY") {
            Ok(k) if !k.is_empty() => k,
            _ => {
                eprintln!("ZEPH_GONKA_PRIVATE_KEY not set, skipping");
                return;
            }
        };

        let node_url = std::env::var("ZEPH_GONKA_NODE_URL")
            .unwrap_or_else(|_| "http://node1.gonka.ai:8000".into());

        let signer = Arc::new(
            RequestSigner::from_hex(&priv_key, "gonka").expect("valid secp256k1 private key"),
        );

        let pool = Arc::new(
            EndpointPool::new(vec![GonkaEndpoint {
                base_url: node_url.clone(),
                address: signer.address().to_owned(),
            }])
            .expect("non-empty pool"),
        );

        let provider =
            GonkaProvider::new(signer, pool, "gpt-4o", 16, None, Duration::from_secs(30));

        let messages = vec![Message::from_legacy(
            Role::User,
            "Say hello in one word.".to_owned(),
        )];

        let response = provider
            .chat(&messages)
            .await
            .expect("chat should succeed against live testnet");

        assert!(!response.is_empty(), "response must not be empty");
    }
}
