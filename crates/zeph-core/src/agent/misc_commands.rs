// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! [`zeph_commands::MiscAccess`] implementation for [`Agent<C>`]: `/loop`, `/notify-test`,
//! and `/search` — a small residual set of commands that do not share a subsystem with any
//! other sub-trait.
//!
//! [`Agent<C>`]: super::Agent

use std::future::Future;
use std::pin::Pin;

use zeph_commands::{CommandError, MiscAccess};

use super::Agent;
use crate::channel::Channel;

/// Parse `<query> [--limit N]` for `/search`. Returns `(trimmed_query, limit)`.
fn parse_search_args(args: &str) -> (&str, Option<usize>) {
    if let Some(pos) = args.find("--limit") {
        let query = args[..pos].trim();
        let rest = args[pos + "--limit".len()..].trim();
        let limit = rest
            .split_whitespace()
            .next()
            .and_then(|s| s.parse::<usize>().ok());
        (query, limit)
    } else {
        (args.trim(), None)
    }
}

impl<C: Channel + Send + 'static> MiscAccess for Agent<C> {
    // ----- /loop -----

    fn handle_loop<'a>(
        &'a mut self,
        args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>> {
        use zeph_commands::handlers::loop_cmd::parse_loop_args;

        let args_owned = args.trim().to_owned();
        Box::pin(async move {
            if args_owned == "stop" {
                return Ok(self.stop_user_loop());
            }
            if args_owned == "status" {
                return Ok(match &self.runtime.lifecycle.user_loop {
                    Some(ls) => format!(
                        "Loop active: \"{}\" (iteration {}, interval every {}s).",
                        ls.prompt,
                        ls.iteration,
                        ls.interval.period().as_secs(),
                    ),
                    None => "No active loop.".to_owned(),
                });
            }
            let (prompt, interval_secs) = parse_loop_args(&args_owned)?;

            if prompt.starts_with('/') {
                return Err(CommandError::new(
                    "Loop prompt must not start with '/'. Slash commands cannot be used as loop prompts.",
                ));
            }

            let min_secs = self.runtime.config.loop_min_interval_secs;
            if interval_secs < min_secs {
                return Err(CommandError::new(format!(
                    "Minimum loop interval is {min_secs}s. Got {interval_secs}s."
                )));
            }
            if self.runtime.lifecycle.user_loop.is_some() {
                return Err(CommandError::new(
                    "A loop is already active. Use /loop stop first.",
                ));
            }

            self.start_user_loop(prompt.clone(), interval_secs);
            Ok(format!(
                "Loop started: \"{prompt}\" every {interval_secs}s. Use /loop stop to cancel."
            ))
        })
    }

    // ----- /notify-test -----

    fn notify_test<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>> {
        let notifier = self.runtime.lifecycle.notifier.clone();
        Box::pin(async move {
            let Some(notifier) = notifier else {
                return Ok(
                    "Notifications are disabled. Set `notifications.enabled = true` in config."
                        .to_owned(),
                );
            };
            match notifier.fire_test().await {
                Ok(()) => Ok("Test notification sent.".to_owned()),
                Err(e) => Err(CommandError::new(format!("notification test failed: {e}"))),
            }
        })
    }

    // ----- /search -----

    fn handle_web_search<'a>(
        &'a mut self,
        args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>> {
        let (query, limit) = parse_search_args(args);
        if query.is_empty() {
            return Box::pin(async move { Ok("Usage: /search <query> [--limit N]".to_owned()) });
        }
        let executor = std::sync::Arc::clone(&self.tool_executor);
        let query = query.to_owned();
        Box::pin(async move {
            let mut params = serde_json::Map::new();
            params.insert("query".to_owned(), serde_json::Value::String(query));
            if let Some(limit) = limit {
                params.insert("limit".to_owned(), serde_json::Value::Number(limit.into()));
            }
            let call = zeph_tools::ToolCall {
                tool_id: "web_search".into(),
                params,
                caller_id: None,
                context: None,
                tool_call_id: String::new(),
                skill_name: None,
            };
            match executor.execute_tool_call_erased(&call).await {
                Ok(Some(output)) => Ok(output.summary),
                Ok(None) => Ok(
                    "web_search is not available. Enable it under `[tools.search]` and store \
                     an API key in the vault."
                        .to_owned(),
                ),
                Err(e) => Err(CommandError::new(format!("web_search failed: {e}"))),
            }
        })
    }
}
