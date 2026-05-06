// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

#[cfg(feature = "acp")]
pub(crate) mod acp;
pub(crate) mod agents;
#[cfg(feature = "bench")]
pub(crate) mod bench;
pub(crate) mod classifiers;
pub(crate) mod db;
pub(crate) mod doctor;
#[cfg(feature = "gonka")]
pub(crate) mod gonka;
pub(crate) mod ingest;
pub(crate) mod memory;
pub(crate) mod migrate;
pub(crate) mod plugin;
pub(crate) mod project;
pub(crate) mod router;
#[cfg(feature = "scheduler")]
pub(crate) mod schedule;
#[cfg(all(unix, feature = "scheduler"))]
pub(crate) mod scheduler_daemon;
#[cfg(feature = "acp")]
pub(crate) mod sessions;
pub(crate) mod skill;
pub(crate) mod vault;
