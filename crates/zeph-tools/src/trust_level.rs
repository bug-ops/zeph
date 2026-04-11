// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Re-export of [`SkillTrustLevel`] from `zeph-common` for backwards compatibility.
//!
//! The canonical definition has moved to `zeph_common::SkillTrustLevel`. Callers that
//! already import from `zeph_tools::SkillTrustLevel` continue to work without changes.

pub use zeph_common::SkillTrustLevel;
