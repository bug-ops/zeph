// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Live subagent transcript forwarding (issue #6359, spec `068-subagent-transcript-forward`;
//! token-level intra-turn streaming, issue #6456, FR-002b).
//!
//! Opt-in forwarding of a running subagent's text/thinking output to the TUI runtime detail
//! view and/or a `--bare` stdout sink, under the single `forward_transcript` config flag.
//! Granularity depends on provider support: when the provider's native streaming-with-tools
//! path is available (`agent_loop.rs` drives it), text/thinking chunks are forwarded as
//! partial deltas *within* a turn; otherwise (or when streaming fails) the full, untruncated
//! text/thinking output of one completed LLM turn is forwarded once the turn completes
//! (FR-002a, unchanged). Pipeline shape:
//!
//! ```text
//! agent_loop.rs (sync, non-blocking) --try_send(RawChunk)--> per-task mpsc (cap 128)
//!     -> manager-owned per-task drain: sanitize (the ONE sanitize point) -> dispatch to sinks
//! ```
//!
//! `RawChunk` only ever travels on the ingress channel; `SanitizedChunk` is constructed
//! exclusively by the drain's sanitize step and is the only type any sink can receive
//! (NFR-005 enforced structurally, not by convention).
//!
//! # Design contract: deltas are ephemeral, display-only (FR-002b)
//!
//! Every chunk sent through `ForwardSender::send_text` / `ForwardSender::send_thinking` —
//! whether it carries a whole turn's text or one streamed delta — travels on the same
//! tail-drop `mpsc` and MUST be treated as **display-only**. A dropped chunk is a display
//! gap, never a correctness error: the loop's own accumulated response text (returned from
//! `run_agent_loop`'s LLM call and pushed into `messages`) is assembled independently of
//! whether any given delta was actually forwarded, and the guaranteed terminal chunk (see
//! `ForwardSender::send_terminal`) marks the one point a consumer may treat as authoritative
//! for "this run reached a terminal state". No consumer (TUI ring buffer, `--bare` sink, a
//! future sink) may reconstruct the subagent's conversational state — let alone feed it back
//! into the parent's LLM context — by concatenating forwarded chunks; deltas never enter any
//! LLM context, they exist purely for live human-facing display.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::sync::mpsc;
use zeph_sanitizer::pii::PiiFilter;
use zeph_sanitizer::secret_mask::SecretMaskRegistry;
use zeph_sanitizer::{ContentSanitizer, ContentSource, ContentSourceKind};

use crate::state::SubAgentState;

/// Bound on the per-task ingress channel (mpsc). `try_send` drops the newest chunk on
/// full (tail-drop) rather than blocking the subagent's own turn loop (NFR-001).
const FORWARD_CHANNEL_CAPACITY: usize = 128;

/// Maximum number of sanitized display lines retained per task in the TUI ring buffer.
const FORWARD_RING_CAPACITY: usize = 200;

/// How long a finished task's ring buffer entry survives after its terminal chunk, so a
/// TUI detail view opened just after completion still shows the final transcript.
const FORWARD_BUFFER_GRACE: Duration = Duration::from_secs(5);

/// Which consumer surfaces are active for this session, fixed at session start (session
/// scope, not hot-swappable — a headless run does not gain a TUI mid-session).
///
/// Set once via [`crate::SubAgentManager::set_forward_surfaces`] during bootstrap. When both
/// fields are `false`, no forwarding sender or drain is ever constructed for any subagent,
/// regardless of `forward_transcript` config (FR-007).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ForwardSurfaces {
    /// A TUI session is active — sanitized chunks are appended to the per-task ring buffer.
    pub tui: bool,
    /// `--bare` mode is active — sanitized chunks are written as JSON lines to stdout.
    pub bare: bool,
}

impl ForwardSurfaces {
    /// Returns `true` when at least one consumer surface is active.
    #[must_use]
    pub fn any(self) -> bool {
        self.tui || self.bare
    }
}

/// One incremental piece of a subagent's forwarded output, pre-sanitize.
///
/// Only ever travels on the per-task ingress `mpsc` — never exposed outside this module.
#[derive(Debug, Clone)]
pub(crate) struct RawChunk {
    kind: ForwardChunkKind,
}

/// The content carried by a forwarded chunk. `pub(crate)`: only ever constructed by
/// `ForwardSender`'s `send_*` methods, never named outside this crate.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub(crate) enum ForwardChunkKind {
    /// Full, untruncated text produced by one completed LLM turn (FR-002a).
    Text(String),
    /// Full, untruncated visible reasoning text from one thinking block.
    Thinking(String),
    /// End-of-transcript signal (FR-008): either the loop's own terminal status, or a
    /// synthesized backstop when the ingress channel closed without one (hard abort).
    Terminal(SubAgentState),
}

/// A forwarded chunk after passing through the drain's single sanitize stage.
///
/// Constructed only by the drain's internal sanitize step — the sole type any sink (TUI
/// ring, `--bare` stdout, a future network sink) can receive, so a sink author cannot
/// physically emit unsanitized content (NFR-005). `pub(crate)` (not `pub`, security review
/// Finding 2): nothing outside this crate needs this type — `SubAgentManager::forwarded_tail`
/// exposes already-rendered `String` lines instead — so it is not part of the public API
/// surface a future sink integration could hand-construct from.
#[derive(Debug, Clone)]
pub(crate) struct SanitizedChunk {
    /// Task ID of the originating subagent.
    pub(crate) task_id: Arc<str>,
    /// Subagent definition name.
    pub(crate) def_name: Arc<str>,
    /// Monotonic per-task sequence number (FR-003).
    pub(crate) seq: u64,
    /// The sanitized content.
    pub(crate) kind: SanitizedChunkKind,
}

/// Sanitized variant of [`ForwardChunkKind`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub(crate) enum SanitizedChunkKind {
    /// Sanitized text output.
    Text(String),
    /// Sanitized thinking output.
    Thinking(String),
    /// End-of-transcript signal, carried through unchanged (no text to sanitize).
    Terminal(SubAgentState),
}

/// The full sanitization pipeline applied at the drain's single sanitize point (NFR-005).
///
/// Bundles the baseline injection/truncation pass (`ContentSanitizer`, always present) with
/// two optional hardening layers that mirror the ones already guarding the analogous
/// sub-agent-output *egress* path (debug dumps, see `PiiScrubbingDumpSink` / #6407 and
/// `apply_secret_masking` / #5437): a [`SecretMaskRegistry`] that replaces known vault
/// secrets with opaque placeholders, and a [`PiiFilter`] that scrubs emails/phones/SSNs/etc.
/// Both are `None` unless explicitly wired via `SubAgentManager::set_secret_registry` /
/// `set_pii_filter` — forwarding remains fully functional (baseline sanitization only) when
/// neither is configured, matching this crate's existing opt-in-hardening conventions.
pub(crate) struct SanitizeLayers {
    pub(crate) sanitizer: ContentSanitizer,
    pub(crate) secret_registry: Option<Arc<SecretMaskRegistry>>,
    pub(crate) pii_filter: Option<PiiFilter>,
}

fn sanitize_text(raw_text: &str, def_name: &str, layers: &SanitizeLayers) -> String {
    let source = ContentSource::new(ContentSourceKind::ToolResult).with_identifier(def_name);
    let mut body = layers.sanitizer.sanitize(raw_text, source).body;
    if let Some(registry) = &layers.secret_registry {
        body = registry.mask(&body);
    }
    if let Some(filter) = &layers.pii_filter {
        body = filter.scrub(&body).into_owned();
    }
    body
}

/// Bounded lookback window (bytes) held back from the tail of a pending `Text`/`Thinking`
/// buffer before sanitizing and emitting its safe prefix (review Critical Issue #2, #6456
/// follow-up).
///
/// Without this, each streamed delta (FR-002b) was sanitized in complete isolation — a
/// secret or PII pattern split across two `ToolSseEvent` chunk boundaries matched neither
/// fragment individually and reached `--bare` stdout / the TUI ring buffer unmasked. Holding
/// back this many trailing bytes on every partial flush guarantees any pattern whose two
/// halves arrive within this window of each other is always sanitized as one contiguous
/// string before being released.
///
/// Chosen generously above [`crate::grants::GrantedSecret`]-delivered or vault-registered
/// secret lengths seen in practice and every PII pattern in `zeph_sanitizer::pii` (email/
/// phone/SSN/credit-card are all well under 80 bytes). A secret whose split fragments are
/// separated by *more* than this many bytes of other already-flushed content is a residual
/// limitation inherent to any bounded-window approach — not eliminated, only made
/// practically unreachable for realistic secret/PII lengths.
const SANITIZE_HOLDBACK_BYTES: usize = 256;

/// Per-task raw text accumulated but not yet sanitized/emitted (review Critical Issue #2).
///
/// Kept separate for the `Text` and `Thinking` streams since they are independent logical
/// channels that must never be concatenated with each other.
#[derive(Default)]
struct PendingSanitizeBuffers {
    text: String,
    thinking: String,
}

/// Split off `buf`'s sanitizable prefix, leaving the last `holdback` bytes (rounded down to
/// the nearest UTF-8 char boundary, same class of problem as UTF-8 chunk-boundary handling)
/// in place for a future call to potentially combine with. Pass `holdback = 0` to flush the
/// entire remaining buffer — used once no more data for this task is coming (an explicit
/// `Terminal` chunk or the hard-abort backstop), so buffered content is only ever delayed,
/// never silently dropped. Returns `None` when there is nothing new to emit yet.
fn split_off_safe_prefix(buf: &mut String, holdback: usize) -> Option<String> {
    if buf.is_empty() {
        return None;
    }
    let target = buf.len().saturating_sub(holdback);
    let boundary = buf.floor_char_boundary(target);
    if boundary == 0 {
        return None;
    }
    let prefix = buf[..boundary].to_owned();
    buf.drain(..boundary);
    Some(prefix)
}

/// Attempt to flush a pending buffer's safe prefix, sanitize it, and wrap the result via
/// `wrap_kind` (`SanitizedChunkKind::Text` or `::Thinking`, both valid as a
/// `fn(String) -> SanitizedChunkKind` since each is a single-field tuple variant). Returns
/// `None` when [`split_off_safe_prefix`] found nothing new to emit yet.
fn try_flush_kind(
    buf: &mut String,
    holdback: usize,
    def_name: &str,
    layers: &SanitizeLayers,
    wrap_kind: fn(String) -> SanitizedChunkKind,
) -> Option<SanitizedChunkKind> {
    let safe = split_off_safe_prefix(buf, holdback)?;
    Some(wrap_kind(sanitize_text(&safe, def_name, layers)))
}

fn make_sanitized_chunk(
    task_id: &Arc<str>,
    def_name: &Arc<str>,
    seq: u64,
    kind: SanitizedChunkKind,
) -> SanitizedChunk {
    SanitizedChunk {
        task_id: Arc::clone(task_id),
        def_name: Arc::clone(def_name),
        seq,
        kind,
    }
}

/// Flush both pending buffers in full (no holdback — nothing more is coming for this task)
/// and dispatch any resulting chunk(s). Called immediately before an explicit `Terminal`
/// chunk or the hard-abort backstop, so buffered content is only ever delayed until the
/// run's very end, never silently dropped.
#[allow(clippy::too_many_arguments)]
fn flush_all_pending(
    pending: &mut PendingSanitizeBuffers,
    task_id: &Arc<str>,
    def_name: &Arc<str>,
    layers: &SanitizeLayers,
    surfaces: ForwardSurfaces,
    buffer: &ForwardBuffer,
    dispatch: &mut impl FnMut(&SanitizedChunk, ForwardSurfaces, &ForwardBuffer),
    emit_seq: &mut u64,
) {
    if let Some(kind) = try_flush_kind(
        &mut pending.text,
        0,
        def_name.as_ref(),
        layers,
        SanitizedChunkKind::Text,
    ) {
        dispatch(
            &make_sanitized_chunk(task_id, def_name, *emit_seq, kind),
            surfaces,
            buffer,
        );
        *emit_seq += 1;
    }
    if let Some(kind) = try_flush_kind(
        &mut pending.thinking,
        0,
        def_name.as_ref(),
        layers,
        SanitizedChunkKind::Thinking,
    ) {
        dispatch(
            &make_sanitized_chunk(task_id, def_name, *emit_seq, kind),
            surfaces,
            buffer,
        );
        *emit_seq += 1;
    }
}

/// Sender-side handle held by a single subagent's own turn loop for the lifetime of its
/// run only.
///
/// Deliberately **not** `Clone`: the drain's hard-abort backstop (see [`run_forward_drain`])
/// relies on this being the sole `mpsc::Sender` for its task — dropping the loop's future
/// must be the only way the channel closes. Do not store this (or its inner `Sender`) in
/// any struct that outlives a single subagent run (`SpawnContext`, a resume/retry retainer,
/// etc.) — see P-new-3 in the implementation handoff.
pub(crate) struct ForwardSender {
    tx: mpsc::Sender<RawChunk>,
    task_id: Arc<str>,
    def_name: Arc<str>,
    seq: AtomicU64,
    dropped: AtomicU64,
}

impl ForwardSender {
    pub(crate) fn new(tx: mpsc::Sender<RawChunk>, task_id: Arc<str>, def_name: Arc<str>) -> Self {
        Self {
            tx,
            task_id,
            def_name,
            seq: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
        }
    }

    fn try_send(&self, kind: ForwardChunkKind) {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let chunk = RawChunk { kind };
        if self.tx.try_send(chunk).is_ok() {
            tracing::debug!(
                task_id = %self.task_id,
                def_name = %self.def_name,
                seq,
                "subagent.forward.emit"
            );
        } else {
            let dropped = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
            tracing::warn!(
                task_id = %self.task_id,
                def_name = %self.def_name,
                seq,
                dropped,
                "subagent.forward.drop: ingress channel full, chunk dropped"
            );
        }
    }

    /// Forward a piece of assistant text output. Call only from behind an
    /// `if let Some(f) = forward` guard — the caller (`agent_loop.rs`) must never construct
    /// or clone the text ahead of that guard (FR-007).
    ///
    /// `text` may be a whole turn's full, untruncated text (FR-002a, the non-streaming or
    /// stream-fallback path) or one incremental delta from a native streaming response
    /// (FR-002b) — both are display-only chunks tail-dropped under backpressure identically;
    /// see the module-level "Design contract" section. Callers must not send both the
    /// streamed deltas and the final whole-turn text for the same turn — that would double-
    /// forward the same content (see `agent_loop.rs::call_provider_with_status`'s `streamed`
    /// flag).
    pub(crate) fn send_text(&self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.try_send(ForwardChunkKind::Text(text.to_owned()));
    }

    /// Forward a piece of visible thinking output — a whole completed thinking block
    /// (FR-002a) or one incremental thinking delta (FR-002b). Same no-op-behind-`Some`
    /// contract and no-double-forward caller responsibility as [`send_text`][Self::send_text].
    pub(crate) fn send_thinking(&self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.try_send(ForwardChunkKind::Thinking(text.to_owned()));
    }

    /// Emit the terminal (end-of-transcript) chunk. Co-located with every site that
    /// publishes a terminal `SubAgentStatus` on the status channel (FR-008).
    pub(crate) fn send_terminal(&self, state: SubAgentState) {
        tracing::debug!(task_id = %self.task_id, ?state, "subagent.forward.terminal");
        self.try_send(ForwardChunkKind::Terminal(state));
    }
}

pub(crate) type ForwardBuffer = std::sync::Mutex<HashMap<String, VecDeque<String>>>;

/// Render a sanitized chunk as a single display line for the TUI ring buffer, or `None`
/// for chunks that carry no display text (terminal events).
fn display_line(kind: &SanitizedChunkKind) -> Option<String> {
    match kind {
        SanitizedChunkKind::Text(t) => Some(t.clone()),
        SanitizedChunkKind::Thinking(t) => Some(format!("[thinking] {t}")),
        SanitizedChunkKind::Terminal(_) => None,
    }
}

fn state_str(state: SubAgentState) -> &'static str {
    match state {
        SubAgentState::Submitted => "submitted",
        SubAgentState::Working => "working",
        SubAgentState::Completed => "completed",
        SubAgentState::Failed => "failed",
        SubAgentState::Canceled => "canceled",
    }
}

/// Write one `--bare` stdout event as a single JSON line (M6: one `println!` per chunk,
/// never multi-write — `println!` takes Rust's internal stdout lock per call, so this is
/// line-atomic even when interleaved with the main output path).
fn emit_bare_line(chunk: &SanitizedChunk) {
    #[derive(serde::Serialize)]
    struct BareForwardEvent<'a> {
        task_id: &'a str,
        def_name: &'a str,
        seq: u64,
        kind: &'static str,
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        state: Option<&'static str>,
    }

    let (kind, content, state) = match &chunk.kind {
        SanitizedChunkKind::Text(t) => ("text", Some(t.as_str()), None),
        SanitizedChunkKind::Thinking(t) => ("thinking", Some(t.as_str()), None),
        SanitizedChunkKind::Terminal(s) => ("terminal", None, Some(state_str(*s))),
    };
    let event = BareForwardEvent {
        task_id: &chunk.task_id,
        def_name: &chunk.def_name,
        seq: chunk.seq,
        kind,
        content,
        state,
    };
    if let Ok(line) = serde_json::to_string(&event) {
        println!("{line}");
    }
}

/// Dispatch one sanitized chunk to every active surface.
fn dispatch_chunk(chunk: &SanitizedChunk, surfaces: ForwardSurfaces, buffer: &ForwardBuffer) {
    if surfaces.tui
        && let Some(line) = display_line(&chunk.kind)
    {
        let mut guard = buffer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let ring = guard.entry(chunk.task_id.to_string()).or_default();
        ring.push_back(line);
        while ring.len() > FORWARD_RING_CAPACITY {
            ring.pop_front();
        }
    }
    if surfaces.bare {
        emit_bare_line(chunk);
    }
}

/// Build a fresh `mpsc` ingress pair and its sender-side handle for one subagent run.
pub(crate) fn new_channel(
    task_id: Arc<str>,
    def_name: Arc<str>,
) -> (ForwardSender, mpsc::Receiver<RawChunk>) {
    let (tx, rx) = mpsc::channel(FORWARD_CHANNEL_CAPACITY);
    (ForwardSender::new(tx, task_id, def_name), rx)
}

/// Manager-owned per-task drain: the single sanitize stage plus sink dispatch, running for
/// the lifetime of one subagent's forwarding channel.
///
/// # Terminal detection (critic C-new-1, must-fix)
///
/// The loop breaks immediately after dispatching **any** explicit terminal chunk (sent by
/// `agent_loop.rs` at each of its three terminal-status sites). This is the only way to
/// avoid double-emitting a terminal on the happy path: on normal completion the loop sends
/// an explicit `Terminal` and then drops its `Sender`; because the `Some(raw)` arm below
/// breaks unconditionally on a terminal chunk, `recv()` is never called again afterward, so
/// the `None` arm can never fire once an explicit terminal has already been handled.
/// Consequently, reaching the `None` arm at all — the channel closed with no message
/// pending — is *only* possible when no explicit terminal was ever sent, i.e. the genuine
/// hard-abort backstop (`JoinHandle::abort()` / cancel-token firing mid-`.await` drops the
/// loop's future, and with it its sole `Sender`, before any terminal-status site runs): it
/// unconditionally synthesizes `Terminal(Canceled)`.
///
/// After the loop ends, the task's ring buffer entry is evicted following a short grace
/// window so a TUI detail view opened just after completion still shows the final
/// transcript (S3: bounds `forward_buffer` growth across a long multi-subagent session).
pub(crate) async fn run_forward_drain(
    task_id: Arc<str>,
    def_name: Arc<str>,
    rx: mpsc::Receiver<RawChunk>,
    layers: SanitizeLayers,
    surfaces: ForwardSurfaces,
    buffer: Arc<ForwardBuffer>,
) {
    run_forward_drain_with(
        task_id,
        def_name,
        rx,
        layers,
        surfaces,
        buffer,
        dispatch_chunk,
    )
    .await;
}

/// Same as [`run_forward_drain`], parameterized over the dispatch step so tests can observe
/// exactly how many (and which) [`SanitizedChunk`]s the drain hands to the sinks — including
/// `Terminal` chunks, which [`dispatch_chunk`] itself never writes to the TUI ring buffer
/// (`display_line` returns `None` for them) and which the eviction sweep runs unconditionally
/// after either loop exit, so buffer *contents* alone cannot distinguish "exactly one terminal
/// dispatched" from "two". Production always calls this via [`run_forward_drain`] with
/// [`dispatch_chunk`] itself as the dispatch step — behavior is unchanged.
async fn run_forward_drain_with(
    task_id: Arc<str>,
    def_name: Arc<str>,
    mut rx: mpsc::Receiver<RawChunk>,
    layers: SanitizeLayers,
    surfaces: ForwardSurfaces,
    buffer: Arc<ForwardBuffer>,
    mut dispatch: impl FnMut(&SanitizedChunk, ForwardSurfaces, &ForwardBuffer),
) {
    let mut pending = PendingSanitizeBuffers::default();
    let mut emit_seq: u64 = 0;

    loop {
        if let Some(raw) = rx.recv().await {
            match raw.kind {
                ForwardChunkKind::Text(delta) => {
                    pending.text.push_str(&delta);
                    if let Some(kind) = try_flush_kind(
                        &mut pending.text,
                        SANITIZE_HOLDBACK_BYTES,
                        def_name.as_ref(),
                        &layers,
                        SanitizedChunkKind::Text,
                    ) {
                        dispatch(
                            &make_sanitized_chunk(&task_id, &def_name, emit_seq, kind),
                            surfaces,
                            &buffer,
                        );
                        emit_seq += 1;
                    }
                }
                ForwardChunkKind::Thinking(delta) => {
                    pending.thinking.push_str(&delta);
                    if let Some(kind) = try_flush_kind(
                        &mut pending.thinking,
                        SANITIZE_HOLDBACK_BYTES,
                        def_name.as_ref(),
                        &layers,
                        SanitizedChunkKind::Thinking,
                    ) {
                        dispatch(
                            &make_sanitized_chunk(&task_id, &def_name, emit_seq, kind),
                            surfaces,
                            &buffer,
                        );
                        emit_seq += 1;
                    }
                }
                ForwardChunkKind::Terminal(state) => {
                    flush_all_pending(
                        &mut pending,
                        &task_id,
                        &def_name,
                        &layers,
                        surfaces,
                        &buffer,
                        &mut dispatch,
                        &mut emit_seq,
                    );
                    let chunk = make_sanitized_chunk(
                        &task_id,
                        &def_name,
                        emit_seq,
                        SanitizedChunkKind::Terminal(state),
                    );
                    dispatch(&chunk, surfaces, &buffer);
                    break;
                }
            }
        } else {
            tracing::warn!(
                task_id = %task_id,
                "subagent.forward.terminal: ingress channel closed without an explicit \
                 terminal chunk — synthesizing hard-abort backstop"
            );
            flush_all_pending(
                &mut pending,
                &task_id,
                &def_name,
                &layers,
                surfaces,
                &buffer,
                &mut dispatch,
                &mut emit_seq,
            );
            let synthesized = make_sanitized_chunk(
                &task_id,
                &def_name,
                emit_seq,
                SanitizedChunkKind::Terminal(SubAgentState::Canceled),
            );
            dispatch(&synthesized, surfaces, &buffer);
            break;
        }
    }

    tokio::time::sleep(FORWARD_BUFFER_GRACE).await;
    buffer
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(task_id.as_ref());
}

/// Read the current ring-buffer tail for `task_id` (up to the last `n` lines).
///
/// Returns an empty vector for a task with no forwarded lines yet (or forwarding inactive).
pub(crate) fn forwarded_tail(buffer: &ForwardBuffer, task_id: &str, n: usize) -> Vec<String> {
    let guard = buffer
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    guard.get(task_id).map_or_else(Vec::new, |ring| {
        ring.iter().rev().take(n).rev().cloned().collect()
    })
}

/// Construct a fresh, empty forwarding ring buffer.
pub(crate) fn new_buffer() -> Arc<ForwardBuffer> {
    Arc::new(std::sync::Mutex::new(HashMap::new()))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use zeph_config::sanitizer::PiiFilterConfig;

    use super::*;

    fn layers() -> SanitizeLayers {
        SanitizeLayers {
            sanitizer: ContentSanitizer::new(&zeph_config::ContentIsolationConfig::default()),
            secret_registry: None,
            pii_filter: None,
        }
    }

    /// Runs the drain via [`run_forward_drain_with`], counting how many `Terminal` chunks
    /// were actually handed to the dispatch step — the direct, discriminating observable for
    /// critic C-new-1 (a regression that re-introduces the double-terminal bug increments this
    /// to 2; buffer state and hang/panic-absence cannot tell the two implementations apart,
    /// since `dispatch_chunk` never writes `Terminal` chunks to the ring buffer and the
    /// post-loop eviction runs exactly once regardless of how many terminals were dispatched
    /// beforehand).
    async fn run_and_count_terminals(
        task_id: Arc<str>,
        def_name: Arc<str>,
        rx: mpsc::Receiver<RawChunk>,
        surfaces: ForwardSurfaces,
        buffer: Arc<ForwardBuffer>,
    ) -> usize {
        let terminal_dispatches = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&terminal_dispatches);
        run_forward_drain_with(
            task_id,
            def_name,
            rx,
            layers(),
            surfaces,
            buffer,
            move |chunk, surfaces, buffer| {
                if matches!(chunk.kind, SanitizedChunkKind::Terminal(_)) {
                    counter.fetch_add(1, Ordering::SeqCst);
                }
                dispatch_chunk(chunk, surfaces, buffer);
            },
        )
        .await;
        terminal_dispatches.load(Ordering::SeqCst)
    }

    #[tokio::test(start_paused = true)]
    async fn happy_path_emits_no_spurious_second_terminal() {
        // Regression guard for critic C-new-1: an explicit Terminal followed by Sender drop
        // must produce exactly one terminal dispatch, not two. Asserts on the actual dispatch
        // count (see `run_and_count_terminals`), not on buffer state — a Terminal chunk is
        // never written to the ring buffer, so buffer-only assertions cannot detect this
        // regression (confirmed by the testing validator).
        let task_id: Arc<str> = Arc::from("task-1");
        let def_name: Arc<str> = Arc::from("agent-1");
        let (sender, rx) = new_channel(Arc::clone(&task_id), Arc::clone(&def_name));
        let buffer = new_buffer();

        sender.send_text("hello");
        sender.send_terminal(SubAgentState::Completed);
        drop(sender);

        let terminal_count = run_and_count_terminals(
            Arc::clone(&task_id),
            def_name,
            rx,
            ForwardSurfaces {
                tui: true,
                bare: false,
            },
            Arc::clone(&buffer),
        )
        .await;

        assert_eq!(
            terminal_count, 1,
            "exactly one terminal chunk must be dispatched — a second would mean the drain \
             looped back to recv() after the explicit terminal (C-new-1 regression)"
        );
        let tail = forwarded_tail(&buffer, &task_id, 10);
        assert!(
            tail.is_empty(),
            "buffer entry must be evicted after grace window"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn hard_abort_without_explicit_terminal_synthesizes_backstop() {
        let task_id: Arc<str> = Arc::from("task-2");
        let def_name: Arc<str> = Arc::from("agent-2");
        let (sender, rx) = new_channel(Arc::clone(&task_id), Arc::clone(&def_name));
        let buffer = new_buffer();

        sender.send_text("partial output");
        drop(sender); // simulate abort: no explicit terminal was ever sent

        let terminal_count = run_and_count_terminals(
            Arc::clone(&task_id),
            def_name,
            rx,
            ForwardSurfaces {
                tui: true,
                bare: false,
            },
            buffer,
        )
        .await;

        assert_eq!(
            terminal_count, 1,
            "exactly one synthesized backstop terminal must be dispatched on hard abort"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn zero_consumer_surfaces_still_drains_without_panicking() {
        let task_id: Arc<str> = Arc::from("task-3");
        let def_name: Arc<str> = Arc::from("agent-3");
        let (sender, rx) = new_channel(Arc::clone(&task_id), Arc::clone(&def_name));
        let buffer = new_buffer();

        sender.send_text("no one is listening");
        sender.send_terminal(SubAgentState::Completed);
        drop(sender);

        run_forward_drain(
            task_id,
            def_name,
            rx,
            layers(),
            ForwardSurfaces::default(),
            buffer,
        )
        .await;
    }

    #[tokio::test(start_paused = true)]
    async fn secret_registry_masks_forwarded_text_and_thinking() {
        // NFR-005 / security Finding 1: forwarded content containing a registered vault
        // secret must come out masked, not verbatim.
        use zeph_sanitizer::secret_mask::{SecretCategory, SecretMaskRegistry};

        let registry = Arc::new(SecretMaskRegistry::new());
        registry.register(
            "MY_KEY",
            "sk-live-topsecretvalue123",
            SecretCategory::ApiKey,
        );

        let task_id: Arc<str> = Arc::from("task-secret");
        let def_name: Arc<str> = Arc::from("agent-secret");
        let (sender, rx) = new_channel(Arc::clone(&task_id), Arc::clone(&def_name));
        let buffer = new_buffer();

        sender.send_text("the key is sk-live-topsecretvalue123, use it wisely");
        sender.send_thinking("I will use sk-live-topsecretvalue123 to authenticate");
        sender.send_terminal(SubAgentState::Completed);
        drop(sender);

        let seen = Arc::new(std::sync::Mutex::new(Vec::<SanitizedChunk>::new()));
        let collected = Arc::clone(&seen);
        let layers = SanitizeLayers {
            sanitizer: ContentSanitizer::new(&zeph_config::ContentIsolationConfig::default()),
            secret_registry: Some(registry),
            pii_filter: None,
        };
        run_forward_drain_with(
            task_id,
            def_name,
            rx,
            layers,
            ForwardSurfaces {
                tui: true,
                bare: false,
            },
            buffer,
            move |chunk, surfaces, buffer| {
                collected.lock().unwrap().push(chunk.clone());
                dispatch_chunk(chunk, surfaces, buffer);
            },
        )
        .await;

        let chunks = seen.lock().unwrap();
        for chunk in chunks.iter() {
            match &chunk.kind {
                SanitizedChunkKind::Text(t) | SanitizedChunkKind::Thinking(t) => {
                    assert!(
                        !t.contains("sk-live-topsecretvalue123"),
                        "forwarded content must not contain the raw secret: {t}"
                    );
                }
                SanitizedChunkKind::Terminal(_) => {}
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn pii_filter_scrubs_forwarded_email() {
        // NFR-005 / security Finding 1: forwarded content containing PII-shaped text must be
        // scrubbed when a PiiFilter layer is configured.
        let task_id: Arc<str> = Arc::from("task-pii");
        let def_name: Arc<str> = Arc::from("agent-pii");
        let (sender, rx) = new_channel(Arc::clone(&task_id), Arc::clone(&def_name));
        let buffer = new_buffer();

        sender.send_text("contact me at victim@example.com for details");
        sender.send_terminal(SubAgentState::Completed);
        drop(sender);

        let seen = Arc::new(std::sync::Mutex::new(Vec::<SanitizedChunk>::new()));
        let collected = Arc::clone(&seen);
        let layers = SanitizeLayers {
            sanitizer: ContentSanitizer::new(&zeph_config::ContentIsolationConfig::default()),
            secret_registry: None,
            pii_filter: Some(PiiFilter::new(PiiFilterConfig::default())),
        };
        run_forward_drain_with(
            task_id,
            def_name,
            rx,
            layers,
            ForwardSurfaces {
                tui: true,
                bare: false,
            },
            buffer,
            move |chunk, surfaces, buffer| {
                collected.lock().unwrap().push(chunk.clone());
                dispatch_chunk(chunk, surfaces, buffer);
            },
        )
        .await;

        let chunks = seen.lock().unwrap();
        let text_chunk = chunks
            .iter()
            .find(|c| matches!(c.kind, SanitizedChunkKind::Text(_)))
            .expect("one text chunk must have been dispatched");
        let SanitizedChunkKind::Text(ref t) = text_chunk.kind else {
            unreachable!()
        };
        assert!(
            !t.contains("victim@example.com"),
            "forwarded content must not contain the raw email address: {t}"
        );
    }

    // --- Review Critical Issue #2: cross-delta secret/PII masking gap ---

    fn collect_forwarded_text(chunks: &[SanitizedChunk]) -> String {
        chunks
            .iter()
            .filter_map(|c| match &c.kind {
                SanitizedChunkKind::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect()
    }

    #[tokio::test(start_paused = true)]
    async fn secret_split_across_two_deltas_is_still_masked() {
        // A secret whose bytes are split across two separate `send_text` calls — simulating
        // two ToolSseEvent::ContentChunk deltas arriving back-to-back during FR-002b
        // streaming — must still be masked once both fragments have been buffered. Neither
        // fragment alone contains the full registered secret value, so per-delta-isolated
        // sanitization (the pre-fix behavior) would have let it straight through.
        use zeph_sanitizer::secret_mask::{SecretCategory, SecretMaskRegistry};

        let secret_value = "sk-live-topsecretvalue123456789";
        let registry = Arc::new(SecretMaskRegistry::new());
        registry.register("MY_KEY", secret_value, SecretCategory::ApiKey);
        let (first_half, second_half) = secret_value.split_at(secret_value.len() / 2);

        let task_id: Arc<str> = Arc::from("task-split");
        let def_name: Arc<str> = Arc::from("agent-split");
        let (sender, rx) = new_channel(Arc::clone(&task_id), Arc::clone(&def_name));
        let buffer = new_buffer();

        sender.send_text(&format!("the key is {first_half}"));
        sender.send_text(&format!("{second_half}, use it wisely"));
        sender.send_terminal(SubAgentState::Completed);
        drop(sender);

        let seen = Arc::new(std::sync::Mutex::new(Vec::<SanitizedChunk>::new()));
        let collected = Arc::clone(&seen);
        let layers = SanitizeLayers {
            sanitizer: ContentSanitizer::new(&zeph_config::ContentIsolationConfig::default()),
            secret_registry: Some(registry),
            pii_filter: None,
        };
        run_forward_drain_with(
            task_id,
            def_name,
            rx,
            layers,
            ForwardSurfaces {
                tui: true,
                bare: false,
            },
            buffer,
            move |chunk, surfaces, buffer| {
                collected.lock().unwrap().push(chunk.clone());
                dispatch_chunk(chunk, surfaces, buffer);
            },
        )
        .await;

        let combined = collect_forwarded_text(&seen.lock().unwrap());
        assert!(
            !combined.contains(secret_value),
            "secret split across two forwarded deltas must still be masked: {combined}"
        );
        assert!(
            combined.contains("<SECRET:api_key:"),
            "masked placeholder must be present in the combined forwarded text: {combined}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn email_split_across_two_deltas_is_still_scrubbed() {
        // Same cross-delta gap, PII side: an email address split across two `send_text`
        // calls must still be scrubbed once both fragments are buffered together.
        let email = "victim@example.com";
        let (first_half, second_half) = email.split_at(email.len() / 2);

        let task_id: Arc<str> = Arc::from("task-split-pii");
        let def_name: Arc<str> = Arc::from("agent-split-pii");
        let (sender, rx) = new_channel(Arc::clone(&task_id), Arc::clone(&def_name));
        let buffer = new_buffer();

        sender.send_text(&format!("contact me at {first_half}"));
        sender.send_text(&format!("{second_half} for details"));
        sender.send_terminal(SubAgentState::Completed);
        drop(sender);

        let seen = Arc::new(std::sync::Mutex::new(Vec::<SanitizedChunk>::new()));
        let collected = Arc::clone(&seen);
        let layers = SanitizeLayers {
            sanitizer: ContentSanitizer::new(&zeph_config::ContentIsolationConfig::default()),
            secret_registry: None,
            pii_filter: Some(PiiFilter::new(PiiFilterConfig::default())),
        };
        run_forward_drain_with(
            task_id,
            def_name,
            rx,
            layers,
            ForwardSurfaces {
                tui: true,
                bare: false,
            },
            buffer,
            move |chunk, surfaces, buffer| {
                collected.lock().unwrap().push(chunk.clone());
                dispatch_chunk(chunk, surfaces, buffer);
            },
        )
        .await;

        let combined = collect_forwarded_text(&seen.lock().unwrap());
        assert!(
            !combined.contains(email),
            "email split across two forwarded deltas must still be scrubbed: {combined}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn secret_split_across_progressive_flush_boundary_is_still_masked() {
        // Stronger test of the holdback *window* itself (not just "buffer until terminal"):
        // enough filler precedes the secret's two fragments to force at least one
        // progressive flush mid-stream (SANITIZE_HOLDBACK_BYTES is well under the total
        // filler size), proving flushing genuinely happens before the terminal event, yet
        // the secret's fragments — arriving back-to-back right after the filler — must still
        // land inside the held-back tail and be masked as one contiguous string once
        // fully buffered.
        use zeph_sanitizer::secret_mask::{SecretCategory, SecretMaskRegistry};

        let secret_value = "sk-live-anothersecretvalue987654321";
        let registry = Arc::new(SecretMaskRegistry::new());
        registry.register("MY_KEY", secret_value, SecretCategory::ApiKey);
        let (first_half, second_half) = secret_value.split_at(secret_value.len() / 2);

        let task_id: Arc<str> = Arc::from("task-window");
        let def_name: Arc<str> = Arc::from("agent-window");
        let (sender, rx) = new_channel(Arc::clone(&task_id), Arc::clone(&def_name));
        let buffer = new_buffer();

        for i in 0..40 {
            sender.send_text(&format!("filler-chunk-{i:03} "));
        }
        sender.send_text(first_half);
        sender.send_text(second_half);
        sender.send_terminal(SubAgentState::Completed);
        drop(sender);

        let seen = Arc::new(std::sync::Mutex::new(Vec::<SanitizedChunk>::new()));
        let collected = Arc::clone(&seen);
        let layers = SanitizeLayers {
            sanitizer: ContentSanitizer::new(&zeph_config::ContentIsolationConfig::default()),
            secret_registry: Some(registry),
            pii_filter: None,
        };
        run_forward_drain_with(
            task_id,
            def_name,
            rx,
            layers,
            ForwardSurfaces {
                tui: true,
                bare: false,
            },
            buffer,
            move |chunk, surfaces, buffer| {
                collected.lock().unwrap().push(chunk.clone());
                dispatch_chunk(chunk, surfaces, buffer);
            },
        )
        .await;

        let seen = seen.lock().unwrap();
        let text_chunk_count = seen
            .iter()
            .filter(|c| matches!(c.kind, SanitizedChunkKind::Text(_)))
            .count();
        assert!(
            text_chunk_count > 1,
            "filler well over the holdback window must have produced at least one \
             progressive flush before the terminal-triggered final flush, got \
             {text_chunk_count} text chunk(s)"
        );
        let combined = collect_forwarded_text(&seen);
        assert!(
            !combined.contains(secret_value),
            "secret split across the streaming boundary must still be masked: {combined}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn buffer_entry_survives_during_grace_window_then_evicted() {
        // S3: the grace window's entire purpose is that a TUI view opened just after
        // completion still sees the transcript — verify the mid-window state directly with
        // controlled virtual-time stepping, not just the post-eviction end state.
        let task_id: Arc<str> = Arc::from("task-grace");
        let def_name: Arc<str> = Arc::from("agent-grace");
        let (sender, rx) = new_channel(Arc::clone(&task_id), Arc::clone(&def_name));
        let buffer = new_buffer();

        sender.send_text("visible during the grace window");
        sender.send_terminal(SubAgentState::Completed);
        drop(sender);

        let drain_buffer = Arc::clone(&buffer);
        let drain_task_id = Arc::clone(&task_id);
        let handle = tokio::spawn(run_forward_drain(
            drain_task_id,
            def_name,
            rx,
            layers(),
            ForwardSurfaces {
                tui: true,
                bare: false,
            },
            drain_buffer,
        ));

        // Let the drain process both chunks and enter its grace-window sleep.
        tokio::time::advance(Duration::from_millis(1)).await;
        tokio::task::yield_now().await;

        let mid_window_tail = forwarded_tail(&buffer, &task_id, 10);
        assert_eq!(
            mid_window_tail.len(),
            1,
            "exactly one forwarded line expected"
        );
        assert!(
            mid_window_tail[0].contains("visible during the grace window"),
            "the transcript must still be visible during the grace window, got: {:?}",
            mid_window_tail[0]
        );

        tokio::time::advance(FORWARD_BUFFER_GRACE + Duration::from_millis(1)).await;
        handle.await.expect("drain task must not panic");

        let post_eviction_tail = forwarded_tail(&buffer, &task_id, 10);
        assert!(
            post_eviction_tail.is_empty(),
            "buffer entry must be evicted once the grace window elapses"
        );
    }

    #[test]
    fn empty_text_is_not_sent() {
        let task_id: Arc<str> = Arc::from("task-4");
        let def_name: Arc<str> = Arc::from("agent-4");
        let (sender, mut rx) = new_channel(task_id, def_name);
        sender.send_text("");
        sender.send_thinking("");
        drop(sender);
        assert!(
            rx.try_recv().is_err(),
            "empty text/thinking must not be sent onto the ingress channel"
        );
    }

    #[test]
    fn channel_full_increments_drop_counter_and_does_not_panic() {
        let task_id: Arc<str> = Arc::from("task-5");
        let def_name: Arc<str> = Arc::from("agent-5");
        let (sender, mut rx) = new_channel(task_id, def_name);
        for i in 0..FORWARD_CHANNEL_CAPACITY + 10 {
            sender.send_text(&format!("chunk {i}"));
        }
        // Drain a few to prove the channel still functions after overflow.
        let mut received = 0;
        while rx.try_recv().is_ok() {
            received += 1;
        }
        assert!(
            received > 0,
            "at least some chunks must have been delivered"
        );
        assert!(
            received <= FORWARD_CHANNEL_CAPACITY,
            "received must never exceed channel capacity"
        );
    }

    #[test]
    fn forward_surfaces_any() {
        assert!(!ForwardSurfaces::default().any());
        assert!(
            ForwardSurfaces {
                tui: true,
                bare: false
            }
            .any()
        );
        assert!(
            ForwardSurfaces {
                tui: false,
                bare: true
            }
            .any()
        );
    }
}
