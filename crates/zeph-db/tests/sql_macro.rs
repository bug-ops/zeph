// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Regression coverage for #5431: the `postgres` `sql!` macro must cache its
//! rewritten query in a per-call-site `static LazyLock<String>` instead of
//! leaking a fresh heap allocation on every invocation (the original defect).
//!
//! Only meaningful under the `postgres` feature — under `sqlite`, `sql!` is an
//! identity macro over a `&'static str` literal, and the compiler is free to
//! deduplicate identical string literals, which would make a pointer-identity
//! test meaningless (and possibly flaky) for that branch.
#![cfg(feature = "postgres")]

/// Distinct call site from [`call_site_b`] despite the identical literal —
/// macro hygiene must give each expansion its own `static`.
fn call_site_a() -> &'static str {
    zeph_db::sql!("SELECT id FROM messages WHERE conversation_id = ?")
}

/// Distinct call site from [`call_site_a`] despite the identical literal.
fn call_site_b() -> &'static str {
    zeph_db::sql!("SELECT id FROM messages WHERE conversation_id = ?")
}

#[test]
fn sql_macro_caches_per_call_site_not_globally() {
    let a1 = call_site_a();
    let a2 = call_site_a();
    let b1 = call_site_b();

    assert_eq!(a1, "SELECT id FROM messages WHERE conversation_id = $1");
    assert_eq!(a1, b1, "rewritten content must match at both call sites");

    // Same call site, repeated invocation: the `static LazyLock` must return
    // the exact same allocation both times — proves caching actually happens,
    // not just that the rewrite is correct.
    assert!(
        std::ptr::eq(a1.as_ptr(), a2.as_ptr()),
        "same call site must reuse the cached allocation across repeated calls"
    );

    // Two distinct call sites sharing the identical literal: macro hygiene
    // must give each expansion its own `static`, so the allocations must NOT
    // collide (a single shared/global cache would alias them).
    assert!(
        !std::ptr::eq(a1.as_ptr(), b1.as_ptr()),
        "distinct call sites must not share the same cached allocation"
    );
}
