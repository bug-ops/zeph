// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Human-voice verb phrases for TUI status strings.
//!
//! Maps raw `send_status` / `tui_status!` strings to short, present-tense verb phrases
//! suitable for display next to the breeze spinner.
//!
//! # Voice rules
//! - Lowercase, present-tense gerund
//! - Maximum 3 words in `verb` + `detail` combined
//! - Pass-through verbatim when no fragment matches (graceful fallback)

/// A short, human-voice description of an ongoing operation.
#[derive(Debug, PartialEq, Eq)]
pub struct VerbPhrase {
    /// Primary verb (e.g. `"compacting"`). Always non-empty after [`humanize`].
    pub verb: String,
    /// Optional detail suffix shown muted after `·` (e.g. `"context"`). May be empty.
    pub detail: String,
}

/// Ordered fragment → (verb, detail) table built from the real `send_status` literal universe.
///
/// Matching is case-insensitive and uses `str::contains`. Earlier entries win.
/// More-specific fragments must appear before any bare sub-fragment they contain.
static FRAGMENTS: &[(&str, &str, &str)] = &[
    // Context compaction / compression
    ("compact", "compacting", "context"),
    ("compress", "compressing", "context"),
    // Summarization
    ("summari", "summarizing", ""),
    // Memory / recall — compound entries before bare "memory" / "recall"
    ("recalling", "recalling", ""),
    ("loading memory", "loading", "memory"),
    ("memory store", "connecting", ""),
    ("memory", "searching", ""),
    ("recall", "searching", ""),
    // Skills — compound entries before bare "skill" / "index"
    ("matching skill", "matching", "skills"),
    ("syncing skill", "syncing", "skills"),
    ("rebuilding search", "rebuilding", "index"),
    ("reloading skill", "reloading", "skills"),
    ("loading skill", "loading", "skills"),
    ("skill", "loading", "skills"),
    // Indexing — after "rebuilding search index" above
    ("index", "indexing", ""),
    // MCP — before bare "connecting" to give a more-specific label
    ("mcp", "connecting", ""),
    // Hooks / filesystem events
    ("working directory", "updating", "directory"),
    ("file-change hook", "running", "hook"),
    // Connecting (tools, servers — after more-specific "mcp" entry)
    ("connecting tool", "connecting", "tools"),
    ("connecting", "connecting", ""),
    // Executing / running scheduled tasks
    ("executing task", "executing", "task"),
    ("running", "running", ""),
    // Tools / shell — bare "tool" after more-specific "connecting tool" entry
    ("selecting tool", "selecting", "tools"),
    ("filtering tool", "filtering", "tools"),
    ("tool", "running", ""),
    ("shell", "running", ""),
    // Generic working indicator (7+ real sites: "working", "working...", "working…")
    ("working", "working", ""),
    // Reflection / learning
    ("reflecting", "reflecting", ""),
    // Analysis
    ("analyzing", "analyzing", ""),
    ("evaluating", "evaluating", ""),
    // Thinking / planning
    ("thinking", "thinking", ""),
    ("planning", "planning", ""),
    ("canceling", "canceling", ""),
    // Session / history
    ("session digest", "generating", "digest"),
    ("generating recap", "generating", "recap"),
    ("generating session", "generating", "digest"),
    ("saving session", "saving", ""),
    ("loading conversation", "loading", "history"),
    // Shutdown
    ("shutting down", "shutting down", ""),
    // Generic bare verbs — after all more-specific compound entries
    ("waiting", "waiting", ""),
    ("loading", "loading", ""),
    // Retrying (matched after tool-name extraction)
    ("retry", "retrying", ""),
    // Utility actions (matched after tool-name extraction)
    ("respond", "responding", ""),
    ("retrieve", "retrieving", ""),
    ("verify", "verifying", ""),
    ("stop", "stopping", ""),
];

/// Convert a raw `send_status` string into a [`VerbPhrase`].
///
/// # Algorithm
/// 1. Normalize: lowercase + strip trailing `.` / `…` / whitespace.
/// 2. Extract tool name from known format patterns:
///    - `"retrying {name}..."` → match `retry` table entry, use `name` as detail
///    - `"utility action: {verb} ({name})"` → extract `name`, match `name` in table
/// 3. Match against the ordered `FRAGMENTS` table (`str::contains`).
/// 4. Pass through verbatim if nothing matches.
///
/// Supervisor labels such as `"mem-extract +2 more"` contain no matching fragments
/// and are returned verbatim via the pass-through path.
#[must_use]
pub fn humanize(raw: &str) -> VerbPhrase {
    // Normalize: trim, strip trailing punctuation
    let trimmed = raw.trim().trim_end_matches(['.', '…']).trim_end();
    if trimmed.is_empty() {
        return VerbPhrase {
            verb: String::new(),
            detail: String::new(),
        };
    }

    let lower = trimmed.to_lowercase();

    // Pattern: "Retrying {name}..." → verb "retrying", detail = tool name
    if let Some(rest) = lower.strip_prefix("retrying") {
        let name = rest.trim_start_matches(|c: char| !c.is_alphabetic()).trim();
        if !name.is_empty() {
            return VerbPhrase {
                verb: "retrying".into(),
                detail: name.to_string(),
            };
        }
    }

    // Pattern: "Utility action: Verb (name)" → match verb first, then tool name as fallback
    if let Some(rest) = lower.strip_prefix("utility action:")
        && let (Some(open), Some(close)) = (rest.rfind('('), rest.rfind(')'))
        && open < close
    {
        // Prefer the verb portion (e.g. "retrieve" in "Retrieve (some-tool)")
        let verb_part = rest[..open].trim();
        if let Some(phrase) = match_fragment(verb_part) {
            return phrase;
        }
        // Fall back to matching the tool name (e.g. "shell" in "Respond (shell)")
        let name = rest[open + 1..close].trim();
        if let Some(phrase) = match_fragment(name) {
            return phrase;
        }
    }

    // General fragment match on the normalized string
    if let Some(phrase) = match_fragment(&lower) {
        return phrase;
    }

    // Pass-through verbatim — supervisor labels and unknown strings arrive here
    VerbPhrase {
        verb: trimmed.to_string(),
        detail: String::new(),
    }
}

/// Try each entry in `FRAGMENTS`; return first match.
fn match_fragment(text: &str) -> Option<VerbPhrase> {
    for &(fragment, verb, detail) in FRAGMENTS {
        if text.contains(fragment) {
            return Some(VerbPhrase {
                verb: verb.into(),
                detail: detail.into(),
            });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vp(verb: &str, detail: &str) -> VerbPhrase {
        VerbPhrase {
            verb: verb.into(),
            detail: detail.into(),
        }
    }

    #[test]
    fn compact_context() {
        assert_eq!(
            humanize("compacting context..."),
            vp("compacting", "context")
        );
        assert_eq!(
            humanize("soft compacting context..."),
            vp("compacting", "context")
        );
        assert_eq!(
            humanize("Compacting context (server-side)..."),
            vp("compacting", "context")
        );
    }

    #[test]
    fn compress_context() {
        assert_eq!(
            humanize("compressing context..."),
            vp("compressing", "context")
        );
    }

    #[test]
    fn summarizing() {
        assert_eq!(humanize("summarizing..."), vp("summarizing", ""));
        assert_eq!(humanize("summarizing output..."), vp("summarizing", ""));
    }

    #[test]
    fn memory_recall() {
        assert_eq!(humanize("recalling context..."), vp("recalling", ""));
        assert_eq!(humanize("Loading memory..."), vp("loading", "memory"));
        assert_eq!(
            humanize("Connecting to memory store..."),
            vp("connecting", "")
        );
    }

    #[test]
    fn skills() {
        assert_eq!(humanize("matching skills..."), vp("matching", "skills"));
        assert_eq!(humanize("matching skill..."), vp("matching", "skills"));
        assert_eq!(humanize("Loading skills..."), vp("loading", "skills"));
        assert_eq!(humanize("reloading skills..."), vp("reloading", "skills"));
        assert_eq!(humanize("syncing skill index..."), vp("syncing", "skills"));
        assert_eq!(
            humanize("rebuilding search index..."),
            vp("rebuilding", "index")
        );
    }

    #[test]
    fn indexing() {
        assert_eq!(humanize("Indexing codebase..."), vp("indexing", ""));
    }

    #[test]
    fn mcp() {
        assert_eq!(
            humanize("MCP server requesting input…"),
            vp("connecting", "")
        );
    }

    #[test]
    fn tools_filtering() {
        assert_eq!(humanize("filtering tools..."), vp("filtering", "tools"));
        assert_eq!(humanize("selecting tools..."), vp("selecting", "tools"));
    }

    #[test]
    fn thinking_and_evaluating() {
        assert_eq!(humanize("thinking..."), vp("thinking", ""));
        assert_eq!(humanize("Evaluating complexity..."), vp("evaluating", ""));
        assert_eq!(humanize("Analyzing changes..."), vp("analyzing", ""));
    }

    #[test]
    fn session_lifecycle() {
        assert_eq!(
            humanize("Generating session digest..."),
            vp("generating", "digest")
        );
        assert_eq!(humanize("Generating recap..."), vp("generating", "recap"));
        assert_eq!(humanize("Saving session summary..."), vp("saving", ""));
        assert_eq!(
            humanize("Loading conversation history..."),
            vp("loading", "history")
        );
        assert_eq!(humanize("Shutting down..."), vp("shutting down", ""));
        assert_eq!(humanize("Canceling plan..."), vp("canceling", ""));
    }

    #[test]
    fn retrying_extracts_tool_name() {
        // "Retrying read..." → verb "retrying", detail "read"
        let phrase = humanize("Retrying read...");
        assert_eq!(phrase.verb, "retrying");
        assert_eq!(phrase.detail, "read");
    }

    #[test]
    fn utility_action_respond_shell() {
        // "Utility action: Respond (shell)" → verb portion "respond" wins
        let phrase = humanize("Utility action: Respond (shell)");
        assert_eq!(phrase.verb, "responding");
    }

    #[test]
    fn utility_action_retrieve() {
        // "Utility action: Retrieve (some-tool)" → verb portion "retrieve" wins
        let phrase = humanize("Utility action: Retrieve (some-tool)");
        assert_eq!(phrase.verb, "retrieving");
    }

    #[test]
    fn supervisor_label_passthrough() {
        // Supervisor labels must pass through verbatim
        let phrase = humanize("mem-extract +2 more");
        assert_eq!(phrase.verb, "mem-extract +2 more");
        assert_eq!(phrase.detail, "");
    }

    #[test]
    fn unknown_string_passthrough() {
        let phrase = humanize("some unknown operation");
        assert_eq!(phrase.verb, "some unknown operation");
        assert_eq!(phrase.detail, "");
    }

    #[test]
    fn empty_string_returns_empty() {
        let phrase = humanize("");
        assert_eq!(phrase.verb, "");
        assert_eq!(phrase.detail, "");
    }

    #[test]
    fn clear_status_returns_empty() {
        // Empty-string status clears the spinner — both empty
        let phrase = humanize("   ");
        assert_eq!(phrase.verb, "");
        assert_eq!(phrase.detail, "");
    }

    #[test]
    fn trailing_dots_stripped() {
        let a = humanize("thinking...");
        let b = humanize("thinking");
        assert_eq!(a, b);
    }

    #[test]
    fn working_indicator() {
        // bare "working" / "working..." — 7+ real sites
        assert_eq!(humanize("working"), vp("working", ""));
        assert_eq!(humanize("working..."), vp("working", ""));
        assert_eq!(humanize("working\u{2026}"), vp("working", ""));
    }

    #[test]
    fn connecting_tools() {
        // "Connecting tools..." → connecting, not running
        assert_eq!(humanize("Connecting tools..."), vp("connecting", "tools"));
        assert_eq!(humanize("connecting..."), vp("connecting", ""));
    }

    #[test]
    fn executing_task() {
        // "Executing task 1/3: foo..." → "executing · task" (truncated, not full sentence)
        let phrase = humanize("Executing task 1/3: some long task name...");
        assert_eq!(phrase.verb, "executing");
        assert_eq!(phrase.detail, "task");
    }

    #[test]
    fn hooks_dispatch() {
        // Real strings from hooks_dispatch.rs
        assert_eq!(
            humanize("Working directory changed\u{2026}"),
            vp("updating", "directory")
        );
        assert_eq!(
            humanize("Running file-change hook\u{2026}"),
            vp("running", "hook")
        );
    }

    #[test]
    fn running_generic() {
        assert_eq!(humanize("running..."), vp("running", ""));
    }
}
