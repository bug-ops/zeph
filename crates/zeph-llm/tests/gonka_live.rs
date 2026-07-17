// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Live integration tests for the Gonka network, exercised via two independent transports.
//!
//! `gonka_live_chat_round_trip` drives the native, request-signing `GonkaProvider` against a
//! raw testnet node. Skipped by default — requires a funded wallet and a *reachable* node URL:
//! there is no fixed, globally-reachable testnet endpoint, so `ZEPH_GONKA_NODE_URL` must be set
//! explicitly to a node that is actually up from the current network vantage point (there is no
//! built-in default — both known candidate hosts have been proven unreachable, see #5549). Run
//! with:
//! ```shell
//! ZEPH_GONKA_PRIVATE_KEY=<hex> ZEPH_GONKA_NODE_URL=<url> \
//!     cargo nextest run -p zeph-llm --features gonka -- --ignored gonka_live
//! ```
//!
//! `gonkagate_compatible_chat_round_trip` drives the same Gonka network via the OpenAI-compatible
//! `gonkagate` gateway (`https://api.gonkagate.com/v1`), the same route used by the `--init`
//! wizard's "gonkagate" preset. Run with:
//! ```shell
//! ZEPH_COMPATIBLE_GONKAGATE_API_KEY=<key> cargo nextest run -p zeph-llm -- --ignored gonkagate_compatible
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

        let node_url = match std::env::var("ZEPH_GONKA_NODE_URL") {
            Ok(u) if !u.is_empty() => u,
            _ => {
                eprintln!(
                    "ZEPH_GONKA_NODE_URL not set to a reachable node, skipping — neither the \
                     old built-in default (`http://node1.gonka.ai:8000`) nor the repo-documented \
                     example (`https://node1.gonka.ai`) are known-reachable (#5549), so this test \
                     no longer guesses a node URL"
                );
                return;
            }
        };

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

        let provider = GonkaProvider::new(zeph_llm::gonka::GonkaConfig {
            signer,
            pool,
            model: "gpt-4o".into(),
            max_tokens: 16,
            embedding_model: None,
            timeout: Duration::from_secs(30),
        });

        let messages = vec![Message::from_legacy(
            Role::User,
            "Say hello in one word.".to_owned(),
        )];

        match provider.chat(&messages).await {
            Ok(response) => assert!(!response.is_empty(), "response must not be empty"),
            Err(err) => panic!(
                "chat request against Gonka testnet node `{node_url}` failed: {err}\n\
                 ZEPH_GONKA_NODE_URL is not guaranteed to point at a live node — confirm it is \
                 actually up (e.g. `curl {node_url}`) before treating this as a regression. To \
                 check whether the Gonka network itself is reachable independent of node URL, \
                 run `gonkagate_compatible_chat_round_trip` in this file, which reaches the \
                 same network through the OpenAI-compatible gonkagate gateway."
            ),
        }
    }
}

mod compatible_live {
    use zeph_llm::compatible::{CompatibleConfig, CompatibleProvider};
    use zeph_llm::provider::{LlmProvider, Message, Role};

    /// Live smoke test for the `gonkagate` OpenAI-compatible gateway
    /// (`https://api.gonkagate.com/v1`), the same preset offered by the `--init` wizard
    /// (`src/init/llm.rs`, option 5). Unlike the native, request-signing `GonkaProvider`
    /// path in `gonka_live_chat_round_trip`, this transport does not depend on locating a
    /// currently-reachable raw testnet node.
    #[tokio::test]
    #[ignore = "requires ZEPH_COMPATIBLE_GONKAGATE_API_KEY env var and live gonkagate access"]
    async fn gonkagate_compatible_chat_round_trip() {
        let api_key = match std::env::var("ZEPH_COMPATIBLE_GONKAGATE_API_KEY") {
            Ok(k) if !k.is_empty() => k,
            _ => {
                eprintln!("ZEPH_COMPATIBLE_GONKAGATE_API_KEY not set, skipping");
                return;
            }
        };

        let provider = CompatibleProvider::new(CompatibleConfig {
            provider_name: "gonkagate".into(),
            api_key,
            base_url: "https://api.gonkagate.com/v1".into(),
            // Catalog-dependent: confirmed present in gonkagate's `/v1/models` list as of
            // #5549's investigation, but hosted-gateway model catalogs can change over time.
            model: "moonshotai/kimi-k2.6".into(),
            max_tokens: 16,
            embedding_model: None,
            completion_tokens_param: None,
            vision: None,
        });

        let messages = vec![Message::from_legacy(
            Role::User,
            "Say hello in one word.".to_owned(),
        )];

        match provider.chat(&messages).await {
            Ok(response) => assert!(!response.is_empty(), "response must not be empty"),
            Err(zeph_llm::error::LlmError::RateLimited) => {
                // `send_with_retry` maps both HTTP 429 (throttled) and 503 (gateway down)
                // to this same variant after retries are exhausted, so this arm cannot tell
                // "account is busy" apart from "gateway is down" — only a real `Ok` response
                // confirms the endpoint, auth, and routing are actually functional.
                eprintln!(
                    "gonkagate gateway is throttled or temporarily unavailable \
                     (HTTP 429/503) — treating as a transient condition rather than a test \
                     failure, without asserting reachability or auth are confirmed."
                );
            }
            Err(err) => panic!("chat request against gonkagate gateway failed: {err}"),
        }
    }
}
