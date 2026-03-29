// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! OpenTelemetry-compatible trace collector for debug sessions.
//!
//! Collects span data during an agent session and serializes to OTLP JSON format
//! at session end. All text-bearing attributes are redacted via `crate::redact::scrub_content`
//! before storage (C-01).
//!
//! Design notes:
//! - Uses explicit `begin_X` / `end_X` methods with owned `SpanGuard` — safe
//!   across async `.await` boundaries because no borrow to `TracingCollector` is held (C-02).
//! - A `HashMap<usize, IterationEntry>` tracks concurrent iterations (I-03).
//! - `Drop` on `TracingCollector` flushes partial traces on error/panic paths (C-04).
//! - When the `otel` feature is enabled an `mpsc` channel forwards completed spans to
//!   the OTLP exporter in `tracing_init.rs` (C-05).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use rand::RngExt as _;
use serde::{Deserialize, Serialize};

use crate::redact::scrub_content;

// ─── Span ID generation ───────────────────────────────────────────────────────

static SPAN_COUNTER: AtomicU64 = AtomicU64::new(0);

#[must_use]
fn new_trace_id() -> [u8; 16] {
    rand::rng().random()
}

#[must_use]
fn new_span_id() -> [u8; 8] {
    let mut id: [u8; 8] = rand::rng().random();
    // XOR low byte with counter to guarantee distinct IDs even under high concurrency.
    id[0] ^= (SPAN_COUNTER.fetch_add(1, Ordering::Relaxed) & 0xFF) as u8;
    id
}

#[must_use]
fn hex16(b: &[u8; 16]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(32);
    for x in b {
        let _ = write!(s, "{x:02x}");
    }
    s
}

#[must_use]
fn hex8(b: [u8; 8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(16);
    for x in b {
        let _ = write!(s, "{x:02x}");
    }
    s
}

#[must_use]
fn now_unix_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
}

// ─── Public types ─────────────────────────────────────────────────────────────

/// Span status code (matches OTLP `StatusCode` values).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SpanStatus {
    Ok,
    Error { message: String },
    Unset,
}

/// A completed span ready for OTLP serialization.
#[derive(Debug, Clone)]
pub struct SpanData {
    pub trace_id: [u8; 16],
    pub span_id: [u8; 8],
    pub parent_span_id: Option<[u8; 8]>,
    pub name: String,
    pub start_time_unix_nanos: u64,
    pub end_time_unix_nanos: u64,
    pub attributes: Vec<(String, String)>,
    pub status: SpanStatus,
}

/// Owned guard returned by `begin_*` methods. Pass back to `end_*` to close the span.
///
/// Does NOT hold a reference to `TracingCollector` — safe across async `.await` boundaries (C-02).
pub struct SpanGuard {
    pub span_id: [u8; 8],
    pub parent_span_id: [u8; 8],
    pub name: String,
    pub start_time_unix_nanos: u64,
}

/// Attributes for a completed LLM request span.
pub struct LlmAttributes {
    pub model: String,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub latency_ms: u64,
    pub streaming: bool,
    pub cache_hit: bool,
}

/// Attributes for a completed tool call span.
pub struct ToolAttributes {
    pub latency_ms: u64,
    pub is_error: bool,
    pub error_kind: Option<String>,
}

/// Attributes for a completed memory search span.
pub struct MemorySearchAttributes {
    pub query_preview: String,
    pub result_count: usize,
    pub latency_ms: u64,
}

// ─── Bridge event for OTLP export (C-05) ─────────────────────────────────────

/// Event sent over the mpsc channel to the OTLP exporter.
///
/// The root crate wires the sender when the `otel` feature is enabled.
/// `zeph-core` compiles this unconditionally so the struct is always available.
#[derive(Debug)]
pub struct TraceEvent {
    pub trace_id: [u8; 16],
    pub spans: Vec<SpanData>,
}

// ─── Internal ─────────────────────────────────────────────────────────────────

/// Internal entry for a still-open iteration.
struct IterationEntry {
    guard: SpanGuard,
    user_msg_preview: String,
}

// ─── TracingCollector ─────────────────────────────────────────────────────────

/// Collects OTel-compatible spans for a single agent session.
///
/// All methods take `&mut self`. The agent loop is single-threaded within a session.
/// For concurrent iteration support (I-03), a `HashMap<usize, IterationEntry>` is used.
/// Default cap on collected spans per session (SEC-02).
const DEFAULT_MAX_SPANS: usize = 10_000;

pub struct TracingCollector {
    trace_id: [u8; 16],
    session_span_id: [u8; 8],
    session_start: u64,
    service_name: String,
    output_dir: PathBuf,
    /// Active (open) iterations keyed by iteration index (I-03).
    active_iterations: HashMap<usize, IterationEntry>,
    completed_spans: Vec<SpanData>,
    /// Hard cap on `completed_spans` length. Oldest span dropped when exceeded (SEC-02).
    max_spans: usize,
    /// Whether to redact text attributes. Defaults to `true` (C-01).
    redact: bool,
    /// Guards against double-write on explicit `finish()` followed by `Drop`.
    flushed: bool,
    /// Optional channel to forward completed spans to the OTLP exporter.
    /// Wired by the root crate when the `otel` feature is enabled (C-05).
    trace_tx: Option<tokio::sync::mpsc::UnboundedSender<TraceEvent>>,
}

impl TracingCollector {
    /// Create a new collector.
    ///
    /// # Errors
    ///
    /// Returns an error if `output_dir` cannot be created.
    pub fn new(
        output_dir: &Path,
        service_name: impl Into<String>,
        redact: bool,
        trace_tx: Option<tokio::sync::mpsc::UnboundedSender<TraceEvent>>,
    ) -> std::io::Result<Self> {
        std::fs::create_dir_all(output_dir)?;
        Ok(Self {
            trace_id: new_trace_id(),
            session_span_id: new_span_id(),
            session_start: now_unix_nanos(),
            service_name: service_name.into(),
            output_dir: output_dir.to_owned(),
            active_iterations: HashMap::new(),
            completed_spans: Vec::new(),
            max_spans: DEFAULT_MAX_SPANS,
            redact,
            flushed: false,
            trace_tx,
        })
    }

    fn maybe_redact<'a>(&self, text: &'a str) -> std::borrow::Cow<'a, str> {
        if self.redact {
            scrub_content(text)
        } else {
            std::borrow::Cow::Borrowed(text)
        }
    }

    /// Append a span, dropping the oldest when `max_spans` is exceeded (SEC-02).
    fn push_span(&mut self, span: SpanData) {
        if self.completed_spans.len() >= self.max_spans {
            tracing::warn!(
                max_spans = self.max_spans,
                "trace span cap reached, dropping oldest span"
            );
            self.completed_spans.remove(0); // lgtm[rust/cleartext-logging]
        }
        self.completed_spans.push(span);
    }

    // ── Iteration spans ───────────────────────────────────────────────────────

    /// Open an iteration span. Call at the start of `process_user_message`.
    pub fn begin_iteration(&mut self, index: usize, user_msg_preview: &str) {
        let preview = self
            .maybe_redact(user_msg_preview)
            .chars()
            .take(100)
            .collect::<String>();
        let entry = IterationEntry {
            guard: SpanGuard {
                span_id: new_span_id(),
                parent_span_id: self.session_span_id,
                name: format!("iteration.{index}"),
                start_time_unix_nanos: now_unix_nanos(),
            },
            user_msg_preview: preview,
        };
        self.active_iterations.insert(index, entry);
    }

    /// Close an iteration span.
    pub fn end_iteration(&mut self, index: usize, status: SpanStatus) {
        let end_time = now_unix_nanos();
        if let Some(entry) = self.active_iterations.remove(&index) {
            let span = SpanData {
                trace_id: self.trace_id,
                span_id: entry.guard.span_id,
                parent_span_id: Some(entry.guard.parent_span_id),
                name: entry.guard.name,
                start_time_unix_nanos: entry.guard.start_time_unix_nanos,
                end_time_unix_nanos: end_time,
                attributes: vec![(
                    "zeph.iteration.user_message_preview".to_owned(),
                    entry.user_msg_preview,
                )],
                status,
            };
            self.push_span(span);
        } else {
            tracing::warn!(index, "end_iteration without matching begin_iteration");
        }
    }

    // ── LLM request spans ─────────────────────────────────────────────────────

    /// Open an LLM request span. Returns an owned `SpanGuard` safe to hold across `.await`.
    #[must_use]
    pub fn begin_llm_request(&self, iteration_span_id: [u8; 8]) -> SpanGuard {
        SpanGuard {
            span_id: new_span_id(),
            parent_span_id: iteration_span_id,
            name: "llm.request".to_owned(),
            start_time_unix_nanos: now_unix_nanos(),
        }
    }

    /// Close an LLM request span.
    pub fn end_llm_request(&mut self, guard: SpanGuard, attrs: &LlmAttributes) {
        let end_time = now_unix_nanos();
        let model_clean = self.maybe_redact(&attrs.model).into_owned();
        self.push_span(SpanData {
            trace_id: self.trace_id,
            span_id: guard.span_id,
            parent_span_id: Some(guard.parent_span_id),
            name: guard.name,
            start_time_unix_nanos: guard.start_time_unix_nanos,
            end_time_unix_nanos: end_time,
            attributes: vec![
                ("zeph.llm.model".to_owned(), model_clean),
                (
                    "zeph.llm.prompt_tokens".to_owned(),
                    attrs.prompt_tokens.to_string(),
                ),
                (
                    "zeph.llm.completion_tokens".to_owned(),
                    attrs.completion_tokens.to_string(),
                ),
                (
                    "zeph.llm.latency_ms".to_owned(),
                    attrs.latency_ms.to_string(),
                ),
                ("zeph.llm.streaming".to_owned(), attrs.streaming.to_string()),
                ("zeph.llm.cache_hit".to_owned(), attrs.cache_hit.to_string()),
            ],
            status: SpanStatus::Ok,
        });
    }

    // ── Tool call spans ───────────────────────────────────────────────────────

    /// Open a tool call span, recording the start time as now.
    #[must_use]
    pub fn begin_tool_call(&self, tool_name: &str, iteration_span_id: [u8; 8]) -> SpanGuard {
        self.begin_tool_call_at(tool_name, iteration_span_id, &std::time::Instant::now())
    }

    /// Open a tool call span with a pre-recorded start time.
    ///
    /// Use this variant when the tool has already executed (post-hoc assembly pattern) and
    /// `started_at` was captured *before* the call. The Unix start timestamp is back-computed
    /// from `started_at.elapsed()` so the span is correctly positioned on the timeline.
    #[must_use]
    pub fn begin_tool_call_at(
        &self,
        tool_name: &str,
        iteration_span_id: [u8; 8],
        started_at: &std::time::Instant,
    ) -> SpanGuard {
        let elapsed_nanos = u64::try_from(started_at.elapsed().as_nanos()).unwrap_or(u64::MAX);
        let start_time_unix_nanos = now_unix_nanos().saturating_sub(elapsed_nanos);
        SpanGuard {
            span_id: new_span_id(),
            parent_span_id: iteration_span_id,
            name: format!("tool.{}", sanitize_name(tool_name)),
            start_time_unix_nanos,
        }
    }

    /// Close a tool call span.
    pub fn end_tool_call(&mut self, guard: SpanGuard, tool_name: &str, attrs: ToolAttributes) {
        let end_time = now_unix_nanos();
        let tool_clean = sanitize_name(tool_name);
        let mut attributes = vec![
            ("zeph.tool.name".to_owned(), tool_clean),
            (
                "zeph.tool.latency_ms".to_owned(),
                attrs.latency_ms.to_string(),
            ),
            ("zeph.tool.is_error".to_owned(), attrs.is_error.to_string()),
        ];
        if let Some(kind) = attrs.error_kind {
            // IMP-04: apply redaction to error messages (may contain secret data).
            let kind_clean = self.maybe_redact(&kind).into_owned();
            attributes.push(("zeph.tool.error_kind".to_owned(), kind_clean));
        }
        let status = if attrs.is_error {
            SpanStatus::Error {
                message: "tool call failed".to_owned(),
            }
        } else {
            SpanStatus::Ok
        };
        self.push_span(SpanData {
            trace_id: self.trace_id,
            span_id: guard.span_id,
            parent_span_id: Some(guard.parent_span_id),
            name: guard.name,
            start_time_unix_nanos: guard.start_time_unix_nanos,
            end_time_unix_nanos: end_time,
            attributes,
            status,
        });
    }

    // ── Memory search spans ───────────────────────────────────────────────────

    /// Open a memory search span.
    #[must_use]
    pub fn begin_memory_search(&self, parent_span_id: [u8; 8]) -> SpanGuard {
        SpanGuard {
            span_id: new_span_id(),
            parent_span_id,
            name: "memory.search".to_owned(),
            start_time_unix_nanos: now_unix_nanos(),
        }
    }

    /// Close a memory search span.
    pub fn end_memory_search(&mut self, guard: SpanGuard, attrs: &MemorySearchAttributes) {
        let end_time = now_unix_nanos();
        let query_clean = self
            .maybe_redact(&attrs.query_preview)
            .chars()
            .take(100)
            .collect::<String>();
        self.push_span(SpanData {
            trace_id: self.trace_id,
            span_id: guard.span_id,
            parent_span_id: Some(guard.parent_span_id),
            name: guard.name,
            start_time_unix_nanos: guard.start_time_unix_nanos,
            end_time_unix_nanos: end_time,
            attributes: vec![
                ("zeph.memory.query_preview".to_owned(), query_clean),
                (
                    "zeph.memory.result_count".to_owned(),
                    attrs.result_count.to_string(),
                ),
                (
                    "zeph.memory.latency_ms".to_owned(),
                    attrs.latency_ms.to_string(),
                ),
            ],
            status: SpanStatus::Ok,
        });
    }

    // ── Accessors ─────────────────────────────────────────────────────────────

    /// Return the path to the `trace.json` file that will be written on `finish()`.
    #[must_use]
    pub fn trace_json_path(&self) -> PathBuf {
        self.output_dir.join("trace.json")
    }

    /// Return the span ID of the currently active iteration, if any.
    #[must_use]
    pub fn current_iteration_span_id(&self, index: usize) -> Option<[u8; 8]> {
        self.active_iterations.get(&index).map(|e| e.guard.span_id)
    }

    /// Return the session root span ID (fallback parent when no iteration is active).
    #[must_use]
    pub fn session_span_id(&self) -> [u8; 8] {
        self.session_span_id
    }

    /// Return the trace ID for this session.
    #[must_use]
    pub fn trace_id(&self) -> [u8; 16] {
        self.trace_id
    }

    // ── Flush ─────────────────────────────────────────────────────────────────

    /// Finalize the session span and write `trace.json`.
    ///
    /// Safe to call multiple times — subsequent calls after the first are no-ops.
    /// Also sends spans over the `OTel` channel when the `otel` feature is enabled (C-05).
    pub fn finish(&mut self) {
        if self.flushed {
            return;
        }
        self.flushed = true;

        // Close any still-open iteration spans with `Unset` status (partial trace on error/cancel).
        let open_keys: Vec<usize> = self.active_iterations.keys().copied().collect();
        let end_time = now_unix_nanos();
        for index in open_keys {
            if let Some(entry) = self.active_iterations.remove(&index) {
                self.push_span(SpanData {
                    trace_id: self.trace_id,
                    span_id: entry.guard.span_id,
                    parent_span_id: Some(entry.guard.parent_span_id),
                    name: entry.guard.name,
                    start_time_unix_nanos: entry.guard.start_time_unix_nanos,
                    end_time_unix_nanos: end_time,
                    attributes: vec![(
                        "zeph.iteration.user_message_preview".to_owned(),
                        entry.user_msg_preview,
                    )],
                    status: SpanStatus::Unset,
                });
            }
        }

        let session_span = SpanData {
            trace_id: self.trace_id,
            span_id: self.session_span_id,
            parent_span_id: None,
            name: "session".to_owned(),
            start_time_unix_nanos: self.session_start,
            end_time_unix_nanos: end_time,
            attributes: vec![
                ("service.name".to_owned(), self.service_name.clone()),
                ("zeph.session.trace_id".to_owned(), hex16(&self.trace_id)),
            ],
            status: SpanStatus::Ok,
        };

        let mut all_spans = vec![session_span];
        all_spans.append(&mut self.completed_spans);

        let json = serialize_otlp_json(&all_spans, &self.service_name);
        let path = self.output_dir.join("trace.json");
        if let Err(e) = write_trace_file(&path, json.as_bytes()) {
            tracing::warn!(path = %path.display(), error = %e, "trace.json write failed");
        } else {
            tracing::info!(path = %path.display(), "OTel trace written");
        }

        // C-05: forward spans to OTLP exporter when the root crate has wired the channel.
        if let Some(ref tx) = self.trace_tx {
            let event = TraceEvent {
                trace_id: self.trace_id,
                spans: all_spans,
            };
            if tx.send(event).is_err() {
                tracing::debug!("OTLP trace channel closed, skipping export");
            }
        }
    }
}

// C-04: Drop flushes partial traces on error/panic/cancellation.
impl Drop for TracingCollector {
    fn drop(&mut self) {
        self.finish();
    }
}

// ─── OTLP JSON serialization (I-04) ───────────────────────────────────────────

fn span_status_code(status: &SpanStatus) -> u8 {
    match status {
        SpanStatus::Unset => 0,
        SpanStatus::Ok => 1,
        SpanStatus::Error { .. } => 2,
    }
}

/// Serialize spans to OTLP JSON Protobuf encoding.
///
/// Format: <https://opentelemetry.io/docs/specs/otlp/#json-protobuf-encoding>
#[must_use]
pub fn serialize_otlp_json(spans: &[SpanData], service_name: &str) -> String {
    let otlp_spans: Vec<serde_json::Value> = spans
        .iter()
        .map(|s| {
            let attrs: Vec<serde_json::Value> = s
                .attributes
                .iter()
                .map(|(k, v)| {
                    serde_json::json!({
                        "key": k,
                        "value": { "stringValue": v }
                    })
                })
                .collect();

            let mut obj = serde_json::json!({
                "traceId": hex16(&s.trace_id),
                "spanId": hex8(s.span_id),
                "name": s.name,
                // OTLP JSON spec requires int64 fields as strings.
                "startTimeUnixNano": s.start_time_unix_nanos.to_string(),
                "endTimeUnixNano": s.end_time_unix_nanos.to_string(),
                "attributes": attrs,
                "status": {
                    "code": span_status_code(&s.status)
                }
            });

            if let Some(parent) = s.parent_span_id {
                obj["parentSpanId"] = serde_json::json!(hex8(parent));
            }

            if let SpanStatus::Error { message } = &s.status {
                obj["status"]["message"] = serde_json::json!(message);
            }

            obj
        })
        .collect();

    let payload = serde_json::json!({
        "resourceSpans": [{
            "resource": {
                "attributes": [{
                    "key": "service.name",
                    "value": { "stringValue": service_name }
                }]
            },
            "scopeSpans": [{
                "scope": {
                    "name": "zeph",
                    "version": env!("CARGO_PKG_VERSION")
                },
                "spans": otlp_spans
            }]
        }]
    });

    serde_json::to_string_pretty(&payload)
        .unwrap_or_else(|e| format!("{{\"error\": \"serialization failed: {e}\"}}"))
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

/// Write `data` to `path` with mode 0o600 on Unix (SEC-01).
/// Falls back to `std::fs::write` on non-Unix platforms.
fn write_trace_file(path: &Path, data: &[u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        f.write_all(data)
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, data)
    }
}

fn sanitize_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn make_collector(dir: &Path) -> TracingCollector {
        TracingCollector::new(dir, "zeph-test", false, None).unwrap()
    }

    #[test]
    fn span_id_generation_is_unique() {
        let ids: Vec<[u8; 8]> = (0..100).map(|_| new_span_id()).collect();
        let unique: std::collections::HashSet<[u8; 8]> = ids.into_iter().collect();
        assert_eq!(unique.len(), 100);
    }

    #[test]
    fn hex_lengths_correct() {
        assert_eq!(hex16(&new_trace_id()).len(), 32);
        assert_eq!(hex8(new_span_id()).len(), 16);
    }

    #[test]
    fn collector_creates_output_dir() {
        let tmp = tempdir().unwrap();
        let sub = tmp.path().join("traces");
        make_collector(&sub);
        assert!(sub.exists());
    }

    #[test]
    fn finish_writes_trace_json() {
        let tmp = tempdir().unwrap();
        let mut c = make_collector(tmp.path());
        c.begin_iteration(0, "hello world");
        c.end_iteration(0, SpanStatus::Ok);
        c.finish();

        let path = tmp.path().join("trace.json");
        assert!(path.exists(), "trace.json must be written");

        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(v["resourceSpans"].is_array());
        // session + iteration = 2 spans.
        let spans = v["resourceSpans"][0]["scopeSpans"][0]["spans"]
            .as_array()
            .unwrap();
        assert_eq!(spans.len(), 2);
    }

    #[test]
    fn span_hierarchy_parent_child_correct() {
        let tmp = tempdir().unwrap();
        let mut c = make_collector(tmp.path());
        c.begin_iteration(0, "test");
        let iter_id = c.current_iteration_span_id(0).unwrap();
        let guard = c.begin_llm_request(iter_id);
        c.end_llm_request(
            guard,
            &LlmAttributes {
                model: "test-model".to_owned(),
                prompt_tokens: 100,
                completion_tokens: 50,
                latency_ms: 200,
                streaming: false,
                cache_hit: false,
            },
        );
        c.end_iteration(0, SpanStatus::Ok);
        c.finish();

        let content = std::fs::read_to_string(tmp.path().join("trace.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&content).unwrap();
        let spans = v["resourceSpans"][0]["scopeSpans"][0]["spans"]
            .as_array()
            .unwrap();
        assert_eq!(spans.len(), 3, "session + iteration + llm = 3 spans");

        let llm_span = spans
            .iter()
            .find(|s| s["name"] == "llm.request")
            .expect("llm.request span missing");
        let iter_span = spans
            .iter()
            .find(|s| s["name"] == "iteration.0")
            .expect("iteration.0 span missing");

        assert_eq!(
            llm_span["parentSpanId"], iter_span["spanId"],
            "llm span parent must be iteration span"
        );
    }

    #[test]
    fn redaction_applied_to_text_attributes() {
        let tmp = tempdir().unwrap();
        let mut c = TracingCollector::new(tmp.path(), "test", true, None).unwrap();
        let iter_id = c.session_span_id();
        let guard = c.begin_memory_search(iter_id);
        c.end_memory_search(
            guard,
            &MemorySearchAttributes {
                query_preview: "search sk-secretkey123 here".to_owned(),
                result_count: 3,
                latency_ms: 10,
            },
        );
        c.finish();

        let content = std::fs::read_to_string(tmp.path().join("trace.json")).unwrap();
        assert!(
            !content.contains("sk-secretkey123"),
            "raw secret must be redacted from trace"
        );
    }

    #[test]
    fn otlp_json_format_spec_compliant() {
        let trace_id = [0xAB_u8; 16];
        let span_id = [0xCD_u8; 8];
        let spans = vec![SpanData {
            trace_id,
            span_id,
            parent_span_id: None,
            name: "session".to_owned(),
            start_time_unix_nanos: 1_000_000,
            end_time_unix_nanos: 2_000_000,
            attributes: vec![("service.name".to_owned(), "zeph".to_owned())],
            status: SpanStatus::Ok,
        }];

        let json = serialize_otlp_json(&spans, "zeph");
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();

        let span = &v["resourceSpans"][0]["scopeSpans"][0]["spans"][0];
        assert_eq!(
            span["traceId"],
            "abababababababababababababababababab"[..32]
        );
        assert_eq!(span["spanId"], "cdcdcdcdcdcdcdcd");
        assert_eq!(span["name"], "session");
        // int64 must be serialized as string per OTLP JSON spec.
        assert!(span["startTimeUnixNano"].is_string());
        assert_eq!(span["status"]["code"], 1_u64);
    }

    #[test]
    fn drop_flushes_trace() {
        let tmp = tempdir().unwrap();
        {
            let mut c = make_collector(tmp.path());
            c.begin_iteration(0, "hello");
            // Drop without explicit finish.
        }
        assert!(
            tmp.path().join("trace.json").exists(),
            "Drop must flush trace.json"
        );
    }

    #[test]
    fn finish_is_idempotent() {
        let tmp = tempdir().unwrap();
        let mut c = make_collector(tmp.path());
        c.finish();
        c.finish();
        assert!(tmp.path().join("trace.json").exists());
    }

    #[test]
    fn concurrent_iterations_tracked_independently() {
        let tmp = tempdir().unwrap();
        let mut c = make_collector(tmp.path());
        c.begin_iteration(0, "first");
        c.begin_iteration(1, "second");
        let id0 = c.current_iteration_span_id(0).unwrap();
        let id1 = c.current_iteration_span_id(1).unwrap();
        assert_ne!(
            id0, id1,
            "concurrent iterations must have distinct span IDs"
        );
        c.end_iteration(0, SpanStatus::Ok);
        c.end_iteration(1, SpanStatus::Ok);
        c.finish();

        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(tmp.path().join("trace.json")).unwrap())
                .unwrap();
        let spans = v["resourceSpans"][0]["scopeSpans"][0]["spans"]
            .as_array()
            .unwrap();
        // session + 2 iterations = 3.
        assert_eq!(spans.len(), 3);
    }

    #[test]
    fn trace_format_skips_legacy_numbered_files() {
        use crate::debug_dump::{DebugDumper, DumpFormat, RequestDebugDump};

        let tmp = tempdir().unwrap();
        let d = DebugDumper::new(tmp.path(), DumpFormat::Trace).unwrap();
        let session_dir = d.dir().to_owned();
        let id = d.dump_request(&RequestDebugDump {
            model_name: "test",
            messages: &[],
            tools: &[],
            provider_request: serde_json::json!({}),
        });
        d.dump_response(id, "resp");
        d.dump_tool_output("shell", "output");

        // No legacy numbered files should be written in Trace format.
        let files: Vec<_> = std::fs::read_dir(&session_dir)
            .unwrap()
            .filter_map(std::result::Result::ok)
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_digit())
            })
            .collect();
        assert!(
            files.is_empty(),
            "no legacy numbered files in Trace format session dir"
        );

        // trace.json is written into the session subdir by TracingCollector when wired.
        // Here we only verify the session dir itself exists (TracingCollector is not wired in this test).
        assert!(session_dir.is_dir(), "session subdir must exist");
    }

    #[test]
    fn tool_call_span_emitted() {
        let tmp = tempdir().unwrap();
        let mut c = make_collector(tmp.path());
        c.begin_iteration(0, "test");
        let iter_id = c.current_iteration_span_id(0).unwrap();
        let guard = c.begin_tool_call("shell", iter_id);
        c.end_tool_call(
            guard,
            "shell",
            ToolAttributes {
                latency_ms: 50,
                is_error: false,
                error_kind: None,
            },
        );
        c.end_iteration(0, SpanStatus::Ok);
        c.finish();

        let content = std::fs::read_to_string(c.trace_json_path()).unwrap();
        let v: serde_json::Value = serde_json::from_str(&content).unwrap();
        let spans = v["resourceSpans"][0]["scopeSpans"][0]["spans"]
            .as_array()
            .unwrap();
        assert!(
            spans.iter().any(|s| s["name"] == "tool.shell"),
            "tool.shell span must be emitted"
        );
    }

    #[test]
    fn tool_call_error_span_emitted() {
        let tmp = tempdir().unwrap();
        let mut c = make_collector(tmp.path());
        c.begin_iteration(0, "test");
        let iter_id = c.current_iteration_span_id(0).unwrap();
        let guard = c.begin_tool_call("shell", iter_id);
        c.end_tool_call(
            guard,
            "shell",
            ToolAttributes {
                latency_ms: 10,
                is_error: true,
                error_kind: Some("permission denied".to_owned()),
            },
        );
        c.end_iteration(0, SpanStatus::Ok);
        c.finish();

        let content = std::fs::read_to_string(c.trace_json_path()).unwrap();
        let v: serde_json::Value = serde_json::from_str(&content).unwrap();
        let spans = v["resourceSpans"][0]["scopeSpans"][0]["spans"]
            .as_array()
            .unwrap();
        let tool_span = spans
            .iter()
            .find(|s| s["name"] == "tool.shell")
            .expect("tool.shell span missing");
        assert_eq!(
            tool_span["status"]["code"], 2_u64,
            "error span must have status code 2"
        );
    }

    #[test]
    fn begin_tool_call_at_timestamps_precede_end_time() {
        let tmp = tempdir().unwrap();
        let mut c = make_collector(tmp.path());
        c.begin_iteration(0, "test");
        let iter_id = c.current_iteration_span_id(0).unwrap();

        // Simulate post-hoc assembly: capture start before "execution", then call begin_tool_call_at.
        let started_at = std::time::Instant::now();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let guard = c.begin_tool_call_at("shell", iter_id, &started_at);
        let span_start = guard.start_time_unix_nanos;
        c.end_tool_call(
            guard,
            "shell",
            ToolAttributes {
                latency_ms: 2,
                is_error: false,
                error_kind: None,
            },
        );
        c.end_iteration(0, SpanStatus::Ok);
        c.finish();

        let content = std::fs::read_to_string(c.trace_json_path()).unwrap();
        let v: serde_json::Value = serde_json::from_str(&content).unwrap();
        let spans = v["resourceSpans"][0]["scopeSpans"][0]["spans"]
            .as_array()
            .unwrap();
        let tool_span = spans
            .iter()
            .find(|s| s["name"] == "tool.shell")
            .expect("tool.shell span missing");
        let recorded_start: u64 = tool_span["startTimeUnixNano"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        let recorded_end: u64 = tool_span["endTimeUnixNano"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        // The span start must be earlier than the end.
        assert!(
            recorded_start < recorded_end,
            "start ({recorded_start}) must precede end ({recorded_end})"
        );
        // The guard's start_time matches what was serialized.
        assert_eq!(
            span_start, recorded_start,
            "guard start must match serialized start"
        );
    }

    #[test]
    fn session_to_iteration_parent_span_id() {
        let tmp = tempdir().unwrap();
        let mut c = make_collector(tmp.path());
        let session_id = c.session_span_id();
        c.begin_iteration(0, "test");
        c.end_iteration(0, SpanStatus::Ok);
        c.finish();

        let content = std::fs::read_to_string(c.trace_json_path()).unwrap();
        let v: serde_json::Value = serde_json::from_str(&content).unwrap();
        let spans = v["resourceSpans"][0]["scopeSpans"][0]["spans"]
            .as_array()
            .unwrap();
        let iter_span = spans
            .iter()
            .find(|s| s["name"] == "iteration.0")
            .expect("iteration.0 span missing");
        assert_eq!(
            iter_span["parentSpanId"],
            serde_json::json!(hex8(session_id)),
            "iteration span parent must be session span"
        );
    }

    #[test]
    fn json_and_raw_formats_still_write_files() {
        use crate::debug_dump::{DebugDumper, DumpFormat, RequestDebugDump};

        let tmp = tempdir().unwrap();
        for fmt in [DumpFormat::Json, DumpFormat::Raw] {
            let d = DebugDumper::new(tmp.path(), fmt).unwrap();
            let id = d.dump_request(&RequestDebugDump {
                model_name: "test-model",
                messages: &[],
                tools: &[],
                provider_request: serde_json::json!({"model": "test-model", "max_tokens": 100}),
            });
            d.dump_response(id, "hello");
            let session_dir = std::fs::read_dir(tmp.path())
                .unwrap()
                .filter_map(std::result::Result::ok)
                .find(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                .unwrap()
                .path();
            assert!(
                session_dir.join("0000-request.json").exists(),
                "request file must exist for format {fmt:?}"
            );
        }
    }
}
