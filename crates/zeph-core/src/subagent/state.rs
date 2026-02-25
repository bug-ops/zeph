// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

/// Lifecycle state of a sub-agent task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubAgentState {
    Submitted,
    Working,
    Completed,
    Failed,
    Canceled,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_all_variants_debug() {
        assert_eq!(format!("{:?}", SubAgentState::Submitted), "Submitted");
        assert_eq!(format!("{:?}", SubAgentState::Working), "Working");
        assert_eq!(format!("{:?}", SubAgentState::Completed), "Completed");
        assert_eq!(format!("{:?}", SubAgentState::Failed), "Failed");
        assert_eq!(format!("{:?}", SubAgentState::Canceled), "Canceled");
    }

    #[test]
    fn test_clone_and_copy() {
        let state = SubAgentState::Working;
        let cloned = state;
        assert_eq!(state, cloned);
        let copied: SubAgentState = state;
        assert_eq!(copied, SubAgentState::Working);
    }

    #[test]
    fn test_partial_eq() {
        assert_eq!(SubAgentState::Completed, SubAgentState::Completed);
        assert_ne!(SubAgentState::Submitted, SubAgentState::Failed);
    }

    #[test]
    fn test_terminal_states_are_distinct_from_active() {
        let active = [SubAgentState::Submitted, SubAgentState::Working];
        let terminal = [
            SubAgentState::Completed,
            SubAgentState::Failed,
            SubAgentState::Canceled,
        ];
        for a in active {
            for t in terminal {
                assert_ne!(a, t);
            }
        }
    }
}
