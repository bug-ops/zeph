// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0
#![allow(clippy::large_futures, clippy::unwrap_used, clippy::expect_used)]
#![recursion_limit = "256"]

//! #6911: the prompt cache must be kept warm while one turn waits longer than the TTL after a
//! tool-use response (a long tool run, or an `ask_user` question the user has not answered yet).

use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use zeph_core::channel::{AskUserOutcome, AskUserRequest, Channel, ChannelError, ChannelMessage};
use zeph_core::{Agent, InteractionConfig};
use zeph_llm::any::AnyProvider;
use zeph_llm::mock::MockProvider;
use zeph_llm::provider::{
    CacheUsage, ChatResponse, KeepaliveRequest, PrefixCache, ResponseUsage, ToolUseRequest,
};
use zeph_skills::registry::SkillRegistry;
use zeph_tools::executor::{ToolError, ToolExecutor, ToolOutput};

const TTL: Duration = Duration::from_secs(300);
/// A long in-turn wait: longer than the TTL, as a long tool run would be.
const LONG_WAIT: Duration = Duration::from_secs(600);

struct OneShot {
    sent: bool,
    asked: Arc<std::sync::atomic::AtomicUsize>,
}

impl Channel for OneShot {
    fn ask_user_support(&self) -> zeph_core::channel::AskUserSupport {
        zeph_core::channel::AskUserSupport::Interactive
    }

    #[allow(clippy::unused_async_trait_impl)]
    async fn recv(&mut self) -> Result<Option<ChannelMessage>, ChannelError> {
        if self.sent {
            return Ok(None);
        }
        self.sent = true;
        Ok(Some(ChannelMessage {
            text: "deploy it".into(),
            attachments: vec![],
            is_guest_context: false,
            is_from_bot: false,
            owner_key: None,
        }))
    }
    #[allow(clippy::unused_async_trait_impl)]
    async fn send(&mut self, _text: &str) -> Result<(), ChannelError> {
        Ok(())
    }
    #[allow(clippy::unused_async_trait_impl)]
    async fn send_chunk(&mut self, _chunk: &str) -> Result<(), ChannelError> {
        Ok(())
    }
    fn flush_chunks(&mut self) -> impl Future<Output = Result<(), ChannelError>> + Send {
        std::future::ready(Ok(()))
    }
    async fn ask_user(&mut self, _r: AskUserRequest) -> Result<AskUserOutcome, ChannelError> {
        self.asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        tokio::time::sleep(LONG_WAIT).await;
        Ok(AskUserOutcome::answered_text("staging"))
    }
}

struct NoopExecutor;
impl ToolExecutor for NoopExecutor {
    fn execute(
        &self,
        _response: &str,
    ) -> impl Future<Output = Result<Option<ToolOutput>, ToolError>> + Send {
        std::future::ready(Ok(None))
    }
    fn set_skill_env(&self, _env: Option<std::collections::HashMap<String, String>>) {}
    zeph_tools::tool_executor_no_inner_defaults!();
}

fn hit() -> ResponseUsage {
    ResponseUsage {
        input: 10,
        output: 1,
        cache: Some(CacheUsage {
            creation: 0,
            read: 4_000,
        }),
        ..ResponseUsage::default()
    }
}

/// #6911 "cache goes cold during long tool runs": a turn whose first response asks for a tool
/// and then waits longer than the TTL must get a keepalive ping during that wait.
#[tokio::test(start_paused = true)]
async fn long_in_turn_wait_after_a_tool_use_response_is_kept_warm() {
    let asked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let dir = tempfile::tempdir().unwrap();
    let registry = SkillRegistry::load(&[dir.path().to_path_buf()]);
    let (mock, _count) = MockProvider::default()
        .with_name("main")
        .with_prefix_cache(PrefixCache::Prefix { ttl: Some(TTL) })
        .with_tool_use(vec![ChatResponse::tool_use(
            None,
            vec![ToolUseRequest {
                id: "c1".into(),
                name: "ask_user".into(),
                input: serde_json::json!({"question": "Which target?"}),
            }],
        )]);
    let (mock, pings): (MockProvider, Arc<Mutex<Vec<KeepaliveRequest>>>) =
        mock.with_keepalive(vec![hit()]);
    let mut cfg = zeph_core::config::Config::default();
    cfg.tools.ask_user.enabled = true;
    cfg.tools.ask_user.timeout_secs = zeph_config::AskUserTimeout::new(900).unwrap();
    cfg.llm.cache_keepalive.enabled = true;
    let mut agent = Agent::new(
        AnyProvider::Mock(mock),
        OneShot {
            sent: false,
            asked: Arc::clone(&asked),
        },
        registry,
        None,
        5,
        NoopExecutor,
    )
    .with_interaction_config(InteractionConfig::from_config(&cfg));
    agent.run().await.expect("agent run");
    assert_eq!(
        asked.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "question was asked"
    );
    assert!(
        !pings.lock().unwrap().is_empty(),
        "no keepalive ping during a {LONG_WAIT:?} in-turn wait (TTL {TTL:?})"
    );
}
