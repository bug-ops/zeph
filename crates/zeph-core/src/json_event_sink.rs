// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `JsonEventSink`: the single stdout writer for `--json` mode.
//!
//! All JSON events in a `--json` session are emitted through a shared
//! `Arc<JsonEventSink>`. The internal `Mutex<Stdout>` ensures that concurrent
//! emitters from different tasks cannot interleave partial lines.
//!
//! # Ordering guarantee
//!
//! Events emitted from the same thread preserve their issuance order. Events
//! from concurrent threads interleave in mutex-acquisition order. Within a
//! single response, `response_chunk` events precede `response_end`. Tool events
//! may interleave with chunks when tools run mid-stream (normal for agent loops).
//!
//! # Lock discipline
//!
//! `emit` holds the lock only for serialization + write + flush. It never
//! `.await`s while holding the lock, satisfying invariant §10.

use std::io::{self, Write};
use std::sync::Mutex;

use serde::Serialize;

/// Structured event emitted on stdout in `--json` mode.
///
/// All variants are serialized as JSONL with a `"event"` discriminator field.
#[derive(Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum JsonEvent<'a> {
    /// Session boot banner emitted before the first prompt.
    Boot {
        version: &'a str,
        bare: bool,
        auto: bool,
    },
    /// User input received from stdin.
    Query { text: &'a str, queue_len: usize },
    /// Streaming assistant text chunk.
    ResponseChunk { text: &'a str },
    /// End-of-response marker.
    ResponseEnd,
    /// A tool invocation is about to run.
    ToolCall {
        tool: &'a str,
        args: &'a serde_json::Value,
        id: &'a str,
    },
    /// A tool returned a result.
    ToolResult {
        tool: &'a str,
        id: &'a str,
        output: &'a str,
        is_error: bool,
    },
    /// Token counts and estimated cost summary.
    Cost {
        input_tokens: u64,
        output_tokens: u64,
        total_usd: f64,
    },
    /// Loop tick notification fired each `/loop` iteration.
    LoopTick {
        iteration: u64,
        total_ticks: u64,
        prompt_preview: &'a str,
    },
    /// Slash command acknowledgement — distinguishes `/loop start` confirmation
    /// from regular assistant output in JSON streams.
    CommandAck { command: &'a str, text: &'a str },
    /// General status message (equivalent to spinner text in interactive channels).
    Status { message: &'a str },
    /// Terminal error emitted before the process exits.
    Error { message: &'a str },
}

/// The single stdout writer for `--json` mode.
///
/// Wrap in `Arc` and share between `JsonCliChannel` and `JsonEventLayer`.
/// `emit` is synchronous and lock-bounded: it never yields across `.await`.
pub struct JsonEventSink {
    writer: Mutex<Box<dyn Write + Send>>,
}

impl std::fmt::Debug for JsonEventSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JsonEventSink").finish_non_exhaustive()
    }
}

impl JsonEventSink {
    /// Create a new sink that writes to the process's stdout.
    #[must_use]
    pub fn new() -> Self {
        Self {
            writer: Mutex::new(Box::new(io::stdout())),
        }
    }

    /// Create a sink backed by an arbitrary [`Write`] implementation.
    ///
    /// Intended for testing: pass a type that implements [`Write`] + [`Send`] + `'static`,
    /// such as [`std::io::Cursor`]`<Vec<u8>>`, to capture emitted JSONL lines in memory.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::io::Cursor;
    /// use std::sync::Arc;
    /// use zeph_core::json_event_sink::{JsonEvent, JsonEventSink};
    ///
    /// let sink = Arc::new(JsonEventSink::with_writer(Cursor::new(Vec::<u8>::new())));
    /// sink.emit(&JsonEvent::Status { message: "hello" });
    /// ```
    #[must_use]
    pub fn with_writer(w: impl Write + Send + 'static) -> Self {
        Self {
            writer: Mutex::new(Box::new(w)),
        }
    }

    /// Serialize `event` as a JSON line and write it to the underlying writer.
    ///
    /// Silently drops the event when the mutex is poisoned or serialization fails.
    /// This is intentional: a JSON output failure must not crash the agent.
    pub fn emit(&self, event: &JsonEvent<'_>) {
        let Ok(mut w) = self.writer.lock() else {
            return;
        };
        if let Ok(line) = serde_json::to_string(event) {
            let _ = writeln!(w, "{line}");
            let _ = w.flush();
        }
    }
}

impl Default for JsonEventSink {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boot_event_serializes_correctly() {
        let event = JsonEvent::Boot {
            version: "0.1.0",
            bare: true,
            auto: false,
        };
        let s = serde_json::to_string(&event).unwrap();
        assert!(s.contains("\"event\":\"boot\""));
        assert!(s.contains("\"version\":\"0.1.0\""));
        assert!(s.contains("\"bare\":true"));
    }

    #[test]
    fn response_end_serializes_without_fields() {
        let event = JsonEvent::ResponseEnd;
        let s = serde_json::to_string(&event).unwrap();
        assert_eq!(s, r#"{"event":"response_end"}"#);
    }

    #[test]
    fn emit_does_not_panic_on_concurrent_use() {
        use std::sync::Arc;
        use std::thread;

        let sink = Arc::new(JsonEventSink::new());
        let handles: Vec<_> = (0..4)
            .map(|i| {
                let s = Arc::clone(&sink);
                thread::spawn(move || {
                    for _ in 0..10 {
                        s.emit(&JsonEvent::Status {
                            message: &format!("thread {i}"),
                        });
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
    }
}
