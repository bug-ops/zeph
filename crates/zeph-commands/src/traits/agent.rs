// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! [`AgentAccess`] — an empty marker supertrait that unifies 15 focused command-domain
//! sub-traits into a single object-safe trait, bridging `zeph-commands` handlers to
//! `zeph-core` subsystems that cannot be decomposed into smaller trait objects without
//! borrow-checker conflicts.
//!
//! ## Design rationale
//!
//! Commands like `/graph`, `/skill`, `/model`, `/policy`, and `/scheduler` access 10–20 internal
//! `Agent<C>` fields simultaneously. Decomposing each into a separate trait object field on
//! [`CommandContext`] would require splitting those fields from `&mut self.channel` (already
//! held by `ctx.sink`), which the borrow checker cannot express with safe Rust.
//!
//! The solution: one dispatch-facing trait object (`dyn AgentAccess`) assembled from 15
//! cohesive per-domain sub-traits (see `crate::traits`) via a blanket impl. Every method
//! ultimately delegates to the corresponding `Agent<C>` methods. Each sub-trait is
//! object-safe because every async method returns `Pin<Box<dyn Future + Send>>`; supertrait
//! methods are callable directly on a `dyn AgentAccess` value without any upcasting step.
//!
//! ## Implementors
//!
//! `zeph-core::Agent<C>` implements each of the 15 sub-traits (in `zeph-core::agent::*_commands`
//! modules) and receives `AgentAccess` for free via the blanket impl below.
//!
//! [`CommandContext`]: crate::context::CommandContext

use crate::traits::graph::GraphAccess;
use crate::traits::integration::IntegrationAccess;
use crate::traits::lsp::LspAccess;
use crate::traits::mcp::McpAccess;
use crate::traits::memory::MemoryAccess;
use crate::traits::misc::MiscAccess;
use crate::traits::model::ModelAccess;
use crate::traits::orchestration::OrchestrationAccess;
use crate::traits::policy::PolicyAccess;
use crate::traits::scheduler::SchedulerAccess;
use crate::traits::session_control::SessionControlAccess;
use crate::traits::skill::SkillAccess;
use crate::traits::subagent::SubagentAccess;
use crate::traits::tracking::TrackingAccess;
use crate::traits::worktree::WorktreeAccess;

/// Broad access to agent subsystems for command handlers that cannot be served by
/// individual sub-traits.
///
/// An empty marker supertrait unifying 15 focused command-domain sub-traits (see
/// `crate::traits`) into a single object-safe trait. Implemented automatically for any
/// type that implements all 15 sub-traits plus `Send` — see the blanket impl below. Never
/// implement this trait directly; implement the sub-traits instead.
///
/// All async methods across the 15 sub-traits return
/// `Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>>` (or similar) for
/// object safety — allowing `&mut dyn AgentAccess` storage in [`CommandContext`].
///
/// [`CommandContext`]: crate::context::CommandContext
pub trait AgentAccess:
    MemoryAccess
    + GraphAccess
    + ModelAccess
    + SkillAccess
    + PolicyAccess
    + SchedulerAccess
    + LspAccess
    + SessionControlAccess
    + McpAccess
    + OrchestrationAccess
    + SubagentAccess
    + IntegrationAccess
    + TrackingAccess
    + WorktreeAccess
    + MiscAccess
    + Send
{
}

impl<T> AgentAccess for T where
    T: MemoryAccess
        + GraphAccess
        + ModelAccess
        + SkillAccess
        + PolicyAccess
        + SchedulerAccess
        + LspAccess
        + SessionControlAccess
        + McpAccess
        + OrchestrationAccess
        + SubagentAccess
        + IntegrationAccess
        + TrackingAccess
        + WorktreeAccess
        + MiscAccess
        + Send
        + ?Sized
{
}

/// A no-op [`AgentAccess`] implementation.
///
/// Used when constructing a [`crate::CommandContext`] for a dispatch block that does not invoke
/// any agent-access commands (e.g., the session/debug-only registry block in `Agent::run`).
/// Allows the borrow checker to accept a split borrow: `sink` holds `&mut channel` while
/// `agent` holds this zero-size sentinel instead of `&mut self`.
///
/// Implements all 15 [`AgentAccess`] sub-traits (grouped below by trait, mirroring
/// `crate::traits`) so it obtains `AgentAccess` via the blanket impl above. Sub-traits whose
/// methods carry trait defaults matching this sentinel's desired no-op behavior are omitted
/// here and rely on the default (`handle_goal`, `active_goal_snapshot`, `handle_undo`,
/// `handle_redo`, `handle_web_search`, `handle_agents`, `handle_conv`, `list_worktrees`,
/// `clean_worktrees`, `change_working_directory`).
pub struct NullAgent;

impl MemoryAccess for NullAgent {
    fn memory_tiers<'a>(
        &'a mut self,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<String, crate::CommandError>> + Send + 'a>,
    > {
        Box::pin(async { Ok(String::new()) })
    }

    fn memory_promote<'a>(
        &'a mut self,
        _ids_str: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<String, crate::CommandError>> + Send + 'a>,
    > {
        Box::pin(async { Ok(String::new()) })
    }

    fn store_command<'a>(
        &'a mut self,
        _args: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<String, crate::CommandError>> + Send + 'a>,
    > {
        Box::pin(async { Ok(String::new()) })
    }

    fn guidelines<'a>(
        &'a mut self,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<String, crate::CommandError>> + Send + 'a>,
    > {
        Box::pin(async { Ok(String::new()) })
    }
}

impl GraphAccess for NullAgent {
    fn graph_stats<'a>(
        &'a mut self,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<String, crate::CommandError>> + Send + 'a>,
    > {
        Box::pin(async { Ok(String::new()) })
    }

    fn graph_entities<'a>(
        &'a mut self,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<String, crate::CommandError>> + Send + 'a>,
    > {
        Box::pin(async { Ok(String::new()) })
    }

    fn graph_facts<'a>(
        &'a mut self,
        _name: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<String, crate::CommandError>> + Send + 'a>,
    > {
        Box::pin(async { Ok(String::new()) })
    }

    fn graph_history<'a>(
        &'a mut self,
        _name: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<String, crate::CommandError>> + Send + 'a>,
    > {
        Box::pin(async { Ok(String::new()) })
    }

    fn graph_communities<'a>(
        &'a mut self,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<String, crate::CommandError>> + Send + 'a>,
    > {
        Box::pin(async { Ok(String::new()) })
    }

    fn graph_backfill<'a>(
        &'a mut self,
        _limit: Option<usize>,
        _progress_cb: &'a mut (dyn FnMut(String) + Send),
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<String, crate::CommandError>> + Send + 'a>,
    > {
        Box::pin(async { Ok(String::new()) })
    }

    fn knowledge_status<'a>(
        &'a mut self,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<String, crate::CommandError>> + Send + 'a>,
    > {
        Box::pin(async { Ok(String::new()) })
    }

    fn knowledge_rollback<'a>(
        &'a mut self,
        _batch_id: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<String, crate::CommandError>> + Send + 'a>,
    > {
        Box::pin(async { Ok(String::new()) })
    }
}

impl ModelAccess for NullAgent {
    fn handle_caveman<'a>(
        &'a mut self,
        _arg: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = String> + Send + 'a>> {
        Box::pin(async { "caveman: unavailable".to_owned() })
    }

    fn handle_model<'a>(
        &'a mut self,
        _arg: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = String> + Send + 'a>> {
        Box::pin(async { String::new() })
    }

    fn handle_provider<'a>(
        &'a mut self,
        _arg: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = String> + Send + 'a>> {
        Box::pin(async { String::new() })
    }

    fn handle_think_tokens<'a>(
        &'a mut self,
        _arg: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = String> + Send + 'a>> {
        Box::pin(async { String::new() })
    }

    fn handle_reasoning_effort<'a>(
        &'a mut self,
        _arg: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = String> + Send + 'a>> {
        Box::pin(async { String::new() })
    }
}

impl SkillAccess for NullAgent {
    fn handle_skill<'a>(
        &'a mut self,
        _args: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<String, crate::CommandError>> + Send + 'a>,
    > {
        Box::pin(async { Ok(String::new()) })
    }

    fn handle_skills<'a>(
        &'a mut self,
        _args: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<String, crate::CommandError>> + Send + 'a>,
    > {
        Box::pin(async { Ok(String::new()) })
    }

    fn handle_feedback_command<'a>(
        &'a mut self,
        _args: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<String, crate::CommandError>> + Send + 'a>,
    > {
        Box::pin(async { Ok(String::new()) })
    }
}

impl PolicyAccess for NullAgent {
    fn handle_policy<'a>(
        &'a mut self,
        _args: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<String, crate::CommandError>> + Send + 'a>,
    > {
        Box::pin(async { Ok(String::new()) })
    }
}

impl SchedulerAccess for NullAgent {
    fn list_scheduled_tasks<'a>(
        &'a mut self,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<Option<String>, crate::CommandError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async { Ok(None) })
    }
}

impl LspAccess for NullAgent {
    fn lsp_status<'a>(
        &'a mut self,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<String, crate::CommandError>> + Send + 'a>,
    > {
        Box::pin(async { Ok(String::new()) })
    }
}

impl SessionControlAccess for NullAgent {
    fn session_recap<'a>(
        &'a mut self,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<String, crate::CommandError>> + Send + 'a>,
    > {
        Box::pin(async { Ok(String::new()) })
    }

    fn compact_context<'a>(
        &'a mut self,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<String, crate::CommandError>> + Send + 'a>,
    > {
        Box::pin(async { Ok(String::new()) })
    }

    fn reset_conversation<'a>(
        &'a mut self,
        _keep_plan: bool,
        _no_digest: bool,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<String, crate::CommandError>> + Send + 'a>,
    > {
        Box::pin(async { Ok(String::new()) })
    }

    fn cache_stats(&self) -> String {
        String::new()
    }

    fn session_status<'a>(
        &'a mut self,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<String, crate::CommandError>> + Send + 'a>,
    > {
        Box::pin(async { Ok(String::new()) })
    }

    fn guardrail_status(&self) -> String {
        String::new()
    }

    fn focus_status(&self) -> String {
        String::new()
    }

    fn sidequest_status(&self) -> String {
        String::new()
    }

    fn load_image<'a>(
        &'a mut self,
        _path: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<String, crate::CommandError>> + Send + 'a>,
    > {
        Box::pin(async { Ok(String::new()) })
    }
}

impl McpAccess for NullAgent {
    fn handle_mcp<'a>(
        &'a mut self,
        _args: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<String, crate::CommandError>> + Send + 'a>,
    > {
        Box::pin(async { Ok(String::new()) })
    }
}

impl OrchestrationAccess for NullAgent {
    fn handle_plan<'a>(
        &'a mut self,
        _input: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<String, crate::CommandError>> + Send + 'a>,
    > {
        Box::pin(async { Ok(String::new()) })
    }

    fn handle_experiment<'a>(
        &'a mut self,
        _input: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<String, crate::CommandError>> + Send + 'a>,
    > {
        Box::pin(async { Ok(String::new()) })
    }
}

impl SubagentAccess for NullAgent {
    fn handle_agent_dispatch<'a>(
        &'a mut self,
        _input: &'a str,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<Option<String>, crate::CommandError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async { Ok(None) })
    }
}

impl IntegrationAccess for NullAgent {
    fn handle_plugins<'a>(
        &'a mut self,
        _args: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<String, crate::CommandError>> + Send + 'a>,
    > {
        Box::pin(async { Ok(String::new()) })
    }

    fn handle_acp<'a>(
        &'a mut self,
        _args: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<String, crate::CommandError>> + Send + 'a>,
    > {
        Box::pin(async { Ok(String::new()) })
    }

    fn handle_cocoon<'a>(
        &'a mut self,
        _args: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<String, crate::CommandError>> + Send + 'a>,
    > {
        Box::pin(async { Ok(String::new()) })
    }
}

impl TrackingAccess for NullAgent {
    fn handle_trajectory(&mut self, _args: &str) -> String {
        String::new()
    }

    fn handle_scope(&self, _args: &str) -> String {
        String::new()
    }
}

impl WorktreeAccess for NullAgent {}

impl MiscAccess for NullAgent {
    fn handle_loop<'a>(
        &'a mut self,
        _args: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<String, crate::CommandError>> + Send + 'a>,
    > {
        Box::pin(async { Ok(String::new()) })
    }

    /// Fire a test notification via all enabled notification channels.
    ///
    /// Returns a status message for the user. If all channels are disabled or the
    /// notifier is not configured, returns a user-visible explanation.
    ///
    /// # Errors
    ///
    /// Returns `Err` if the notification send fails.
    fn notify_test<'a>(
        &'a mut self,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<String, crate::CommandError>> + Send + 'a>,
    > {
        Box::pin(async { Ok("Notifications not configured.".to_owned()) })
    }
}
