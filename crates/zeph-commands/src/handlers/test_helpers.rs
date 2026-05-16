// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Shared test mock types for handler unit tests.
//!
//! Import with `use super::test_helpers::*;` inside `#[cfg(test)]` blocks.

use std::future::Future;
use std::pin::Pin;

use crate::CommandError;
use crate::context::CommandContext;
use crate::sink::NullSink;
use crate::traits::debug::DebugAccess;
use crate::traits::messages::MessageAccess;
use crate::traits::session::SessionAccess;

pub struct MockDebug;

impl DebugAccess for MockDebug {
    fn log_status(&self) -> String {
        String::new()
    }

    fn read_log_tail<'a>(
        &'a self,
        _n: usize,
    ) -> Pin<Box<dyn Future<Output = Option<String>> + Send + 'a>> {
        Box::pin(async { None })
    }

    fn scrub(&self, text: &str) -> String {
        text.to_owned()
    }

    fn dump_status(&self) -> Option<String> {
        None
    }

    fn dump_format_name(&self) -> String {
        String::new()
    }

    fn enable_dump(&mut self, _dir: &str) -> Result<String, CommandError> {
        Ok(String::new())
    }

    fn set_dump_format(&mut self, _name: &str) -> Result<(), CommandError> {
        Ok(())
    }
}

pub struct MockMessages;

impl MessageAccess for MockMessages {
    fn clear_history(&mut self) {}

    fn queue_len(&self) -> usize {
        0
    }

    fn drain_queue(&mut self) -> usize {
        0
    }

    fn notify_queue_count<'a>(
        &'a mut self,
        _count: usize,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async {})
    }
}

pub struct MockSession;

impl SessionAccess for MockSession {
    fn supports_exit(&self) -> bool {
        false
    }
}

pub fn make_ctx<'a>(
    sink: &'a mut NullSink,
    debug: &'a mut MockDebug,
    messages: &'a mut MockMessages,
    session: &'a MockSession,
    agent: &'a mut crate::NullAgent,
) -> CommandContext<'a> {
    CommandContext {
        sink,
        debug,
        messages,
        session: session as &dyn SessionAccess,
        agent,
    }
}
