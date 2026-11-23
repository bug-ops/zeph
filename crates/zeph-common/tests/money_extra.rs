// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Extra coverage for `CentsPer1k` not pinned by the protected `money.rs` suite.

use zeph_common::money::CentsPer1k;

#[test]
fn cents_per_1k_as_f64_returns_wrapped_rate() {
    assert_eq!(CentsPer1k::new(2.5).as_f64().to_bits(), 2.5_f64.to_bits());
}
