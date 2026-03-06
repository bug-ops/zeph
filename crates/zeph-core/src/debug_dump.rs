// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Debug dump writer for a single agent session.
//!
//! When active, every LLM request/response pair and raw tool output is written to
//! numbered files in a timestamped subdirectory of the configured output directory.
//! Intended for context debugging only — do not use in production.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use zeph_llm::provider::Message;

pub struct DebugDumper {
    dir: PathBuf,
    counter: AtomicU32,
}

impl DebugDumper {
    /// Create a new dumper, creating a timestamped subdirectory under `base_dir`.
    ///
    /// # Errors
    ///
    /// Returns an error if the directory cannot be created.
    pub fn new(base_dir: &Path) -> std::io::Result<Self> {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let dir = base_dir.join(ts.to_string());
        std::fs::create_dir_all(&dir)?;
        tracing::info!(path = %dir.display(), "debug dump directory created");
        Ok(Self {
            dir,
            counter: AtomicU32::new(0),
        })
    }

    /// Return the session dump directory.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn next_id(&self) -> u32 {
        self.counter.fetch_add(1, Ordering::Relaxed)
    }

    fn write(&self, filename: &str, content: &[u8]) {
        let path = self.dir.join(filename);
        if let Err(e) = std::fs::write(&path, content) {
            tracing::warn!(path = %path.display(), error = %e, "debug dump write failed");
        }
    }

    /// Dump the messages about to be sent to the LLM.
    ///
    /// Returns an ID that must be passed to [`dump_response`] to correlate request and response.
    pub fn dump_request(&self, messages: &[Message]) -> u32 {
        let id = self.next_id();
        let json = serde_json::to_string_pretty(messages)
            .unwrap_or_else(|e| format!("serialization error: {e}"));
        self.write(&format!("{id:04}-request.json"), json.as_bytes());
        id
    }

    /// Dump the LLM response corresponding to a prior [`dump_request`] call.
    pub fn dump_response(&self, id: u32, response: &str) {
        self.write(&format!("{id:04}-response.txt"), response.as_bytes());
    }

    /// Dump raw tool output before any truncation or summarization.
    pub fn dump_tool_output(&self, tool_name: &str, output: &str) {
        let id = self.next_id();
        let safe_name: String = tool_name
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        self.write(&format!("{id:04}-tool-{safe_name}.txt"), output.as_bytes());
    }
}
