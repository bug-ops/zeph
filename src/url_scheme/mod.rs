// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! URL scheme handling for `zeph://` deep links.
//!
//! This module contains platform-specific registration and validation logic for
//! the `zeph://` URI scheme. It is gated behind the `deep-link` feature flag.
//!
//! # Modules
//!
//! - [`validate`] — CWD path validation following INV-CWD from spec §3.
//! - [`register`] — platform-specific scheme registration (Linux, macOS, Windows).

pub mod prompt;
pub mod register;
pub mod validate;
