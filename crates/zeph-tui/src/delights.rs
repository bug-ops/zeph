// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Micro-delight state types for the TUI dashboard (#5104).
//!
//! All animation math is a pure function of `(state, anim_tick)` — deterministic
//! and snapshot-testable. Wall-clock (`Instant`) is used only for the streaming
//! rate estimate, which is display-only and never fed into layout math.

use std::collections::VecDeque;
use std::time::Instant;

// ── Constants ────────────────────────────────────────────────────────────────

/// Lifetime of a toast in animation ticks. ≈3s at 10fps (100ms/tick).
pub(crate) const TOAST_TTL_TICKS: u64 = 30;

/// Number of samples kept for the tok/s rolling window.
const RATE_SAMPLES: usize = 8;

/// Maximum number of toasts displayed simultaneously.
const TOAST_CAP: usize = 3;

/// Duration of the splash shimmer sweep in animation ticks. ≈1.2s at 10fps.
pub(crate) const SHIMMER_TICKS: u64 = 12;

// ── StreamRate ───────────────────────────────────────────────────────────────

/// Approximate streaming rate and TTFT for the status bar.
///
/// Tracks chunk-arrival times (not per-token), so tok/s is an estimate.
/// Display-only — never fed into layout math or persisted.
pub(crate) struct StreamRate {
    /// Rolling window: `(sample_instant, cumulative_completion_tokens)`.
    samples: VecDeque<(Instant, u64)>,
    /// TTFT from the last completed turn (milliseconds).
    pub(crate) last_ttft_ms: Option<u64>,
    /// When the current agent turn started (set on `AgentEvent::Typing`).
    pub(crate) turn_start: Option<Instant>,
    /// When the first streaming token of the current turn arrived.
    pub(crate) first_token_at: Option<Instant>,
}

impl StreamRate {
    pub(crate) fn new() -> Self {
        Self {
            samples: VecDeque::with_capacity(RATE_SAMPLES),
            last_ttft_ms: None,
            turn_start: None,
            first_token_at: None,
        }
    }

    /// Called at turn-start (`AgentEvent::Typing`).
    pub(crate) fn on_turn_start(&mut self) {
        self.turn_start = Some(Instant::now());
        self.first_token_at = None;
        self.samples.clear();
    }

    /// Called on each streaming chunk with the current cumulative completion token count.
    pub(crate) fn on_token_chunk(&mut self, completion_tokens: u64) {
        let now = Instant::now();
        if self.first_token_at.is_none() {
            self.first_token_at = Some(now);
            if let Some(start) = self.turn_start {
                #[allow(clippy::cast_possible_truncation)]
                let ttft = now.duration_since(start).as_millis() as u64;
                self.last_ttft_ms = Some(ttft);
            }
        }
        if self.samples.len() == RATE_SAMPLES {
            self.samples.pop_front();
        }
        self.samples.push_back((now, completion_tokens));
    }

    /// TTFT (time-to-first-token) from the last completed turn, in milliseconds.
    ///
    /// Returns `None` when no turn has completed since the rate tracker was created.
    #[must_use]
    pub(crate) fn ttft_ms(&self) -> Option<u64> {
        self.last_ttft_ms
    }

    /// Compute current tok/s using the rolling window (EMA-smoothed over window).
    ///
    /// Returns `None` when fewer than 2 samples are available.
    #[must_use]
    pub(crate) fn tokens_per_sec(&self) -> Option<f32> {
        if self.samples.len() < 2 {
            return None;
        }
        let (t0, c0) = &self.samples[0];
        let (tn, cn) = self.samples.back()?;
        let dt = tn.duration_since(*t0).as_secs_f32();
        if dt < 0.001 {
            return None;
        }
        #[allow(clippy::cast_precision_loss)]
        let tokens = cn.saturating_sub(*c0) as f32;
        Some(tokens / dt)
    }
}

// ── ToastKind ────────────────────────────────────────────────────────────────

/// Visual category of a toast notification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToastKind {
    /// Informational notice (default colour).
    #[allow(dead_code)]
    Info,
    /// Success / completion notice (green tint).
    Success,
    /// Warning notice (yellow tint).
    #[allow(dead_code)]
    Warn,
}

// ── Toast + ToastQueue ───────────────────────────────────────────────────────

pub(crate) struct Toast {
    pub(crate) text: String,
    pub(crate) kind: ToastKind,
    /// `anim_tick` when this toast was enqueued — drives deterministic expiry.
    pub(crate) born_tick: u64,
}

/// Queue of ephemeral overlay toasts, capped at [`TOAST_CAP`].
///
/// `push_toast` is render-thread-only — never call from background tasks.
/// Off-thread toast origins must be routed as `AgentEvent`/`AppEvent` variants
/// and enqueued inside the event handler.
pub(crate) struct ToastQueue {
    items: VecDeque<Toast>,
}

impl ToastQueue {
    pub(crate) fn new() -> Self {
        Self {
            items: VecDeque::with_capacity(TOAST_CAP),
        }
    }

    /// Enqueue a new toast. When the queue is full, the oldest entry is dropped.
    pub(crate) fn push(&mut self, text: impl Into<String>, kind: ToastKind, born_tick: u64) {
        if self.items.len() == TOAST_CAP {
            self.items.pop_front();
        }
        self.items.push_back(Toast {
            text: text.into(),
            kind,
            born_tick,
        });
    }

    /// Whether any toast is still within its TTL.
    #[must_use]
    pub(crate) fn has_active(&self, now: u64) -> bool {
        self.items
            .iter()
            .any(|t| now.saturating_sub(t.born_tick) < TOAST_TTL_TICKS)
    }

    /// Remove all expired toasts. Called once per draw frame.
    pub(crate) fn prune(&mut self, now: u64) {
        self.items
            .retain(|t| now.saturating_sub(t.born_tick) < TOAST_TTL_TICKS);
    }

    /// Iterate active (non-expired) toasts.
    pub(crate) fn active_items(&self, now: u64) -> impl Iterator<Item = &Toast> {
        self.items
            .iter()
            .filter(move |t| now.saturating_sub(t.born_tick) < TOAST_TTL_TICKS)
    }
}

// ── SplashShimmer ────────────────────────────────────────────────────────────

/// One-shot shimmer state for the splash wordmark (#5104).
///
/// `start_tick` is set on the first draw frame where `show_splash` is `true`.
/// Cleared on the rising edge of `show_splash` (false → true) so each new
/// splash show gets a fresh sweep.
pub(crate) struct SplashShimmer {
    pub(crate) start_tick: Option<u64>,
}

impl SplashShimmer {
    pub(crate) fn new() -> Self {
        Self { start_tick: None }
    }

    /// Whether the shimmer is still sweeping.
    #[must_use]
    pub(crate) fn is_active(&self, now: u64) -> bool {
        self.start_tick
            .is_some_and(|s| now.saturating_sub(s) < SHIMMER_TICKS)
    }

    /// Phase `(0..SHIMMER_TICKS)` of the sweep, or `None` when inactive.
    #[must_use]
    pub(crate) fn phase(&self, now: u64) -> Option<u64> {
        let start = self.start_tick?;
        let elapsed = now.saturating_sub(start);
        if elapsed < SHIMMER_TICKS {
            Some(elapsed)
        } else {
            None
        }
    }

    /// Record the start tick on first splash frame.
    pub(crate) fn activate(&mut self, now: u64) {
        if self.start_tick.is_none() {
            self.start_tick = Some(now);
        }
    }

    /// Reset so the next splash show gets a fresh sweep.
    pub(crate) fn reset(&mut self) {
        self.start_tick = None;
    }
}

// ── Pure math helpers (snapshot-testable) ────────────────────────────────────

/// Format tok/s for the status bar (e.g. `"42 tok/s"`, `"1.2k tok/s"`).
#[must_use]
pub(crate) fn format_toks(rate: f32) -> String {
    if rate >= 1000.0 {
        format!("{:.1}k tok/s", rate / 1000.0)
    } else {
        format!("{rate:.0} tok/s")
    }
}

/// Format TTFT milliseconds for the status bar (e.g. `"TTFT 234ms"`, `"TTFT 1.2s"`).
#[must_use]
pub(crate) fn format_ttft(ms: u64) -> String {
    if ms >= 1000 {
        #[allow(clippy::cast_precision_loss)]
        let secs = ms as f32 / 1000.0;
        format!("TTFT {secs:.1}s")
    } else {
        format!("TTFT {ms}ms")
    }
}

/// Brightness boost multiplier for the shimmer highlight at letter index `i`.
///
/// Returns a value in `[0.0, 1.0]` where 1.0 is peak brightness.
/// The sweep front passes each letter at `phase * letters_per_tick`.
#[must_use]
pub(crate) fn shimmer_brightness(letter_idx: usize, n_letters: usize, phase: u64) -> f32 {
    if n_letters == 0 {
        return 0.0;
    }
    // Map phase [0..SHIMMER_TICKS] to a sweep position across all letters.
    #[allow(clippy::cast_precision_loss)]
    let sweep_pos = phase as f32 / SHIMMER_TICKS as f32 * (n_letters + 2) as f32 - 1.0;
    #[allow(clippy::cast_precision_loss)]
    let dist = (letter_idx as f32 - sweep_pos).abs();
    // Bell curve: peak at 0 distance, falls to ~0 at distance ≥ 1.5 letters.
    let brightness = (-dist * dist * 2.0).exp();
    brightness.clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_toks_below_1k() {
        assert_eq!(format_toks(42.0), "42 tok/s");
    }

    #[test]
    fn format_toks_above_1k() {
        let s = format_toks(1200.0);
        assert!(s.contains("tok/s"), "got: {s}");
        assert!(s.starts_with("1.2k"), "got: {s}");
    }

    #[test]
    fn format_ttft_ms() {
        assert_eq!(format_ttft(234), "TTFT 234ms");
    }

    #[test]
    fn format_ttft_seconds() {
        let s = format_ttft(1200);
        assert!(s.starts_with("TTFT 1.2s"), "got: {s}");
    }

    #[test]
    fn shimmer_peak_at_sweep_front() {
        // Phase 6 out of 12 ticks → sweep at mid-point (letter 2 of 4).
        let brightness = shimmer_brightness(2, 4, 6);
        assert!(
            brightness > 0.5,
            "brightness at sweep front must be > 0.5, got {brightness}"
        );
    }

    #[test]
    fn shimmer_zero_when_far_from_front() {
        // Phase 0 → sweep front at beginning; letter 3 (far away) should be dim.
        let brightness = shimmer_brightness(3, 4, 0);
        assert!(
            brightness < 0.2,
            "brightness far from front must be < 0.2, got {brightness}"
        );
    }

    #[test]
    fn toast_queue_caps_at_3() {
        let mut q = ToastQueue::new();
        for i in 0..5u64 {
            q.push(format!("t{i}"), ToastKind::Info, i);
        }
        assert_eq!(q.items.len(), 3);
        // Oldest (0,1) dropped; remaining are 2,3,4.
        assert_eq!(q.items[0].born_tick, 2);
    }

    #[test]
    fn toast_prune_removes_expired() {
        let mut q = ToastQueue::new();
        q.push("old", ToastKind::Info, 0);
        q.push("new", ToastKind::Info, 50);
        q.prune(TOAST_TTL_TICKS + 5); // old expired; new still alive
        assert_eq!(q.items.len(), 1);
        assert_eq!(q.items[0].born_tick, 50);
    }

    #[test]
    fn splash_shimmer_activate_and_phase() {
        let mut s = SplashShimmer::new();
        assert!(!s.is_active(5));
        s.activate(10);
        assert!(s.is_active(10));
        assert_eq!(s.phase(10), Some(0));
        assert_eq!(s.phase(15), Some(5));
        assert!(!s.is_active(10 + SHIMMER_TICKS));
    }

    #[test]
    fn splash_shimmer_reset() {
        let mut s = SplashShimmer::new();
        s.activate(10);
        s.reset();
        assert!(!s.is_active(10));
    }

    #[test]
    fn scroll_anim_ease_out() {
        use crate::session::ScrollAnim;
        let anim = ScrollAnim {
            from: 0,
            to: 100,
            start_tick: 0,
        };
        let (mid, done) = anim.current_offset(1);
        assert!(!done);
        // After 1 of 3 ticks ease-out-cubic gives ≈1-(2/3)^3 ≈ 0.704 → ~70 rows.
        assert!(
            mid > 50,
            "ease-out should be past midpoint at t=1/3, got {mid}"
        );
    }
}
