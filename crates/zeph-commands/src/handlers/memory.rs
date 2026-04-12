// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Memory command handlers: `/memory`, `/graph`, `/guidelines`.

use std::future::Future;
use std::pin::Pin;

use crate::context::CommandContext;
use crate::{CommandError, CommandHandler, CommandOutput, SlashCategory};

/// Display memory tier statistics or promote messages to the semantic tier.
///
/// Subcommands: (none or `tiers`) shows stats; `promote <id>...` promotes messages.
pub struct MemoryCommand;

impl CommandHandler<CommandContext<'_>> for MemoryCommand {
    fn name(&self) -> &'static str {
        "/memory"
    }

    fn description(&self) -> &'static str {
        "Show memory tier stats or manually promote messages to semantic tier"
    }

    fn args_hint(&self) -> &'static str {
        "[tiers|promote <id>...]"
    }

    fn category(&self) -> SlashCategory {
        SlashCategory::Memory
    }

    fn handle<'a>(
        &'a self,
        ctx: &'a mut CommandContext<'_>,
        args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, CommandError>> + Send + 'a>> {
        Box::pin(async move {
            let result = if args.is_empty() || args == "tiers" {
                ctx.agent.memory_tiers().await?
            } else if let Some(rest) = args.strip_prefix("promote") {
                ctx.agent.memory_promote(rest.trim()).await?
            } else {
                "Unknown /memory subcommand. Available: /memory tiers, /memory promote <id>..."
                    .to_owned()
            };
            Ok(CommandOutput::Message(result))
        })
    }
}

/// Query and manage the knowledge graph.
///
/// Subcommands: (none) stats; `entities`; `facts <name>`; `history <name>`;
/// `communities`; `backfill [--limit N]`.
pub struct GraphCommand;

impl CommandHandler<CommandContext<'_>> for GraphCommand {
    fn name(&self) -> &'static str {
        "/graph"
    }

    fn description(&self) -> &'static str {
        "Query or manage the knowledge graph"
    }

    fn args_hint(&self) -> &'static str {
        "[subcommand]"
    }

    fn category(&self) -> SlashCategory {
        SlashCategory::Memory
    }

    fn handle<'a>(
        &'a self,
        ctx: &'a mut CommandContext<'_>,
        args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, CommandError>> + Send + 'a>> {
        Box::pin(async move {
            let result = if args.is_empty() {
                ctx.agent.graph_stats().await?
            } else if args == "entities" || args.starts_with("entities ") {
                ctx.agent.graph_entities().await?
            } else if let Some(name) = args.strip_prefix("facts ") {
                ctx.agent.graph_facts(name.trim()).await?
            } else if args == "communities" {
                ctx.agent.graph_communities().await?
            } else if args == "backfill" || args.starts_with("backfill ") {
                let limit = parse_backfill_limit(args);
                let mut progress_messages: Vec<String> = Vec::new();
                let final_msg = ctx
                    .agent
                    .graph_backfill(limit, &mut |msg| progress_messages.push(msg))
                    .await?;
                for msg in &progress_messages {
                    ctx.sink.send(msg).await?;
                }
                final_msg
            } else if let Some(name) = args.strip_prefix("history ") {
                ctx.agent.graph_history(name.trim()).await?
            } else {
                "Unknown /graph subcommand. Available: /graph, /graph entities, \
                 /graph facts <name>, /graph history <name>, /graph communities, \
                 /graph backfill [--limit N]"
                    .to_owned()
            };
            Ok(CommandOutput::Message(result))
        })
    }
}

/// Show current compression guidelines.
pub struct GuidelinesCommand;

impl CommandHandler<CommandContext<'_>> for GuidelinesCommand {
    fn name(&self) -> &'static str {
        "/guidelines"
    }

    fn description(&self) -> &'static str {
        "Show current compression guidelines"
    }

    fn category(&self) -> SlashCategory {
        SlashCategory::Memory
    }

    fn feature_gate(&self) -> Option<&'static str> {
        Some("compression-guidelines")
    }

    fn handle<'a>(
        &'a self,
        ctx: &'a mut CommandContext<'_>,
        _args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, CommandError>> + Send + 'a>> {
        Box::pin(async move {
            let result = ctx.agent.guidelines().await?;
            Ok(CommandOutput::Message(result))
        })
    }
}

fn parse_backfill_limit(args: &str) -> Option<usize> {
    let pos = args.find("--limit")?;
    args[pos + "--limit".len()..]
        .split_whitespace()
        .next()
        .and_then(|s| s.parse::<usize>().ok())
}

#[cfg(test)]
mod tests {
    use super::parse_backfill_limit;

    #[test]
    fn backfill_limit_parsing() {
        assert_eq!(parse_backfill_limit("backfill --limit 100"), Some(100));
        assert_eq!(parse_backfill_limit("backfill"), None);
        assert_eq!(parse_backfill_limit("backfill --limit"), None);
        assert_eq!(parse_backfill_limit("backfill --limit 0"), Some(0));
    }
}
