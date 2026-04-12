// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! [`DebugAccess`] trait for command handlers that read or mutate debug/diagnostics state.
//!
//! This trait exposes the subset of debug state used by `/log`, `/debug-dump`, and
//! `/dump-format` handlers. All methods return or accept primitives and `String`s so that
//! `zeph-commands` does not depend on `zeph-core`'s `DebugState`, `DebugDumper`, or
//! `DumpFormat` types.

use std::future::Future;
use std::pin::Pin;

use crate::CommandError;

/// Access to debug/diagnostics state for command handlers.
///
/// Implemented by `zeph-core` on a struct that wraps `DebugState`. Methods intentionally return
/// strings rather than internal types, keeping `zeph-commands` free of `zeph-core` dependencies.
pub trait DebugAccess: Send {
    /// Return a formatted human-readable status of the log configuration.
    ///
    /// Used by `/log` to display the current log file path, level, rotation, and max files.
    fn log_status(&self) -> String;

    /// Optionally read and return recent log file tail entries (last `n` lines).
    ///
    /// Returns `None` when log file output is disabled. The implementation performs blocking
    /// I/O via `spawn_blocking`.
    fn read_log_tail<'a>(
        &'a self,
        n: usize,
    ) -> Pin<Box<dyn Future<Output = Option<String>> + Send + 'a>>;

    /// Scrub credentials from a string before displaying it.
    ///
    /// Delegates to `zeph-core`'s `redact::scrub_content`.
    fn scrub(&self, text: &str) -> String;

    /// Return a description of the current debug dump state.
    ///
    /// Returns `Some(path_display)` if debug dump is active, `None` otherwise.
    fn dump_status(&self) -> Option<String>;

    /// Return the current dump format name (e.g. `"json"`, `"raw"`, `"trace"`).
    fn dump_format_name(&self) -> String;

    /// Enable debug dump output to the given directory path.
    ///
    /// Returns `Ok(path_display)` on success.
    ///
    /// # Errors
    ///
    /// Returns `Err(CommandError)` when the directory cannot be created or is inaccessible.
    fn enable_dump(&mut self, dir: &str) -> Result<String, CommandError>;

    /// Switch the dump format to one of `"json"`, `"raw"`, or `"trace"`.
    ///
    /// # Errors
    ///
    /// Returns `Err(CommandError)` when the format name is unrecognized.
    fn set_dump_format(&mut self, format_name: &str) -> Result<(), CommandError>;
}
