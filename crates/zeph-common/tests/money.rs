// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0
#![expect(
    clippy::expect_used,
    reason = "A failed fixture or an impossible serde error should fail the test immediately."
)]

//! Acceptance tests (tests-first) for #7176: `zeph_common::money::{Cents, CentsPer1k}`.

use proptest::prelude::*;
use serde::{Deserialize, Serialize};
use zeph_common::money::{Cents, CentsPer1k};

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
}

#[test]
fn cents_display_defaults_to_four_decimal_dollars() {
    assert_eq!(format!("{}", Cents::new(1.5)), "$0.0150");
    assert_eq!(format!("{}", Cents::ZERO), "$0.0000");
}

#[test]
fn cents_display_explicit_precision_overrides_default() {
    assert_eq!(format!("{:.2}", Cents::new(150.0)), "$1.50");
    assert_eq!(format!("{:.1}", Cents::new(150.0)), "$1.5");
}

#[test]
fn cents_display_ignores_fill_and_width() {
    assert_eq!(format!("{:>12}", Cents::new(1.5)), "$0.0150");
    assert_eq!(format!("{:*<12.2}", Cents::new(150.0)), "$1.50");
}

#[test]
fn cents_usd_divides_by_one_hundred() {
    assert!(close(Cents::new(250.0).usd(), 2.5));
    assert!(close(Cents::new(Cents::new(37.0).as_f64()).usd(), 0.37));
}

#[test]
fn cents_saturating_sub_floors_at_zero() {
    assert_eq!(
        Cents::new(5.0).saturating_sub(Cents::new(3.0)),
        Cents::new(2.0)
    );
    assert_eq!(Cents::new(3.0).saturating_sub(Cents::new(5.0)), Cents::ZERO);
    assert_eq!(Cents::new(3.0).saturating_sub(Cents::new(3.0)), Cents::ZERO);
}

#[test]
fn cents_add_and_add_assign_sum_amounts() {
    let mut total = Cents::new(1.0) + Cents::new(2.0);
    assert_eq!(total, Cents::new(3.0));
    total += Cents::new(0.5);
    assert_eq!(total, Cents::new(3.5));
}

#[test]
fn cents_per_1k_cost_scales_with_tokens() {
    let rate = CentsPer1k::new(2.0);
    assert_eq!(rate.cost(500), Cents::new(1.0));
    assert_eq!(rate.cost(1_000), Cents::new(2.0));
}

#[test]
fn cents_per_1k_cost_zero_tokens_is_zero() {
    assert_eq!(CentsPer1k::new(7.5).cost(0), Cents::ZERO);
}

#[test]
fn cents_per_1k_cost_is_linear_in_tokens() {
    let rate = CentsPer1k::new(0.25);
    assert_eq!(rate.cost(2_000), rate.cost(1_000) + rate.cost(1_000));
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct Wire {
    spent: Cents,
    rate: CentsPer1k,
}

#[test]
fn cents_serde_is_transparent_f64() {
    assert_eq!(
        serde_json::to_string(&Cents::new(1.5)).expect("serialize"),
        "1.5"
    );
    let back: Cents = serde_json::from_str("2.25").expect("deserialize");
    assert_eq!(back, Cents::new(2.25));
    let from_int: Cents = serde_json::from_str("3").expect("deserialize integer");
    assert_eq!(from_int, Cents::new(3.0));
}

#[test]
fn cents_per_1k_serde_is_transparent_f64() {
    assert_eq!(
        serde_json::to_string(&CentsPer1k::new(0.4)).expect("serialize"),
        "0.4"
    );
    let back: CentsPer1k = serde_json::from_str("1.25").expect("deserialize");
    assert_eq!(back, CentsPer1k::new(1.25));
}

#[test]
fn money_serde_inside_struct_keeps_plain_numeric_fields() {
    let wire = Wire {
        spent: Cents::new(1.5),
        rate: CentsPer1k::new(0.25),
    };
    let json = serde_json::to_string(&wire).expect("serialize");
    assert_eq!(json, r#"{"spent":1.5,"rate":0.25}"#);
    assert_eq!(
        serde_json::from_str::<Wire>(&json).expect("round trip"),
        wire
    );
}

proptest! {
    #[test]
    fn cents_saturating_sub_is_max_of_difference_and_zero(
        a in -1.0e9f64..1.0e9,
        b in -1.0e9f64..1.0e9,
    ) {
        let got = Cents::new(a).saturating_sub(Cents::new(b)).as_f64();
        prop_assert!(got >= 0.0);
        prop_assert!(close(got, (a - b).max(0.0)));
    }

    #[test]
    fn cents_per_1k_cost_matches_rate_times_tokens_over_1000(
        rate in 0.0f64..1.0e4,
        tokens in 0u64..10_000_000,
    ) {
        #[expect(clippy::cast_precision_loss, reason = "test tokens stay far below 2^52")]
        let expected = rate * tokens as f64 / 1000.0;
        prop_assert!(close(CentsPer1k::new(rate).cost(tokens).as_f64(), expected));
    }
}
