// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Per-lane activity status board.
//!
//! The activity bar used to be a single `Option<String>` slot, so a clear from one producer (the
//! agent turn finishing) hid a status that another producer (backfill, indexing) still owned.
//! The board keeps one slot per [`StatusLane`]: a set or clear touches only its own lane, and
//! the visible text is the most recently set lane.

use crate::event::{LaneUpdate, ProducerLane};

/// Number of lanes on the board.
const LANES: usize = 10;

/// A status writer identity on the TUI side.
///
/// Producers outside the TUI address a subset through [`ProducerLane`]; the remaining lanes are
/// written only by TUI-local operations, so an external event cannot overwrite them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StatusLane {
    /// Agent turn: thinking, tool activity, subagent labels.
    Turn,
    /// Background subsystems reporting through `StatusSender`.
    Background,
    /// Startup sequence in the binary.
    Bootstrap,
    /// Model warm-up and embedding backfill.
    Warmup,
    /// Code-index progress.
    CodeIndex,
    /// Deep-link handling.
    DeepLink,
    /// Periodic fleet and durable-run polls.
    Refresh,
    /// File-picker index build (TUI-local).
    FileIndex,
    /// Subagent transcript load (TUI-local).
    TranscriptLoad,
    /// Theme file load (TUI-local).
    ThemeLoad,
}

impl StatusLane {
    const fn index(self) -> usize {
        match self {
            Self::Turn => 0,
            Self::Background => 1,
            Self::Bootstrap => 2,
            Self::Warmup => 3,
            Self::CodeIndex => 4,
            Self::DeepLink => 5,
            Self::Refresh => 6,
            Self::FileIndex => 7,
            Self::TranscriptLoad => 8,
            Self::ThemeLoad => 9,
        }
    }
}

impl From<ProducerLane> for StatusLane {
    fn from(lane: ProducerLane) -> Self {
        match lane {
            ProducerLane::Background => Self::Background,
            ProducerLane::Bootstrap => Self::Bootstrap,
            ProducerLane::Warmup => Self::Warmup,
            ProducerLane::CodeIndex => Self::CodeIndex,
            ProducerLane::DeepLink => Self::DeepLink,
            ProducerLane::Refresh => Self::Refresh,
        }
    }
}

/// Coarse state of the board; "is the agent busy" must key on [`Self::Turn`] only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BoardActivity {
    /// No lane holds a status.
    Idle,
    /// Only non-turn lanes hold a status (backfill, indexing, refresh, loads).
    Background,
    /// The agent-turn lane holds a status.
    Turn,
}

#[derive(Debug, Clone)]
struct Slot {
    seq: u64,
    text: String,
}

/// One status slot per [`StatusLane`] with latest-set-wins visibility.
#[derive(Debug, Default)]
pub(crate) struct StatusBoard {
    slots: [Option<Slot>; LANES],
    seq: u64,
}

impl StatusBoard {
    /// Show `text` on `lane`, replacing that lane's previous text only.
    pub(crate) fn set(&mut self, lane: StatusLane, text: impl Into<String>) {
        self.seq += 1;
        self.slots[lane.index()] = Some(Slot {
            seq: self.seq,
            text: text.into(),
        });
    }

    /// Clear `lane`; every other lane keeps its text.
    pub(crate) fn clear(&mut self, lane: StatusLane) {
        self.slots[lane.index()] = None;
    }

    /// Apply an external producer update to its lane.
    pub(crate) fn apply(&mut self, lane: ProducerLane, update: LaneUpdate) {
        match update {
            LaneUpdate::Set(text) => self.set(lane.into(), text),
            LaneUpdate::Clear => self.clear(lane.into()),
        }
    }

    /// Text of the most recently set non-empty lane.
    pub(crate) fn visible(&self) -> Option<&str> {
        self.slots
            .iter()
            .flatten()
            .max_by_key(|slot| slot.seq)
            .map(|slot| slot.text.as_str())
    }

    /// What the board's occupied lanes say about the agent: a set turn lane means a turn is
    /// running, any other lane is background work only.
    pub(crate) fn activity(&self) -> BoardActivity {
        if self.slots[StatusLane::Turn.index()].is_some() {
            BoardActivity::Turn
        } else if self.slots.iter().any(Option::is_some) {
            BoardActivity::Background
        } else {
            BoardActivity::Idle
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clear_hides_only_its_own_lane() {
        let mut board = StatusBoard::default();
        board.set(StatusLane::Background, "Backfilling embeddings…");
        board.set(StatusLane::Turn, "thinking...");
        assert_eq!(board.visible(), Some("thinking..."));

        board.clear(StatusLane::Turn);
        assert_eq!(board.visible(), Some("Backfilling embeddings…"));
        assert_eq!(board.activity(), BoardActivity::Background);

        board.clear(StatusLane::Background);
        assert_eq!(board.visible(), None);
        assert_eq!(board.activity(), BoardActivity::Idle);
    }

    #[test]
    fn activity_is_turn_only_when_the_turn_lane_is_set() {
        let mut board = StatusBoard::default();
        assert_eq!(board.activity(), BoardActivity::Idle);
        board.set(StatusLane::ThemeLoad, "loading theme...");
        assert_eq!(board.activity(), BoardActivity::Background);
        board.set(StatusLane::Turn, "thinking...");
        assert_eq!(board.activity(), BoardActivity::Turn);
        board.clear(StatusLane::Turn);
        assert_eq!(board.activity(), BoardActivity::Background);
    }

    #[test]
    fn latest_set_wins_and_older_lane_is_the_fallback() {
        let mut board = StatusBoard::default();
        board.set(StatusLane::CodeIndex, "indexing");
        board.set(StatusLane::Refresh, "fleet");
        board.set(StatusLane::CodeIndex, "indexing 2");
        assert_eq!(board.visible(), Some("indexing 2"));
        board.clear(StatusLane::CodeIndex);
        assert_eq!(board.visible(), Some("fleet"));
    }

    #[test]
    fn set_replaces_only_the_same_lane() {
        let mut board = StatusBoard::default();
        board.set(StatusLane::Turn, "a");
        board.set(StatusLane::Turn, "b");
        board.clear(StatusLane::Turn);
        assert_eq!(board.activity(), BoardActivity::Idle);
    }

    #[test]
    fn external_updates_map_to_their_lane() {
        let mut board = StatusBoard::default();
        board.apply(ProducerLane::Warmup, LaneUpdate::Set("warming".into()));
        board.set(StatusLane::Turn, "thinking...");
        board.apply(ProducerLane::Warmup, LaneUpdate::Clear);
        assert_eq!(board.visible(), Some("thinking..."));
        board.apply(ProducerLane::Background, LaneUpdate::Clear);
        assert_eq!(board.visible(), Some("thinking..."));
    }

    #[test]
    fn every_lane_has_a_distinct_slot() {
        let lanes = [
            StatusLane::Turn,
            StatusLane::Background,
            StatusLane::Bootstrap,
            StatusLane::Warmup,
            StatusLane::CodeIndex,
            StatusLane::DeepLink,
            StatusLane::Refresh,
            StatusLane::FileIndex,
            StatusLane::TranscriptLoad,
            StatusLane::ThemeLoad,
        ];
        assert_eq!(lanes.len(), LANES);
        let mut board = StatusBoard::default();
        for lane in lanes {
            board.set(lane, "x");
        }
        for (cleared, lane) in lanes.iter().enumerate() {
            board.clear(*lane);
            let remaining = board.slots.iter().flatten().count();
            assert_eq!(remaining, LANES - cleared - 1);
        }
    }
}
