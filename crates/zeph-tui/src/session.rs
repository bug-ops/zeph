// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Per-session state registry for the TUI.
//!
//! Phase-1: always exactly one [`SessionSlot`] owned by [`SessionRegistry`].
//! Phase-2 will add `SessionNew` and multi-slot rendering.

use tokio::sync::oneshot;

use crate::app::{AgentViewTarget, TranscriptCache, TuiTranscriptEntry};
use crate::render_cache::RenderCache;
use crate::types::{ChatMessage, InputMode, PasteState};

/// Maximum number of chat messages retained per session slot.
pub(crate) const MAX_TUI_MESSAGES: usize = 2000;

/// Stable identifier for a TUI session slot.
///
/// Assigned once at creation and never reused within a process lifetime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SlotId(pub u64);

impl SlotId {
    /// The identifier assigned to the first (and in phase-1, only) slot.
    pub const FIRST: Self = SlotId(1);
}

/// All per-session mutable state in one place.
///
/// Phase-1 contents: 10 fields relocated from [`crate::app::App`].
/// `queued_count`, `pending_count`, `editing_queued`, and `subagent_sidebar`
/// stay on `App` because they belong to the single shared agent process.
pub(crate) struct SessionSlot {
    // id and label are used by the tab bar in phase-2; present now so SessionNew can set them.
    #[allow(dead_code)]
    pub id: SlotId,
    #[allow(dead_code)]
    pub label: String,

    // Chat / transcript state
    pub messages: Vec<ChatMessage>,
    pub scroll_offset: usize,
    pub render_cache: RenderCache,

    // Input composer state
    pub input: String,
    pub cursor_position: usize,
    pub input_mode: InputMode,
    pub input_history: Vec<String>,
    pub history_index: Option<usize>,
    pub draft_input: String,
    pub paste_state: Option<PasteState>,

    // Chat area routing
    pub view_target: AgentViewTarget,

    // Sub-agent transcript cache (follows view_target)
    pub transcript_cache: Option<TranscriptCache>,
    pub pending_transcript: Option<oneshot::Receiver<(Vec<TuiTranscriptEntry>, usize)>>,

    // Per-turn splash/plan flags
    pub show_splash: bool,
    pub plan_view_active: bool,
    pub status_label: Option<String>,
}

impl SessionSlot {
    /// Create a new slot with default (empty) state matching the original `App::new` defaults.
    pub fn new(id: SlotId, label: impl Into<String>) -> Self {
        Self {
            id,
            label: label.into(),
            messages: Vec::new(),
            scroll_offset: 0,
            render_cache: RenderCache::default(),
            input: String::new(),
            cursor_position: 0,
            input_mode: InputMode::Insert,
            input_history: Vec::new(),
            history_index: None,
            draft_input: String::new(),
            paste_state: None,
            view_target: AgentViewTarget::Main,
            transcript_cache: None,
            pending_transcript: None,
            show_splash: true,
            plan_view_active: false,
            status_label: None,
        }
    }

    /// Evict oldest messages when the buffer exceeds `MAX_TUI_MESSAGES`.
    ///
    /// Shifts the render cache to match the drained messages, preserving cached renders
    /// for the remaining entries.
    pub fn trim_messages(&mut self) {
        if self.messages.len() > MAX_TUI_MESSAGES {
            let excess = self.messages.len() - MAX_TUI_MESSAGES;
            self.messages.drain(0..excess);
            self.render_cache.shift(excess);
            self.scroll_offset = self.scroll_offset.saturating_sub(excess);
        }
    }
}

/// Registry of all open TUI session slots.
///
/// Phase-1: always exactly one slot. Iteration order is insertion order
/// (browser-tab convention) via [`indexmap::IndexMap`].
pub(crate) struct SessionRegistry {
    slots: indexmap::IndexMap<SlotId, SessionSlot>,
    active: SlotId,
    // next_id is used by create() which is called by tests and will be used by SessionNew in phase-2.
    #[allow(dead_code)]
    next_id: u64,
}

impl SessionRegistry {
    /// Bootstrap with a single slot labelled `"session 1"`.
    pub fn bootstrap() -> Self {
        let id = SlotId::FIRST;
        let mut slots = indexmap::IndexMap::new();
        slots.insert(id, SessionSlot::new(id, "session 1"));
        Self {
            slots,
            active: id,
            next_id: 2,
        }
    }

    /// The currently active slot identifier.
    pub fn active(&self) -> SlotId {
        self.active
    }

    /// Number of open slots.
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    /// Shared reference to the active slot.
    ///
    /// # Panics
    ///
    /// Panics if the active slot is missing (invariant violation; cannot happen
    /// through the public API).
    pub fn current(&self) -> &SessionSlot {
        self.slots
            .get(&self.active)
            .expect("invariant: active slot exists")
    }

    /// Exclusive reference to the active slot.
    ///
    /// # Panics
    ///
    /// Panics if the active slot is missing (invariant violation).
    pub fn current_mut(&mut self) -> &mut SessionSlot {
        self.slots
            .get_mut(&self.active)
            .expect("invariant: active slot exists")
    }

    /// Iterate over all slots in insertion order.
    #[allow(dead_code)]
    pub fn iter(&self) -> impl Iterator<Item = (&SlotId, &SessionSlot)> {
        self.slots.iter()
    }

    /// Focus the next slot cyclically. Single-slot case: no-op (silent).
    pub fn switch_next(&mut self) {
        if self.slots.len() <= 1 {
            return;
        }
        let idx = self
            .slots
            .get_index_of(&self.active)
            .expect("invariant: active slot exists");
        let next_idx = (idx + 1) % self.slots.len();
        self.active = *self.slots.get_index(next_idx).expect("valid index").0;
    }

    /// Focus the previous slot cyclically. Single-slot case: no-op (silent).
    pub fn switch_prev(&mut self) {
        if self.slots.len() <= 1 {
            return;
        }
        let idx = self
            .slots
            .get_index_of(&self.active)
            .expect("invariant: active slot exists");
        let prev_idx = if idx == 0 {
            self.slots.len() - 1
        } else {
            idx - 1
        };
        self.active = *self.slots.get_index(prev_idx).expect("valid index").0;
    }

    /// Close the given slot. Returns `false` and does nothing if this is the last slot.
    ///
    /// Phase-1: always returns `false` because there is always exactly one slot.
    pub fn close(&mut self, id: SlotId) -> bool {
        if self.slots.len() <= 1 {
            return false;
        }
        // Switch to a different slot before removing.
        if self.active == id {
            self.switch_next();
        }
        self.slots.shift_remove(&id);
        true
    }

    /// Create a new slot and return its id.
    ///
    /// Not exposed as a user command in phase-1; exists for unit tests and phase-2.
    #[allow(dead_code)]
    pub(crate) fn create(&mut self, label: impl Into<String>) -> SlotId {
        let id = SlotId(self.next_id);
        self.next_id += 1;
        self.slots.insert(id, SessionSlot::new(id, label));
        id
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::MessageRole;

    fn make_registry() -> SessionRegistry {
        SessionRegistry::bootstrap()
    }

    #[test]
    fn bootstrap_has_one_slot() {
        let reg = make_registry();
        assert_eq!(reg.len(), 1);
        assert_eq!(reg.active(), SlotId::FIRST);
        assert_eq!(reg.current().label, "session 1");
    }

    #[test]
    fn switch_next_noop_on_single_slot() {
        let mut reg = make_registry();
        let before = reg.active();
        reg.switch_next();
        assert_eq!(reg.active(), before);
    }

    #[test]
    fn switch_prev_noop_on_single_slot() {
        let mut reg = make_registry();
        let before = reg.active();
        reg.switch_prev();
        assert_eq!(reg.active(), before);
    }

    #[test]
    fn switch_next_cyclic() {
        let mut reg = make_registry();
        let second = reg.create("session 2");
        let third = reg.create("session 3");

        // Initially active = FIRST
        reg.switch_next();
        assert_eq!(reg.active(), second);
        reg.switch_next();
        assert_eq!(reg.active(), third);
        reg.switch_next();
        assert_eq!(reg.active(), SlotId::FIRST);
    }

    #[test]
    fn switch_prev_cyclic() {
        let mut reg = make_registry();
        let second = reg.create("session 2");
        let third = reg.create("session 3");

        // Initially active = FIRST; go backwards: wraps to third
        reg.switch_prev();
        assert_eq!(reg.active(), third);
        reg.switch_prev();
        assert_eq!(reg.active(), second);
        reg.switch_prev();
        assert_eq!(reg.active(), SlotId::FIRST);
    }

    #[test]
    fn close_refuses_last_slot() {
        let mut reg = make_registry();
        let result = reg.close(SlotId::FIRST);
        assert!(!result);
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn create_bumps_next_id() {
        let mut reg = make_registry();
        let a = reg.create("session 2");
        let b = reg.create("session 3");
        assert_ne!(a, b);
        assert_eq!(a, SlotId(2));
        assert_eq!(b, SlotId(3));
    }

    #[test]
    fn trim_messages_respects_cap() {
        let mut slot = SessionSlot::new(SlotId::FIRST, "test");
        for i in 0..(MAX_TUI_MESSAGES + 10) {
            slot.messages
                .push(ChatMessage::new(MessageRole::User, format!("msg {i}")));
        }
        slot.trim_messages();
        assert_eq!(slot.messages.len(), MAX_TUI_MESSAGES);
    }
}
