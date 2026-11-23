// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0
#![expect(
    clippy::expect_used,
    reason = "A failed fixture or an impossible serde error should fail the test immediately."
)]

//! Acceptance tests (tests-first) for #7176: `DailyBudget` semantics and
//! `CostTracker::new(DailyBudget)` behaviour with typed `Cents`.

use zeph_common::money::{Cents, CentsPer1k};
use zeph_config::ServiceSpeed;
use zeph_core::cost::{CostTracker, DailyBudget, ModelPricing};
use zeph_llm::provider::{CacheUsage, ResponseUsage, TokenUsage};

const MODEL: &str = "acceptance-model";

/// One cent per 1000 prompt tokens and per 1000 cache-read tokens.
fn priced(budget: DailyBudget) -> CostTracker {
    CostTracker::new(budget).with_pricing(
        MODEL,
        ModelPricing {
            prompt_cents_per_1k: CentsPer1k::new(1.0),
            completion_cents_per_1k: CentsPer1k::new(0.0),
            cache_read_cents_per_1k: CentsPer1k::new(1.0),
            cache_write_cents_per_1k: CentsPer1k::new(0.0),
        },
    )
}

fn one_cent_usage() -> TokenUsage {
    TokenUsage {
        input: 1_000,
        ..TokenUsage::default()
    }
}

fn spend_one_cent(tracker: &CostTracker) {
    tracker.record_usage(
        "p",
        "cloud",
        MODEL,
        one_cent_usage(),
        ServiceSpeed::Standard,
    );
}

// ---- DailyBudget ----

#[test]
fn daily_budget_from_config_zero_is_unlimited() {
    let budget = DailyBudget::from_config(0);
    assert_eq!(budget.limit(), None);
    assert_eq!(budget.remaining(Cents::new(1.0e9)), None);
    assert!(!budget.is_exhausted(Cents::new(1.0e9)));
}

#[test]
fn daily_budget_from_config_nonzero_is_limited() {
    assert_eq!(
        DailyBudget::from_config(500).limit(),
        Some(Cents::new(500.0))
    );
}

#[test]
fn daily_budget_remaining_saturates_at_zero() {
    let budget = DailyBudget::Limited(Cents::new(500.0));
    assert_eq!(budget.remaining(Cents::new(120.0)), Some(Cents::new(380.0)));
    assert_eq!(budget.remaining(Cents::new(500.0)), Some(Cents::ZERO));
    assert_eq!(budget.remaining(Cents::new(900.0)), Some(Cents::ZERO));
}

#[test]
fn daily_budget_is_exhausted_at_and_above_limit_only() {
    let budget = DailyBudget::Limited(Cents::new(100.0));
    assert!(!budget.is_exhausted(Cents::new(99.99)));
    assert!(budget.is_exhausted(Cents::new(100.0)));
    assert!(budget.is_exhausted(Cents::new(100.01)));
}

#[test]
fn daily_budget_non_positive_limit_is_always_exhausted() {
    assert!(DailyBudget::Limited(Cents::ZERO).is_exhausted(Cents::ZERO));
    assert!(DailyBudget::Limited(Cents::new(-1.0)).is_exhausted(Cents::ZERO));
}

// ---- CostTracker::new(DailyBudget) ----

#[test]
fn cost_tracker_budget_returns_configured_budget() {
    let tracker = CostTracker::new(DailyBudget::from_config(300));
    assert_eq!(tracker.budget().limit(), Some(Cents::new(300.0)));
    assert_eq!(
        CostTracker::new(DailyBudget::Unlimited).budget().limit(),
        None
    );
}

#[test]
fn cost_tracker_unlimited_never_exhausts() {
    let tracker = priced(DailyBudget::Unlimited);
    for _ in 0..5 {
        spend_one_cent(&tracker);
    }
    assert_eq!(tracker.current_spend(), Cents::new(5.0));
    assert!(tracker.check_budget().is_ok());
}

#[test]
fn cost_tracker_check_budget_errs_exactly_at_limit() {
    let tracker = priced(DailyBudget::Limited(Cents::new(2.0)));
    assert!(tracker.check_budget().is_ok());
    spend_one_cent(&tracker);
    assert!(tracker.check_budget().is_ok());
    spend_one_cent(&tracker);
    let err = tracker
        .check_budget()
        .expect_err("spend == limit is exhausted");
    assert_eq!(err.spent_cents, Cents::new(2.0));
    assert_eq!(err.budget_cents, Cents::new(2.0));
}

#[test]
fn cost_tracker_limited_zero_is_exhausted_before_any_spend() {
    let tracker = CostTracker::new(DailyBudget::Limited(Cents::ZERO));
    assert!(tracker.check_budget().is_err());
}

#[test]
fn budget_exhausted_message_bytes_are_unchanged() {
    let tracker = priced(DailyBudget::Limited(Cents::new(1.0)));
    spend_one_cent(&tracker);
    let err = tracker.check_budget().expect_err("exhausted");
    assert_eq!(
        err.to_string(),
        "daily budget exhausted: spent 1.00 / 1.00 cents \
         (raise or disable via `[cost] max_daily_cents` in config.toml; 0 = unlimited)"
    );
}

#[test]
fn cost_tracker_price_of_returns_cents() {
    let tracker = priced(DailyBudget::Unlimited);
    assert_eq!(
        tracker.price_of(MODEL, one_cent_usage(), ServiceSpeed::Standard),
        Cents::new(1.0)
    );
    assert_eq!(
        tracker.price_of("unpriced-model", one_cent_usage(), ServiceSpeed::Standard),
        Cents::ZERO
    );
}

#[test]
fn cost_tracker_record_keepalive_usage_returns_cost_added_to_spend() {
    let tracker = priced(DailyBudget::Unlimited);
    let usage = ResponseUsage {
        cache: Some(CacheUsage {
            creation: 0,
            read: 1_000,
        }),
        ..ResponseUsage::default()
    };
    let cost = tracker.record_keepalive_usage("p", "cloud", MODEL, usage);
    assert_eq!(cost, Cents::new(1.0));
    assert_eq!(tracker.current_spend(), cost);
}

#[test]
fn cost_tracker_cps_is_spend_per_successful_task() {
    let tracker = priced(DailyBudget::Unlimited);
    assert_eq!(tracker.cps(), None);
    spend_one_cent(&tracker);
    tracker.record_successful_task();
    tracker.record_successful_task();
    assert_eq!(tracker.cps(), Some(Cents::new(0.5)));
}

#[test]
fn cost_tracker_provider_breakdown_carries_cents() {
    let tracker = priced(DailyBudget::Unlimited);
    spend_one_cent(&tracker);
    let breakdown = tracker.provider_breakdown();
    assert_eq!(breakdown.len(), 1);
    assert_eq!(breakdown[0].1.cost_cents, Cents::new(1.0));
}
