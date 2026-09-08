// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Shared `ToolUse`/`ToolResult` pairing and repair, used by every history-mutating call site
//! and by the Claude request builder's per-request hot path (issue #6771).
//!
//! # Adjacency, not global id sets
//!
//! A pair is valid only when the `ToolResult` is in the message **immediately following** (across
//! `Role::System` messages, which are skipped) the `ToolUse`'s message, and vice versa. Matching
//! against a global id set across the whole history is wrong: some providers (e.g. Ollama, which
//! mints ids as `format!("call_{i}")` by batch index — `crates/zeph-llm/src/ollama.rs`) legitimately
//! reuse the same id across unrelated turns, so a global set cross-pairs an orphan left by a trim
//! or restore boundary against an unrelated call that happens to share its id (issue #6770). Every
//! function in this module is adjacency-scoped for this reason.
//!
//! # Two layers
//!
//! - **Layer A** — [`unmatched_tool_use_ids`] / [`unmatched_tool_result_ids`]: pure classification,
//!   the single definition of "orphaned" in this codebase. They take already-resolved neighbour
//!   messages, so they work whether the caller holds `Vec<Message>` (history-mutating call sites)
//!   or `Vec<&Message>` (the Claude request builder's per-request hot path, which lowers parts into
//!   wire blocks without cloning history).
//! - **Layer B** — [`repair_tool_pairs`] / [`repair_window`]: mutating repair, built strictly on top
//!   of Layer A. Layer B never re-implements the pairing predicate — it calls Layer A, collects the
//!   returned ids into an owned `HashSet<String>`, then mutates. This is visible directly in the
//!   source of [`repair_tool_pairs`] below: every orphan decision is a call to
//!   [`unmatched_tool_use_ids`] or [`unmatched_tool_result_ids`], and neither of those functions
//!   themselves has a caller inside Layer B other than each other's opposite pass. A reviewer
//!   verifies this by reading the two pass loops, not by grepping for a pattern — a
//!   `MessagePart::ToolUse`/`MessagePart::ToolResult` match alone does not distinguish correct
//!   Layer B code (which must match those variants to mutate `parts`) from a duplicated predicate.
//!
//! # Orphaned `ToolUse` is always deleted
//!
//! Unlike an orphaned `ToolResult` (which already carries content and can be downgraded to a
//! non-empty `Text` part, see [`OrphanAction::DowngradeToText`]), an orphaned `ToolUse` has no safe
//! text form worth preserving as conversation content — every call site already deletes it
//! unconditionally, so [`repair_tool_pairs`] does the same regardless of `action`.
//!
//! # Divergent markers (defensive, not currently exercised)
//!
//! [`has_meaningful_content`] recognizes the legacy `[tool_use: `/`[tool_result: `/`[tool output: `
//! markers produced by `Message::flatten_parts` (underscore, no space before the colon).
//! [`OrphanAction::DowngradeToText`]'s marker is deliberately `[tool result: {id}] {content}`
//! (space, no underscore) so a downgraded `ToolResult` always reads as "meaningful" and is never
//! mistaken for empty, marker-only content — an orphan downgraded specifically to survive as a
//! window's leading `user` anchor must not then be treated as removable. **If a future caller ever
//! passes `DowngradeToText` to a site whose surviving text is later checked with
//! [`has_meaningful_content`], the two markers must stay divergent for that invariant to hold.**
//! No current caller does this: the only [`has_meaningful_content`] consumer
//! (`zeph_agent_persistence::sanitize::sanitize_tool_pairs`) uses [`OrphanAction::Delete`], so the
//! downgrade marker is never actually evaluated against it today.
//!
//! # Content resync is exact, not a per-caller policy (issue #6771, R1)
//!
//! Both [`repair_tool_pairs`] and [`repair_window`] mutate a message's `content` after stripping
//! or downgrading a part **only when that message's `content` was already an exact
//! `flatten_parts` of its (pre-mutation) `parts`** — captured immediately before the mutation, per
//! message. A DB-restored message can hold independently-authored text `flatten_parts` cannot
//! reconstruct (e.g. `"Let me list the directory. [tool_use: shell(x)]"` with only a `ToolUse`
//! part); rebuilding such a message's `content` would silently destroy that text. An earlier
//! revision instead rebuilt unconditionally in [`repair_window`] on the (false) assumption that
//! every caller of that function builds messages via `Message::from_parts` — `trim_parent_messages`
//! (`zeph-core`) actually clones DB-restored parent messages, so this fired on every subagent
//! spawn under `ParentContextPolicy::Inherit`/`InheritSanitized`, not just on genuine boundary
//! damage. The per-message, pre-mutation check is exact regardless of caller: a `from_parts`-built
//! message is always `content == flatten_parts(parts)`, so nothing that needed resyncing before is
//! now missed, and nothing with divergent content is ever touched.
//!
//! One refinement on top of the exact check: a mutated message is also resynced when its stale
//! `content` had no [`has_meaningful_content`] text (empty or whitespace-only), even though it
//! wasn't flatten-derived. Without this, [`OrphanAction::DowngradeToText`]'s non-empty-anchor
//! guarantee (the #6762 N5 fix) could be silently defeated: the orphan survives as a `Text` part
//! in `parts`, but a stale empty/whitespace `content` is what request builders that read
//! `to_llm_content()` directly actually see. This is still lossless with respect to the rule
//! above — a stale content with nothing meaningful to preserve is, by definition, safe to
//! overwrite in either `OrphanAction` arm.
//!
//! # Pass ordering is closed under both `OrphanAction` values
//!
//! [`repair_tool_pairs`] runs pass 1 (repair orphaned `ToolResult`s in `User` messages) fully
//! before pass 2 (strip orphaned `ToolUse`s from `Assistant` messages) — a single ordered sweep,
//! not a fixed-point loop. This is sufficient: a `ToolResult` that pass 1 validates as matched
//! against assistant `A` implies `A` holds the matching `ToolUse`, which pass 2 therefore always
//! keeps — pass 2 can never remove the anchor pass 1 relied on. Iteration is required only for
//! [`repair_window`]'s leading-message-drop cascade: dropping a leading `Assistant` message can
//! newly orphan a `ToolResult` that `repair_tool_pairs` correctly kept intact (the pair was
//! well-formed before the drop), so `repair_window` loops `repair_tool_pairs` and the leading-drop
//! together to a joint fixed point. Termination is guaranteed because every non-final iteration
//! strictly decreases `(#ToolUse parts + #ToolResult parts) + (#messages)`: a delete removes a
//! part, a downgrade converts a `ToolResult` into a `Text` part (never re-examined by either pass),
//! and a leading-drop removes a message — no combination can increase the measure, so no ping-pong.

use std::collections::HashSet;

use crate::provider::{Message, MessagePart, Role};

/// Collect `tool_use` ids from `msg` that have no matching `ToolResult` in `next`.
///
/// `next` must resolve to the message immediately following `msg` in the caller's window,
/// skipping any `Role::System` messages — this function does not walk `Vec<Message>` itself, so
/// it works identically whether the caller holds owned messages or borrowed references (the
/// Claude request builder's hot path holds `Vec<&Message>`). A match requires `next.role ==
/// Role::User`; any other role (including `None`, e.g. `msg` is the last message in the window)
/// means every `ToolUse` id in `msg` is unmatched.
///
/// `msg`'s own role is not checked — the caller decides whether `msg` is eligible (only an
/// `Assistant` message can meaningfully carry an unmatched `ToolUse`).
///
/// # Examples
///
/// ```
/// use zeph_llm::provider::{Message, MessagePart, Role};
/// use zeph_llm::tool_pairing::unmatched_tool_use_ids;
///
/// let asst = Message::from_parts(Role::Assistant, vec![MessagePart::ToolUse {
///     id: "t1".into(),
///     name: "bash".into(),
///     input: serde_json::json!({}),
/// }]);
///
/// // No next message at all: the ToolUse is unmatched.
/// assert_eq!(unmatched_tool_use_ids(&asst, None), ["t1"].into_iter().collect());
///
/// // A following User message with the matching ToolResult clears it.
/// let user = Message::from_parts(Role::User, vec![MessagePart::ToolResult {
///     tool_use_id: "t1".into(),
///     content: "ok".into(),
///     is_error: false,
/// }]);
/// assert!(unmatched_tool_use_ids(&asst, Some(&user)).is_empty());
/// ```
#[must_use]
pub fn unmatched_tool_use_ids<'a>(msg: &'a Message, next: Option<&Message>) -> HashSet<&'a str> {
    let matched: HashSet<&str> = next.filter(|n| n.role == Role::User).map_or_default(|n| {
        n.parts
            .iter()
            .filter_map(|p| match p {
                MessagePart::ToolResult { tool_use_id, .. } => Some(tool_use_id.as_str()),
                _ => None,
            })
            .collect()
    });

    msg.parts
        .iter()
        .filter_map(|p| match p {
            MessagePart::ToolUse { id, .. } if !matched.contains(id.as_str()) => Some(id.as_str()),
            _ => None,
        })
        .collect()
}

/// Collect `tool_result` ids from `msg` that have no matching `ToolUse` in `prev`.
///
/// `prev` must resolve to the message immediately preceding `msg`, skipping `Role::System`
/// messages. A match requires `prev.role == Role::Assistant`; any other role (including `None`,
/// e.g. `msg` is the first message in the window) means every `ToolResult` id in `msg` is
/// unmatched.
///
/// `msg`'s own role is not checked — the caller decides whether `msg` is eligible.
///
/// # Examples
///
/// ```
/// use zeph_llm::provider::{Message, MessagePart, Role};
/// use zeph_llm::tool_pairing::unmatched_tool_result_ids;
///
/// let user = Message::from_parts(Role::User, vec![MessagePart::ToolResult {
///     tool_use_id: "t1".into(),
///     content: "ok".into(),
///     is_error: false,
/// }]);
///
/// // No preceding message: the ToolResult is unmatched.
/// assert_eq!(unmatched_tool_result_ids(&user, None), ["t1"].into_iter().collect());
///
/// // A preceding Assistant message with the matching ToolUse clears it.
/// let asst = Message::from_parts(Role::Assistant, vec![MessagePart::ToolUse {
///     id: "t1".into(),
///     name: "bash".into(),
///     input: serde_json::json!({}),
/// }]);
/// assert!(unmatched_tool_result_ids(&user, Some(&asst)).is_empty());
/// ```
#[must_use]
pub fn unmatched_tool_result_ids<'a>(msg: &'a Message, prev: Option<&Message>) -> HashSet<&'a str> {
    let available: HashSet<&str> = prev
        .filter(|p| p.role == Role::Assistant)
        .map_or_default(|p| {
            p.parts
                .iter()
                .filter_map(|part| match part {
                    MessagePart::ToolUse { id, .. } => Some(id.as_str()),
                    _ => None,
                })
                .collect()
        });

    msg.parts
        .iter()
        .filter_map(|p| match p {
            MessagePart::ToolResult { tool_use_id, .. }
                if !available.contains(tool_use_id.as_str()) =>
            {
                Some(tool_use_id.as_str())
            }
            _ => None,
        })
        .collect()
}

/// How [`repair_tool_pairs`] and [`repair_window`] handle an orphaned `ToolResult` part.
///
/// Only `ToolResult` orphans are configurable — orphaned `ToolUse` parts are always deleted (see
/// the module docs). Choose `Delete` for terminal, DB-restored, or otherwise final snapshots where
/// the orphan carries no further use; choose `DowngradeToText` for a live in-progress window where
/// the orphaned `ToolResult` may be the only message available to anchor the window on a `user`
/// role (deleting it there can cascade into removing the window's only conversational content).
///
/// # Examples
///
/// ```
/// use zeph_llm::tool_pairing::OrphanAction;
///
/// let terminal_snapshot = OrphanAction::Delete;
/// let live_window = OrphanAction::DowngradeToText;
///
/// let describes_deletion = matches!(terminal_snapshot, OrphanAction::Delete);
/// assert!(describes_deletion);
/// assert_ne!(terminal_snapshot, live_window);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrphanAction {
    /// Remove the orphaned `ToolResult` part entirely.
    Delete,
    /// Replace the orphaned `ToolResult` part with a `MessagePart::Text` carrying the same
    /// content, marked `[tool result: {id}] {content}` — guaranteed non-empty even when
    /// `content` is empty, so the message survives as a valid, non-empty anchor.
    DowngradeToText,
}

/// Outcome of a [`repair_tool_pairs`] or [`repair_window`] call.
///
/// # Examples
///
/// ```
/// use zeph_llm::provider::{Message, MessagePart, Role};
/// use zeph_llm::tool_pairing::{repair_tool_pairs, OrphanAction};
///
/// let mut messages = vec![Message::from_parts(Role::Assistant, vec![MessagePart::ToolUse {
///     id: "t1".into(),
///     name: "bash".into(),
///     input: serde_json::json!({}),
/// }])];
/// let report = repair_tool_pairs(&mut messages, OrphanAction::Delete);
///
/// assert_eq!(report.parts_repaired, 1, "the orphaned ToolUse was stripped");
/// assert_eq!(report.removed_messages.len(), 1, "the now-empty message was removed");
/// ```
pub struct RepairReport {
    /// Number of `ToolUse`/`ToolResult` parts deleted or downgraded.
    pub parts_repaired: usize,
    /// Messages removed entirely because repair left them with no parts and no content.
    /// Moved (not cloned) so a caller can recover `metadata.db_id` for soft-delete without a
    /// separate lookup pass.
    pub removed_messages: Vec<Message>,
}

/// One ordered sweep repairing orphaned `ToolUse`/`ToolResult` parts in `messages`.
///
/// Pass 1 repairs orphaned `ToolResult` parts in `User` messages per `action` (adjacency against
/// the immediately preceding non-system message). Pass 2 then strips orphaned `ToolUse` parts from
/// `Assistant` messages (adjacency against the immediately following non-system message, which by
/// this point reflects pass 1's mutations) — always deleted, regardless of `action`. A message
/// left with empty `parts` and no [`has_meaningful_content`] text after either pass is removed,
/// **except** `Role::System` messages, which are never removed. `content` itself is never
/// mutated by this function (see the inline notes on why). See the module docs for why one
/// ordered sweep (not a fixed-point loop) is sufficient here.
///
/// # Examples
///
/// ```
/// use zeph_llm::provider::{Message, MessagePart, Role};
/// use zeph_llm::tool_pairing::{repair_tool_pairs, OrphanAction};
///
/// // A trailing, unanswered ToolUse is orphaned and stripped, regardless of position.
/// let mut messages = vec![
///     Message::from_legacy(Role::System, "sys"),
///     Message::from_parts(Role::Assistant, vec![MessagePart::ToolUse {
///         id: "t1".into(),
///         name: "bash".into(),
///         input: serde_json::json!({}),
///     }]),
/// ];
/// let report = repair_tool_pairs(&mut messages, OrphanAction::Delete);
/// assert_eq!(report.parts_repaired, 1);
/// // The now-empty assistant message was removed; only the system message survives.
/// assert_eq!(messages.len(), 1);
/// assert_eq!(messages[0].role, Role::System);
/// ```
#[must_use]
pub fn repair_tool_pairs(messages: &mut Vec<Message>, action: OrphanAction) -> RepairReport {
    let mut parts_repaired = 0usize;

    // Pass 1: repair orphaned ToolResult parts in User messages.
    for i in 0..messages.len() {
        if messages[i].role != Role::User || messages[i].parts.is_empty() {
            continue;
        }
        let orphaned: HashSet<String> = {
            let prev = (0..i)
                .rev()
                .find(|&j| messages[j].role != Role::System)
                .map(|j| &messages[j]);
            unmatched_tool_result_ids(&messages[i], prev)
                .into_iter()
                .map(str::to_owned)
                .collect()
        };
        if orphaned.is_empty() {
            continue;
        }

        // Captured BEFORE mutating `parts`: only a message whose `content` was already an exact
        // flatten of its (pre-repair) `parts` is safe to re-flatten afterward. A DB-restored
        // message can hold independently-authored text beyond what `flatten_parts` produces —
        // rebuilding unconditionally would silently clobber it (R1 regression, issue #6771). This
        // is exact per message, not a per-caller policy: a from_parts-built message is always
        // `was_derived`, so this never under-syncs the in-memory callers either.
        let was_derived = messages[i].content == Message::flatten_parts(&messages[i].parts);

        let mut changed = 0usize;
        match action {
            OrphanAction::Delete => {
                let before = messages[i].parts.len();
                messages[i].parts.retain(|p| {
                    !matches!(p, MessagePart::ToolResult { tool_use_id, .. } if orphaned.contains(tool_use_id.as_str()))
                });
                changed = before - messages[i].parts.len();
            }
            OrphanAction::DowngradeToText => {
                for part in &mut messages[i].parts {
                    if let MessagePart::ToolResult {
                        tool_use_id,
                        content,
                        ..
                    } = part
                        && orphaned.contains(tool_use_id.as_str())
                    {
                        let text = format!("[tool result: {tool_use_id}] {content}");
                        *part = MessagePart::Text { text };
                        changed += 1;
                    }
                }
            }
        }

        if changed > 0 {
            parts_repaired += changed;
            // E1 hardening: also rebuild when the stale `content` had nothing worth preserving
            // (empty/whitespace) — under `Delete` this is a no-op (parts are now empty, see
            // `Message::rebuild_content`'s own contract); under `DowngradeToText` it restores the
            // downgrade's non-empty-anchor guarantee (#6762 N5) instead of leaving a message
            // whose surviving `Text` part is invisible to `to_llm_content()` because `content`
            // was never synced. Lossless in both arms: a *meaningful* stale content is still
            // never touched, which is R1's entire point.
            if was_derived || !has_meaningful_content(&messages[i].content) {
                messages[i].rebuild_content();
            }
        }
    }

    // Pass 2: strip orphaned ToolUse parts from Assistant messages — always deleted.
    for i in 0..messages.len() {
        if messages[i].role != Role::Assistant || messages[i].parts.is_empty() {
            continue;
        }
        let orphaned: HashSet<String> = {
            let next = (i + 1..messages.len())
                .find(|&j| messages[j].role != Role::System)
                .map(|j| &messages[j]);
            unmatched_tool_use_ids(&messages[i], next)
                .into_iter()
                .map(str::to_owned)
                .collect()
        };
        if orphaned.is_empty() {
            continue;
        }

        // See the matching note in pass 1 — the same exact, per-message guard applies here.
        let was_derived = messages[i].content == Message::flatten_parts(&messages[i].parts);

        let before = messages[i].parts.len();
        messages[i].parts.retain(
            |p| !matches!(p, MessagePart::ToolUse { id, .. } if orphaned.contains(id.as_str())),
        );
        let changed = before - messages[i].parts.len();
        if changed > 0 {
            parts_repaired += changed;
            // See the matching note in pass 1 — same E1 hardening applies here (though pass 2
            // always deletes, so `rebuild_content` no-ops on the now-empty `parts` regardless).
            if was_derived || !has_meaningful_content(&messages[i].content) {
                messages[i].rebuild_content();
            }
        }
    }

    // Unconditional — not gated on `parts_repaired > 0`. A message that was already empty
    // (parts and meaningful content both absent) before this call is removed too, matching the
    // pre-#6771 per-site behavior (site 1's old unconditional `retain`, site 3's `service.rs`
    // pre-filter) rather than making removal a side effect of an unrelated repair elsewhere in
    // the same call. This also converges site 3's two callers: `service.rs` pre-filters on the
    // identical predicate before calling this function, `hydrate.rs` does not — with an
    // unconditional sweep both now get the same cleanup regardless of that difference.
    let mut removed_messages = Vec::new();
    let mut idx = 0;
    while idx < messages.len() {
        // `has_meaningful_content` (not a plain `content.is_empty()` check) because a message
        // that was not `was_derived` above never got its `content` rebuilt from the (now empty)
        // `parts` — it can still carry independently-authored text after its last structured
        // part is gone.
        let empty = messages[idx].role != Role::System
            && messages[idx].parts.is_empty()
            && !has_meaningful_content(&messages[idx].content);
        if empty {
            removed_messages.push(messages.remove(idx));
        } else {
            idx += 1;
        }
    }

    RepairReport {
        parts_repaired,
        removed_messages,
    }
}

/// Drops the first non-system message in `messages` if its role is not `Role::User`.
fn drop_leading_non_user_message(messages: &mut Vec<Message>) -> Option<Message> {
    let idx = messages.iter().position(|m| m.role != Role::System)?;
    if messages[idx].role == Role::User {
        return None;
    }
    Some(messages.remove(idx))
}

/// [`repair_tool_pairs`] plus a leading-role invariant: the first non-system message in
/// `messages` must have `Role::User` (required by the Anthropic API and not enforced by
/// `repair_tool_pairs` alone). The two are looped to a joint fixed point because dropping a
/// leading non-`user` message can newly orphan a pair that `repair_tool_pairs` had correctly
/// kept intact — see the module docs for the counter-example and termination argument.
///
/// `content` handling is identical to `repair_tool_pairs` (each mutated message is resynced only
/// if its `content` was already an exact flatten of its parts before that mutation — see
/// `repair_tool_pairs`'s doc comment) — `repair_window`'s extra leading-drop pass introduces no
/// separate content-handling rule of its own.
///
/// # Examples
///
/// ```
/// use zeph_llm::provider::{Message, MessagePart, Role};
/// use zeph_llm::tool_pairing::{repair_window, OrphanAction};
///
/// // A well-formed pair leads the window — repair_tool_pairs alone would keep it, but the
/// // leading-role invariant requires a leading Assistant message to be dropped, which then
/// // orphans the ToolResult it left behind. repair_window catches this second-order effect.
/// let mut messages = vec![
///     Message::from_parts(Role::Assistant, vec![MessagePart::ToolUse {
///         id: "t1".into(),
///         name: "bash".into(),
///         input: serde_json::json!({}),
///     }]),
///     Message::from_parts(Role::User, vec![MessagePart::ToolResult {
///         tool_use_id: "t1".into(),
///         content: "ok".into(),
///         is_error: false,
///     }]),
/// ];
/// repair_window(&mut messages, OrphanAction::Delete);
/// assert!(messages.is_empty(), "the leading-drop cascade must remove the whole window");
/// ```
#[must_use]
pub fn repair_window(messages: &mut Vec<Message>, action: OrphanAction) -> RepairReport {
    let mut parts_repaired = 0usize;
    let mut removed_messages = Vec::new();

    loop {
        let report = repair_tool_pairs(messages, action);
        parts_repaired += report.parts_repaired;
        removed_messages.extend(report.removed_messages);

        let dropped = drop_leading_non_user_message(messages);
        let dropped_any = dropped.is_some();
        removed_messages.extend(dropped);

        if report.parts_repaired == 0 && !dropped_any {
            break;
        }
    }

    RepairReport {
        parts_repaired,
        removed_messages,
    }
}

/// Returns `true` if `content` contains human-readable text beyond legacy tool bracket markers.
///
/// Moved from `zeph_agent_persistence::sanitize` (issue #6771) — it decodes markers produced by
/// [`Message::flatten_parts`](crate::provider::Message), which lives in this crate. Legacy
/// markers are:
/// - `[tool_use: name(id)]` — assistant `ToolUse`
/// - `[tool_result: id]\nbody` — user `ToolResult`
/// - `[tool output: name] body` — `ToolOutput`
///
/// A message whose content consists solely of such markers (and whitespace) has no user-visible
/// text and is a candidate for soft-delete. See the module docs for why this deliberately does
/// *not* recognize [`OrphanAction::DowngradeToText`]'s `[tool result: ` (space) marker.
///
/// # Examples
///
/// ```
/// use zeph_llm::tool_pairing::has_meaningful_content;
///
/// assert!(has_meaningful_content("hello world"));
/// assert!(!has_meaningful_content("[tool_use: bash(abc123)]"));
/// assert!(!has_meaningful_content("   [tool_result: abc]\nsome output"));
/// ```
#[must_use]
pub fn has_meaningful_content(content: &str) -> bool {
    const PREFIXES: [&str; 3] = ["[tool_use: ", "[tool_result: ", "[tool output: "];

    let mut remaining = content.trim();

    loop {
        let next = PREFIXES
            .iter()
            .filter_map(|prefix| remaining.find(prefix).map(|pos| (pos, *prefix)))
            .min_by_key(|(pos, _)| *pos);

        let Some((start, prefix)) = next else {
            break;
        };

        if !remaining[..start].trim().is_empty() {
            return true;
        }

        let after_prefix = &remaining[start + prefix.len()..];
        let Some(close) = after_prefix.find(']') else {
            return true; // Malformed tag — treat as meaningful.
        };

        let tag_end = start + prefix.len() + close + 1;

        if prefix == "[tool_result: " || prefix == "[tool output: " {
            let body = remaining[tag_end..].trim_start_matches('\n');
            let next_tag = PREFIXES
                .iter()
                .filter_map(|p| body.find(p))
                .min()
                .unwrap_or(body.len());
            remaining = &body[next_tag..];
        } else {
            remaining = &remaining[tag_end..];
        }
    }

    !remaining.trim().is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_use(id: &str) -> MessagePart {
        MessagePart::ToolUse {
            id: id.to_owned(),
            name: "bash".to_owned(),
            input: serde_json::json!({}),
        }
    }

    fn tool_result(id: &str, content: &str) -> MessagePart {
        MessagePart::ToolResult {
            tool_use_id: id.to_owned(),
            content: content.to_owned(),
            is_error: false,
        }
    }

    fn has_tool_use(m: &Message, id: &str) -> bool {
        m.parts
            .iter()
            .any(|p| matches!(p, MessagePart::ToolUse { id: tid, .. } if tid == id))
    }

    fn has_tool_result(m: &Message, id: &str) -> bool {
        m.parts
            .iter()
            .any(|p| matches!(p, MessagePart::ToolResult { tool_use_id, .. } if tool_use_id == id))
    }

    #[test]
    fn adjacency_scoping_ignores_id_reuse_across_unrelated_turns() {
        // #6770: a global id set would wrongly pair a leading orphan against an unrelated,
        // later call sharing its id (Ollama-style `call_0` reuse).
        let mut messages = vec![
            Message::from_parts(Role::User, vec![tool_result("call_0", "orphaned")]),
            Message::from_legacy(Role::Assistant, "unrelated text turn"),
            Message::from_parts(Role::Assistant, vec![tool_use("call_0")]),
            Message::from_parts(Role::User, vec![tool_result("call_0", "turn-2 output")]),
        ];
        let report = repair_tool_pairs(&mut messages, OrphanAction::Delete);
        assert_eq!(report.parts_repaired, 1);
        assert_eq!(report.removed_messages.len(), 1);
        assert!(messages.iter().any(|m| has_tool_use(m, "call_0")));
        let remaining_results: Vec<&str> = messages
            .iter()
            .flat_map(|m| m.parts.iter())
            .filter_map(|p| match p {
                MessagePart::ToolResult { content, .. } => Some(content.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(remaining_results, vec!["turn-2 output"]);
    }

    #[test]
    fn trailing_orphaned_tool_use_is_repaired_not_exempted() {
        // S1: TrailingToolUse::Exempt was deleted — Repair is universal, including the trailing
        // message of the window.
        let mut messages = vec![Message::from_parts(Role::Assistant, vec![tool_use("t1")])];
        let report = repair_tool_pairs(&mut messages, OrphanAction::Delete);
        assert_eq!(report.parts_repaired, 1);
        assert!(messages.is_empty());
    }

    #[test]
    fn intact_pair_survives_a_single_sweep() {
        // S2: pass 1 keeps a validated ToolResult, so pass 2 (reading the same, unmutated
        // message) never removes the ToolUse anchor it relied on.
        let mut messages = vec![
            Message::from_parts(Role::Assistant, vec![tool_use("t1")]),
            Message::from_parts(Role::User, vec![tool_result("t1", "ok")]),
        ];
        let report = repair_tool_pairs(&mut messages, OrphanAction::Delete);
        assert_eq!(report.parts_repaired, 0);
        assert!(has_tool_use(&messages[0], "t1"));
        assert!(has_tool_result(&messages[1], "t1"));
    }

    #[test]
    fn downgrade_produces_non_empty_marker_even_for_empty_content() {
        let mut messages = vec![
            Message::from_legacy(Role::System, "sys"),
            Message::from_parts(Role::User, vec![tool_result("t1", "")]),
        ];
        let report = repair_tool_pairs(&mut messages, OrphanAction::DowngradeToText);
        assert_eq!(report.parts_repaired, 1);
        assert_eq!(
            messages.len(),
            2,
            "the downgraded message must survive, not be removed"
        );
        let text = messages[1]
            .parts
            .iter()
            .find_map(|p| match p {
                MessagePart::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .expect("orphan must survive downgraded to Text");
        assert!(!text.trim().is_empty());
    }

    #[test]
    fn repair_window_cascades_leading_drop_into_a_newly_orphaned_pair() {
        // S2's counter-example: repair_tool_pairs alone keeps this pair intact, but the
        // leading-role invariant drops the leading Assistant message, which then orphans the
        // ToolResult it left behind — requiring a second pairing pass.
        let mut messages = vec![
            Message::from_parts(Role::Assistant, vec![tool_use("t1")]),
            Message::from_parts(Role::User, vec![tool_result("t1", "ok")]),
        ];
        let report = repair_window(&mut messages, OrphanAction::Delete);
        assert!(messages.is_empty());
        assert_eq!(report.removed_messages.len(), 2);
    }

    #[test]
    fn system_messages_are_never_removed() {
        let mut messages = vec![
            Message::from_legacy(Role::System, String::new()),
            Message::from_parts(Role::User, vec![tool_result("t1", "orphaned")]),
        ];
        let report = repair_tool_pairs(&mut messages, OrphanAction::Delete);
        assert_eq!(report.parts_repaired, 1);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role, Role::System);
    }

    #[test]
    fn adjacency_skips_over_an_intervening_system_message() {
        // The module docs promise adjacency is scoped to the immediately preceding/following
        // *non-system* message. A Role::System message spliced between a ToolUse and its
        // ToolResult (e.g. a focus checkpoint or utility hint pushed mid-turn, per the impl
        // critic's verified S6 trace) must not break the pairing.
        let mut messages = vec![
            Message::from_parts(Role::Assistant, vec![tool_use("t1")]),
            Message::from_legacy(Role::System, "focus checkpoint"),
            Message::from_parts(Role::User, vec![tool_result("t1", "ok")]),
        ];
        let report = repair_tool_pairs(&mut messages, OrphanAction::Delete);
        assert_eq!(
            report.parts_repaired, 0,
            "the pair is adjacent across the System message"
        );
        assert!(has_tool_use(&messages[0], "t1"));
        assert_eq!(messages[1].role, Role::System);
        assert!(has_tool_result(&messages[2], "t1"));
    }

    #[test]
    fn multiple_consecutive_orphaned_tool_use_ids_in_one_message_are_all_stripped() {
        let mut messages = vec![Message::from_parts(
            Role::Assistant,
            vec![tool_use("t1"), tool_use("t2"), tool_use("t3")],
        )];
        let report = repair_tool_pairs(&mut messages, OrphanAction::Delete);
        assert_eq!(report.parts_repaired, 3);
        assert!(messages.is_empty());
    }

    #[test]
    fn all_orphan_window_is_fully_repaired_in_one_sweep() {
        let mut messages = vec![
            Message::from_parts(Role::User, vec![tool_result("a", "orphan-a")]),
            Message::from_parts(Role::Assistant, vec![tool_use("b")]),
            Message::from_parts(Role::User, vec![tool_result("c", "orphan-c")]),
        ];
        let report = repair_tool_pairs(&mut messages, OrphanAction::Delete);
        assert_eq!(report.parts_repaired, 3);
        assert!(messages.is_empty());
    }

    #[test]
    fn repair_window_on_a_system_only_window_is_a_noop() {
        let mut messages = vec![Message::from_legacy(Role::System, "sys")];
        let report = repair_window(&mut messages, OrphanAction::Delete);
        assert_eq!(report.parts_repaired, 0);
        assert!(report.removed_messages.is_empty());
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role, Role::System);
    }

    #[test]
    fn repair_tool_pairs_never_touches_content_on_a_partial_strip() {
        // C2: a message that keeps some parts after a strip must have its `content` left
        // untouched when that `content` was not an exact flatten of its parts to begin with — a
        // DB-restored message can hold independently-authored text (`ThinkingBlock` alone
        // flattens to "") that `flatten_parts` of the surviving parts cannot reconstruct. The
        // `was_derived` check (captured before this strip) is false here, so no resync happens.
        let mut messages = vec![Message {
            role: Role::Assistant,
            content: "Independently-authored reasoning flatten_parts cannot reconstruct."
                .to_owned(),
            parts: vec![
                MessagePart::ThinkingBlock {
                    thinking: "internal reasoning".to_owned(),
                    signature: "sig".to_owned(),
                },
                tool_use("orphan"),
            ],
            metadata: crate::provider::MessageMetadata::default(),
        }];
        let content_before = messages[0].content.clone();
        let report = repair_tool_pairs(&mut messages, OrphanAction::Delete);
        assert_eq!(
            report.parts_repaired, 1,
            "the orphaned ToolUse must be stripped"
        );
        assert_eq!(messages.len(), 1, "ThinkingBlock keeps parts non-empty");
        assert!(!has_tool_use(&messages[0], "orphan"));
        assert_eq!(
            messages[0].content, content_before,
            "content must never be touched by repair_tool_pairs, even on a partial strip"
        );
    }

    #[test]
    fn repair_window_resyncs_content_when_it_was_flatten_derived() {
        // A from_parts-built message always has content == flatten_parts(parts), so the
        // `was_derived` check is true here and the resync proceeds normally.
        // A leading User anchor keeps the leading-role rule from also dropping the Assistant
        // message under test (out of scope here — that behavior is covered elsewhere).
        let mut messages = vec![
            Message::from_parts(
                Role::User,
                vec![MessagePart::Text {
                    text: "please investigate".to_owned(),
                }],
            ),
            Message::from_parts(
                Role::Assistant,
                vec![
                    MessagePart::Text {
                        text: "thinking out loud".to_owned(),
                    },
                    tool_use("orphan"),
                ],
            ),
        ];
        let report = repair_window(&mut messages, OrphanAction::Delete);
        assert_eq!(report.parts_repaired, 1);
        assert_eq!(messages.len(), 2);
        assert!(
            messages[1].content.contains("thinking out loud"),
            "repair_window must rebuild content to reflect the surviving Text part"
        );
        assert!(!messages[1].content.contains("tool_use"));
    }

    #[test]
    fn repair_window_never_touches_divergent_content_when_nothing_needs_repair() {
        // R1 regression (issue #6771): a DB-restored message can hold content richer than
        // flatten_parts(parts) even when its ToolUse/ToolResult pair is fully matched and
        // nothing needs repairing anywhere in the window. An earlier revision's unconditional
        // bulk `for m in messages { m.rebuild_content() }` pass fired on every message with
        // non-empty parts regardless of whether it was touched, silently destroying this exact
        // case on every subagent spawn under ParentContextPolicy::Inherit. A leading User anchor
        // keeps the leading-role rule from dropping the Assistant message under test.
        let mut messages = vec![
            Message::from_parts(
                Role::User,
                vec![MessagePart::Text {
                    text: "please investigate".to_owned(),
                }],
            ),
            Message {
                role: Role::Assistant,
                content: "Let me list the directory. [tool_use: shell(x)]".to_owned(),
                parts: vec![tool_use("x")],
                metadata: crate::provider::MessageMetadata::default(),
            },
            Message::from_parts(Role::User, vec![tool_result("x", "file1.rs")]),
        ];
        let content_before = messages[1].content.clone();
        let report = repair_window(&mut messages, OrphanAction::Delete);
        assert_eq!(
            report.parts_repaired, 0,
            "the pair is fully matched, nothing to repair"
        );
        assert_eq!(messages.len(), 3);
        assert_eq!(
            messages[1].content, content_before,
            "content must survive untouched when nothing needed repair"
        );
    }

    #[test]
    fn e1_downgrade_still_restores_the_anchor_when_stale_content_was_not_meaningful() {
        // E1 (impl-critic, non-blocking hardening): a message whose `content` is empty or
        // whitespace-only and NOT flatten-derived (`was_derived == false`) must still get
        // `content` resynced after a DowngradeToText repair — otherwise the downgrade's own
        // non-empty-anchor guarantee (#6762 N5) is defeated at request-build time, since
        // `to_llm_content()` reads `content` directly and a whitespace-only `content` is dropped
        // by non-structured request-building branches. Rebuilding here is lossless: the stale
        // content had nothing meaningful to preserve, so overwriting it with the flattened
        // downgrade marker restores the anchor without reopening R1 (a *meaningful* stale
        // content, R1's whole concern, is never touched by this path).
        let mut messages = vec![Message {
            role: Role::User,
            content: "   ".to_owned(),
            parts: vec![tool_result("orphan", "something")],
            metadata: crate::provider::MessageMetadata::default(),
        }];
        let report = repair_tool_pairs(&mut messages, OrphanAction::DowngradeToText);
        assert_eq!(report.parts_repaired, 1);
        assert_eq!(
            messages.len(),
            1,
            "the downgraded message must survive as the anchor"
        );
        let text_part = messages[0]
            .parts
            .iter()
            .find_map(|p| match p {
                MessagePart::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .expect("orphan must survive downgraded to Text");
        assert!(!text_part.trim().is_empty());
        assert!(
            messages[0].content.contains(text_part),
            "content must be resynced to the downgraded anchor text, not left as stale whitespace"
        );
    }
}
