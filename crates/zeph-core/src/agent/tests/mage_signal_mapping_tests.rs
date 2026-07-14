// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Tests for #6272: `Agent::begin_turn` maps drained `RiskSignal`s to MAGE
//! `(AuditSignalType, Severity)` pairs by matching on the already-decoded `RiskSignal` enum
//! rather than re-deriving the mapping from the raw `u8` signal code. These tests pin the
//! resulting mapping table (spec 004-16 FR-002/FR-007) so a future refactor of either
//! `RiskSignal::from_code` or the MAGE match arm cannot silently desync the two.

use zeph_config::TrajectoryRiskAccumulatorConfig;
use zeph_memory::shadow::{AuditSignalType, Severity};

use crate::agent::agent_tests::{
    MockChannel, MockToolExecutor, create_test_registry, mock_provider,
};
use crate::agent::turn::TurnInput;
use crate::agent::{Agent, trajectory::RiskSignal};

fn make_agent_with_mage() -> Agent<MockChannel> {
    let agent = Agent::new(
        mock_provider(vec![]),
        MockChannel::new(vec![]),
        create_test_registry(),
        None,
        5,
        MockToolExecutor::no_tools(),
    );
    agent.with_mage_accumulator_config(TrajectoryRiskAccumulatorConfig {
        enabled: true,
        ..Default::default()
    })
}

/// Push a raw signal code into the trajectory queue the same way `RiskSignalSink` callbacks
/// do, then drive one turn so `begin_turn` drains and maps it.
fn drain_one_code(agent: &mut Agent<MockChannel>, code: u8) {
    agent
        .services
        .security
        .trajectory_signal_queue
        .lock()
        .push(code);
    let _turn = agent.begin_turn(TurnInput::new("hi".to_owned(), vec![]));
}

/// Codes 1, 2, 6, 7 (`PolicyDeny`, `ExfiltrationRedaction`, `VigilFlagged(Medium)`,
/// `VigilFlagged(High)`) are the only `RiskSignal` variants with a MAGE equivalent
/// (spec 004-16 FR-002). Each must ingest into `mage_accumulator` with the exact
/// `AuditSignalType`/`Severity` pair documented at the match site in `begin_turn`.
#[test]
fn begin_turn_maps_known_risk_codes_to_mage_signals() {
    let cases: [(u8, AuditSignalType, Severity); 4] = [
        (1, AuditSignalType::PolicyViolation, Severity::Medium),
        (2, AuditSignalType::ToolChainAnomaly, Severity::Medium),
        (6, AuditSignalType::PromptInjectionPattern, Severity::Medium),
        (7, AuditSignalType::PromptInjectionPattern, Severity::High),
    ];

    for (code, expected_type, expected_severity) in cases {
        let mut agent = make_agent_with_mage();
        drain_one_code(&mut agent, code);

        assert!(
            agent.services.security.mage_accumulator.current_risk() > 0.0,
            "code {code} must ingest a non-zero-weight MAGE signal"
        );
        let top = agent.services.security.mage_accumulator.top_signals(1);
        assert_eq!(
            top.len(),
            1,
            "code {code} must record exactly one MAGE signal event"
        );
        assert_eq!(
            top[0].signal_type, expected_type,
            "code {code} mapped to the wrong AuditSignalType"
        );
        assert_eq!(
            top[0].severity, expected_severity,
            "code {code} mapped to the wrong Severity"
        );
    }
}

/// The remaining `RiskSignal` variants — `OutOfScope` (3), `PiiRedaction` (4),
/// `ToolFailure` (5), and the `VigilFlagged(Low)` fallback (any unmapped code, e.g. 99) —
/// are trajectory-only per the doc comment above the match in `begin_turn` and must NOT
/// surface to MAGE.
#[test]
fn begin_turn_no_mage_signal_for_trajectory_only_codes() {
    for code in [3u8, 4, 5, 99] {
        let mut agent = make_agent_with_mage();
        drain_one_code(&mut agent, code);

        // trajectory_risk only ever accumulates non-negative contributions, so `<= 0.0` is
        // equivalent to `== 0.0` here without tripping clippy::float_cmp on exact equality.
        assert!(
            agent.services.security.mage_accumulator.current_risk() <= 0.0,
            "code {code} must not ingest any MAGE signal (trajectory-only)"
        );
        assert!(
            agent
                .services
                .security
                .mage_accumulator
                .top_signals(1)
                .is_empty(),
            "code {code} must leave MAGE signal history empty"
        );
    }
}

/// Sanity guard: `RiskSignal::from_code` itself must still decode these codes to the
/// variants this test file assumes — if this fails, the MAGE-mapping tests above are
/// exercising the wrong `RiskSignal`, not the mapping logic.
#[test]
fn risk_signal_from_code_matches_assumed_variants() {
    use crate::agent::trajectory::VigilRiskLevel;

    assert_eq!(RiskSignal::from_code(1), RiskSignal::PolicyDeny);
    assert_eq!(RiskSignal::from_code(2), RiskSignal::ExfiltrationRedaction);
    assert_eq!(
        RiskSignal::from_code(6),
        RiskSignal::VigilFlagged(VigilRiskLevel::Medium)
    );
    assert_eq!(
        RiskSignal::from_code(7),
        RiskSignal::VigilFlagged(VigilRiskLevel::High)
    );
    assert_eq!(RiskSignal::from_code(3), RiskSignal::OutOfScope);
    assert_eq!(RiskSignal::from_code(4), RiskSignal::PiiRedaction);
    assert_eq!(RiskSignal::from_code(5), RiskSignal::ToolFailure);
    assert_eq!(
        RiskSignal::from_code(99),
        RiskSignal::VigilFlagged(VigilRiskLevel::Low)
    );
}
