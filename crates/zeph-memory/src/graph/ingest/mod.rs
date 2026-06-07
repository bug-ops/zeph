// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Knowledge-ingest subsystem for `zeph-memory`.
//!
//! Provides the idempotency ledger used by `zeph knowledge ingest` to skip unchanged inputs.
//! Phase-2 graph-sink types (`IngestDocument`, adapters, `ingest_documents`) will join this
//! module when implemented.

pub mod ledger;

pub use ledger::{IngestLedger, LedgerEntry};
