// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Sticky per-conversation tool offers (#6840).
//!
//! The tools array precedes the system prompt and messages in the provider's prompt cache, so a
//! per-turn change to the offered tool set invalidates everything after it. A
//! [`StickyToolOffer`] keeps the union of every set offered so far in the conversation: it only
//! grows, so each growth step costs one cache rewrite and a stable set costs none.

use std::collections::HashSet;

/// Union of the tool ids offered so far in one conversation.
///
/// `None` inside means no filter has succeeded yet: callers then offer every tool and record
/// nothing, so a transient embedding failure never narrows or widens the recorded set.
#[derive(Debug, Default)]
pub(crate) struct StickyToolOffer {
    offered: Option<HashSet<String>>,
}

impl StickyToolOffer {
    /// Merge a successful filter result into the union and return the union.
    pub(crate) fn grow(&mut self, fresh: impl IntoIterator<Item = String>) -> &HashSet<String> {
        let union = self.offered.get_or_insert_with(HashSet::new);
        union.extend(fresh);
        union
    }

    /// The union recorded so far, or `None` when no filter result has been recorded yet.
    pub(crate) fn current(&self) -> Option<&HashSet<String>> {
        self.offered.as_ref()
    }

    /// Forget the union (conversation cleared).
    pub(crate) fn reset(&mut self) {
        self.offered = None;
    }
}

/// Sticky offers for the two independent tool sources.
#[derive(Debug, Default)]
pub(crate) struct ToolOfferState {
    /// Native tool ids chosen by the tool schema filter.
    pub(crate) native: StickyToolOffer,
    /// Qualified MCP tool names chosen by embedding discovery.
    pub(crate) mcp: StickyToolOffer,
}

impl ToolOfferState {
    /// Forget both unions (conversation cleared).
    pub(crate) fn reset(&mut self) {
        self.native.reset();
        self.mcp.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn grows_monotonically() {
        let mut offer = StickyToolOffer::default();
        assert!(offer.current().is_none());
        offer.grow(ids(&["a", "b"]));
        assert_eq!(offer.current().unwrap().len(), 2);
        offer.grow(ids(&["c"]));
        let current = offer.current().unwrap();
        assert!(["a", "b", "c"].iter().all(|id| current.contains(*id)));
    }

    #[test]
    fn empty_first_result_is_recorded() {
        let mut offer = StickyToolOffer::default();
        offer.grow(Vec::new());
        assert_eq!(offer.current().map(HashSet::len), Some(0));
    }

    #[test]
    fn reset_forgets_union() {
        let mut state = ToolOfferState::default();
        state.native.grow(ids(&["a"]));
        state.mcp.grow(ids(&["m"]));
        state.reset();
        assert!(state.native.current().is_none());
        assert!(state.mcp.current().is_none());
    }
}
