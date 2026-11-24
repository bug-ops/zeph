// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0
//! Acceptance tests for #7189: `CostTracker::session_spend` is a never-reset accumulator of
//! every recorded cost, independent of the daily budget.

use zeph_common::money::{Cents, CentsPer1k};
use zeph_config::ServiceSpeed;
use zeph_core::cost::{CostTracker, DailyBudget, ModelPricing};
use zeph_llm::provider::TokenUsage;

const MODEL: &str = "acceptance-model";

fn tracker(budget: DailyBudget) -> CostTracker {
    CostTracker::new(budget).with_pricing(
        MODEL,
        ModelPricing {
            prompt_cents_per_1k: CentsPer1k::new(1.0),
            completion_cents_per_1k: CentsPer1k::new(0.0),
            cache_read_cents_per_1k: CentsPer1k::new(0.0),
            cache_write_cents_per_1k: CentsPer1k::new(0.0),
        },
    )
}

fn spend_one_cent(t: &CostTracker) {
    let usage = TokenUsage {
        input: 1_000,
        ..TokenUsage::default()
    };
    t.record_usage("p", "cloud", MODEL, usage, ServiceSpeed::Standard);
}

#[test]
fn test_session_spend_starts_at_zero_and_sums_recorded_costs() {
    let t = tracker(DailyBudget::Unlimited);
    assert_eq!(t.session_spend(), Cents::ZERO);
    spend_one_cent(&t);
    spend_one_cent(&t);
    assert_eq!(t.session_spend(), Cents::new(2.0));
}

#[test]
fn test_session_spend_matches_current_spend_within_one_day() {
    let t = tracker(DailyBudget::from_config(100));
    spend_one_cent(&t);
    assert_eq!(t.session_spend(), t.current_spend());
}
