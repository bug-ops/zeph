// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

/// Returns BLAKE3 hex digest of arbitrary bytes.
#[must_use]
pub fn content_hash(data: &[u8]) -> String {
    blake3::hash(data).to_hex().to_string()
}
