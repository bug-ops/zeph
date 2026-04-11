// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Re-export of injection-detection patterns from `zeph-common` for backwards compatibility.
//!
//! The canonical definitions have moved to `zeph_common::patterns`. Callers that already
//! import from `zeph_tools::patterns` continue to work without changes.

pub use zeph_common::patterns::{
    RAW_INJECTION_PATTERNS, RAW_RESPONSE_PATTERNS, strip_format_chars,
};
