// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Tests for #6272: `Agent::begin_turn` maps drained `RiskSignal`s to MAGE
//! `(AuditSignalType, Severity)` pairs by matching on the already-decoded `RiskSignal` enum
//! rather than re-deriving the mapping from the raw `u8` signal code. These tests pin the
//! resulting mapping table (spec 004-19 FR-002/FR-007) so a future refactor of either
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
/// (spec 004-19 FR-002). Each must ingest into `mage_accumulator` with the exact
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
    // 10/11 (ExfilReadThenSend/CredThenEgress, #6561/F2) are trajectory-only too — MAGE's
    // mapping stays at the fixed spec 004-19 four-class set; widening it is out of scope.
    for code in [3u8, 4, 5, 10, 11, 99] {
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
    assert_eq!(RiskSignal::from_code(10), RiskSignal::ExfilReadThenSend);
    assert_eq!(RiskSignal::from_code(11), RiskSignal::CredThenEgress);
    assert_eq!(
        RiskSignal::from_code(99),
        RiskSignal::VigilFlagged(VigilRiskLevel::Low)
    );
}

/// Regression for F2 (found during #6561 rework review): codes `10`/`11` — pushed by
/// `zeph-tools`'s `RiskChainAccumulator` when it confirms a multi-step attack chain — used to
/// fall through `from_code`'s wildcard arm into `VigilFlagged(Low)`, the lowest weight tier
/// (0.3, same as noisy `ToolFailure`), silently near-inert once ingested by
/// `TrajectorySentinel`. A confirmed chain fire is a high-confidence signal and must weight at
/// least as high as other confirmed-pattern signals (`ExfiltrationRedaction`/
/// `ToolPairTransition`, both 2.0), not the low-confidence fallback tier.
#[test]
fn risk_chain_signal_codes_weight_above_fallback_tier() {
    use crate::agent::trajectory::VigilRiskLevel;

    let fallback_weight = RiskSignal::VigilFlagged(VigilRiskLevel::Low).default_weight();
    let exfil_weight = RiskSignal::from_code(10).default_weight();
    let cred_weight = RiskSignal::from_code(11).default_weight();

    assert!(
        exfil_weight > fallback_weight,
        "exfil_read_then_send (code 10) must weight above the VigilFlagged(Low) fallback tier \
         ({fallback_weight}), got {exfil_weight}"
    );
    assert!(
        cred_weight > fallback_weight,
        "cred_then_egress (code 11) must weight above the VigilFlagged(Low) fallback tier \
         ({fallback_weight}), got {cred_weight}"
    );
    assert!(
        exfil_weight >= RiskSignal::ExfiltrationRedaction.default_weight(),
        "a confirmed exfil chain must weight at least as high as ExfiltrationRedaction"
    );
    assert!(
        cred_weight >= RiskSignal::ExfiltrationRedaction.default_weight(),
        "a confirmed cred-then-egress chain must weight at least as high as ExfiltrationRedaction"
    );
}

/// Regression for critic finding C1 (#6490): `begin_turn` must reset the turn-scoped
/// memory-consent trust tracker so a prior turn's untrusted tool output cannot leak into a
/// later turn's `memory_save` confirmation-gate decision. Doc comments in `sanitize.rs`,
/// `memory_tools.rs`, and `state/mod.rs` all claim this reset already happens here.
#[test]
fn begin_turn_resets_memory_consent_trust_to_zero() {
    let mut agent = make_agent_with_mage();
    // Simulate sanitize_tool_output having ratcheted the slot up during the prior turn.
    *agent.services.security.memory_consent_trust.write() = 2; // ExternalUntrusted
    let _turn = agent.begin_turn(TurnInput::new("hi".to_owned(), vec![]));
    assert_eq!(
        *agent.services.security.memory_consent_trust.read(),
        0,
        "memory_consent_trust must reset to 0 (Trusted) at the start of a new turn"
    );
}
