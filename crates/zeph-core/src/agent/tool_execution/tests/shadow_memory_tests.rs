// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Tests for `ShadowMemory` wiring via the `with_shadow_memory_config` builder (spec 010-7).

use zeph_config::ShadowMemoryConfig;

use crate::agent::agent_tests::{
    MockChannel, MockToolExecutor, create_test_registry, mock_provider,
};

#[test]
fn shadow_memory_disabled_config_leaves_none() {
    let cfg = ShadowMemoryConfig {
        enabled: false,
        ..Default::default()
    };
    let agent = crate::agent::Agent::new(
        mock_provider(vec![]),
        MockChannel::new(vec![]),
        create_test_registry(),
        None,
        5,
        MockToolExecutor::no_tools(),
    )
    .with_shadow_memory_config(&cfg);
    assert!(
        agent.services.security.shadow_memory.is_none(),
        "shadow_memory must be None when config.enabled = false"
    );
}

#[test]
fn shadow_memory_enabled_config_sets_some() {
    let cfg = ShadowMemoryConfig {
        enabled: true,
        ..Default::default()
    };
    let agent = crate::agent::Agent::new(
        mock_provider(vec![]),
        MockChannel::new(vec![]),
        create_test_registry(),
        None,
        5,
        MockToolExecutor::no_tools(),
    )
    .with_shadow_memory_config(&cfg);
    assert!(
        agent.services.security.shadow_memory.is_some(),
        "shadow_memory must be Some when config.enabled = true"
    );
}

#[test]
fn shadow_memory_starts_empty_after_enable() {
    let cfg = ShadowMemoryConfig {
        enabled: true,
        ..Default::default()
    };
    let agent = crate::agent::Agent::new(
        mock_provider(vec![]),
        MockChannel::new(vec![]),
        create_test_registry(),
        None,
        5,
        MockToolExecutor::no_tools(),
    )
    .with_shadow_memory_config(&cfg);
    let mem = agent.services.security.shadow_memory.as_ref().unwrap();
    assert_eq!(
        mem.len(),
        0,
        "shadow_memory must contain zero recorded events after construction"
    );
}
