// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Recurrence-guard coverage for `Agent::set_provider` (#5437 round-3 follow-up, S1/M1).
//!
//! Two prior rounds each missed one runtime `self.provider` reassignment site that skipped
//! secret masking (the primary turn-loop dispatch first, then the ACP `set_session_config_option`
//! provider override). `set_provider` is the single guarded path every reassignment must go
//! through now — these tests exercise it directly and via `apply_provider_override`.

use std::sync::Arc;

use zeph_llm::any::AnyProvider;
use zeph_sanitizer::secret_mask::SecretMaskRegistry;

use crate::agent::agent_tests::{
    MockChannel, MockToolExecutor, create_test_registry, mock_provider,
};

fn base_agent() -> crate::agent::Agent<MockChannel> {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    crate::agent::Agent::new(provider, channel, registry, None, 5, executor)
}

#[test]
fn set_provider_wraps_unmasked_provider_when_masking_enabled() {
    let mut agent = base_agent().with_secret_registry(Arc::new(SecretMaskRegistry::new()));
    // Confirm the initial construction wrap already applied (belt-and-braces on the fixture).
    assert!(matches!(agent.provider, AnyProvider::Masked(_)));

    // A raw, unwrapped provider arriving via any future reassignment path.
    let fresh_unmasked = mock_provider(vec!["hi".into()]);
    assert!(!matches!(fresh_unmasked, AnyProvider::Masked(_)));

    agent.set_provider(fresh_unmasked);

    assert!(
        matches!(agent.provider, AnyProvider::Masked(_)),
        "set_provider must wrap an unmasked provider when secret masking is enabled"
    );
}

#[test]
fn set_provider_does_not_double_wrap_already_masked_provider() {
    let mut agent = base_agent().with_secret_registry(Arc::new(SecretMaskRegistry::new()));
    let registry = Arc::new(SecretMaskRegistry::new());
    let already_masked = mock_provider(vec![])
        .masked(Arc::clone(&registry) as Arc<dyn zeph_llm::masking::OutboundMasker>);

    agent.set_provider(already_masked);

    assert!(matches!(agent.provider, AnyProvider::Masked(_)));
    // Exactly one layer: unwrap once and confirm the inner provider is the raw mock, not
    // another `Masked` wrapper.
    if let AnyProvider::Masked(p) = &agent.provider {
        assert!(
            !matches!(p.inner(), AnyProvider::Masked(_)),
            "set_provider must not nest a second Masked layer around an already-wrapped provider"
        );
    }
}

#[test]
fn set_provider_leaves_provider_unwrapped_when_masking_disabled() {
    let mut agent = base_agent();
    assert!(agent.services.security.secret_registry.is_none());

    let fresh = mock_provider(vec!["hi".into()]);
    agent.set_provider(fresh);

    assert!(
        !matches!(agent.provider, AnyProvider::Masked(_)),
        "no registry attached — set_provider must be a no-op passthrough"
    );
}

#[test]
fn apply_provider_override_masks_new_provider() {
    let slot: Arc<parking_lot::RwLock<Option<AnyProvider>>> =
        Arc::new(parking_lot::RwLock::new(None));
    let mut agent = base_agent()
        .with_provider_override(Arc::clone(&slot))
        .with_secret_registry(Arc::new(SecretMaskRegistry::new()));

    // Simulate ACP's `set_session_config_option` populating the override slot with a freshly
    // built, unwrapped provider (mirrors `zeph-acp`'s own construction path, which has no
    // access to the agent's secret registry).
    *slot.write() = Some(mock_provider(vec!["override response".into()]));

    agent.apply_provider_override();

    assert!(
        matches!(agent.provider, AnyProvider::Masked(_)),
        "ACP provider override must be masked by apply_provider_override, not assigned raw"
    );
}

#[test]
fn apply_provider_override_noop_when_slot_empty() {
    let slot: Arc<parking_lot::RwLock<Option<AnyProvider>>> =
        Arc::new(parking_lot::RwLock::new(None));
    let mut agent = base_agent()
        .with_provider_override(Arc::clone(&slot))
        .with_secret_registry(Arc::new(SecretMaskRegistry::new()));
    let before = format!("{:?}", agent.provider);

    agent.apply_provider_override();

    assert_eq!(format!("{:?}", agent.provider), before);
}
