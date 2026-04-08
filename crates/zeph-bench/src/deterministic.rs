// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use zeph_llm::provider::GenerationOverrides;

/// Returns `GenerationOverrides` that pins temperature to 0.0 for reproducible runs.
///
/// Apply these overrides to each provider via `provider.with_generation_overrides(overrides())`
/// before constructing the agent for a benchmark run.
#[must_use]
pub fn deterministic_overrides() -> GenerationOverrides {
    GenerationOverrides {
        temperature: Some(0.0),
        top_p: None,
        top_k: None,
        frequency_penalty: None,
        presence_penalty: None,
    }
}

/// Apply deterministic overrides to a provider unless `no_deterministic` is set.
///
/// When `no_deterministic` is `false` (the default for bench runs), temperature is
/// forced to 0.0 via `GenerationOverrides`. When `true`, the provider is returned
/// unchanged.
pub fn apply_deterministic_overrides(
    provider: zeph_llm::any::AnyProvider,
    no_deterministic: bool,
) -> zeph_llm::any::AnyProvider {
    if no_deterministic {
        provider
    } else {
        provider.with_generation_overrides(deterministic_overrides())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_overrides_returns_temperature_zero() {
        let overrides = deterministic_overrides();
        assert_eq!(overrides.temperature, Some(0.0));
    }

    #[test]
    fn deterministic_overrides_leaves_other_fields_none() {
        let overrides = deterministic_overrides();
        assert!(overrides.top_p.is_none());
        assert!(overrides.top_k.is_none());
        assert!(overrides.frequency_penalty.is_none());
        assert!(overrides.presence_penalty.is_none());
    }

    #[test]
    fn apply_with_no_deterministic_true_skips_override() {
        // Use Mock provider (zero-network) to verify the skip branch.
        let provider =
            zeph_llm::any::AnyProvider::Mock(zeph_llm::mock::MockProvider::with_responses(vec![]));
        // When no_deterministic=true, provider is returned without applying overrides.
        // We can't introspect the override directly, but we verify the call doesn't panic
        // and returns an AnyProvider (the mock variant).
        let result = apply_deterministic_overrides(provider, true);
        assert!(matches!(result, zeph_llm::any::AnyProvider::Mock(_)));
    }

    #[test]
    fn apply_with_no_deterministic_false_applies_override() {
        let provider =
            zeph_llm::any::AnyProvider::Mock(zeph_llm::mock::MockProvider::with_responses(vec![]));
        // Mock provider's with_generation_overrides is a no-op but still returns Mock variant.
        let result = apply_deterministic_overrides(provider, false);
        assert!(matches!(result, zeph_llm::any::AnyProvider::Mock(_)));
    }
}
