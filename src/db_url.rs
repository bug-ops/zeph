// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

/// Returns the effective database URL for the current build.
///
/// Prefers `database_url` when set and non-empty (postgres backend), otherwise falls
/// back to `sqlite_path` (sqlite backend). Mirrors the logic in `AppBuilder::build_memory`.
pub(crate) fn resolve_db_url(config: &zeph_core::config::Config) -> &str {
    config
        .memory
        .database_url
        .as_ref()
        .map(zeph_common::secret::Secret::expose)
        .filter(|s| !s.is_empty())
        .unwrap_or(&config.memory.sqlite_path)
}
