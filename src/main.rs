// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

// Thread-local allocation counter used by AllocLayer for per-span heap tracking.
// All unsafe code in the project is confined to this module (global allocator declaration).
#[cfg(feature = "profiling-alloc")]
#[allow(unsafe_code)]
mod alloc_counter {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::RefCell;

    /// Global allocator that records per-thread allocation counts and bytes.
    ///
    /// All allocation and deallocation operations are forwarded unchanged to the
    /// system allocator. The only addition is a thread-local counter update, which
    /// uses `const`-initialised storage to avoid re-entrant allocation on first access.
    pub struct CountingAllocator;

    // SAFETY: All methods delegate to `System` with identical arguments.
    // Thread-local counter updates use `const`-initialised `RefCell` in `.tdata`/`.tbss`,
    // so no dynamic allocation occurs on first thread-local access, eliminating allocator
    // re-entrancy. Borrows are non-overlapping: each function borrow-mutably, completes,
    // and drops the guard before returning.
    unsafe impl GlobalAlloc for CountingAllocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            increment_alloc(layout.size());
            // SAFETY: forwarding to System with the same layout.
            unsafe { System.alloc(layout) }
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            increment_dealloc(layout.size());
            // SAFETY: forwarding to System with the same pointer and layout.
            unsafe { System.dealloc(ptr, layout) }
        }
    }

    #[derive(Clone, Copy)]
    struct RawStats {
        alloc_count: u64,
        alloc_bytes: u64,
        dealloc_count: u64,
        dealloc_bytes: u64,
    }

    impl RawStats {
        const ZERO: Self = Self {
            alloc_count: 0,
            alloc_bytes: 0,
            dealloc_count: 0,
            dealloc_bytes: 0,
        };
    }

    // `const` initialisation places the slot in `.tdata`/`.tbss` — no heap allocation
    // on first access, preventing re-entrant calls back into this allocator.
    thread_local! {
        static STATS: RefCell<RawStats> = const { RefCell::new(RawStats::ZERO) };
    }

    fn increment_alloc(size: usize) {
        STATS.with(|s| {
            let mut s = s.borrow_mut();
            s.alloc_count += 1;
            s.alloc_bytes += size as u64;
        });
    }

    fn increment_dealloc(size: usize) {
        STATS.with(|s| {
            let mut s = s.borrow_mut();
            s.dealloc_count += 1;
            s.dealloc_bytes += size as u64;
        });
    }

    /// Snapshot the current thread's allocation counters.
    ///
    /// Returns `(alloc_count, alloc_bytes, dealloc_count, dealloc_bytes)`.
    /// Counters are monotonically increasing per-thread and are never reset.
    /// `AllocLayer` computes deltas by subtracting the enter snapshot from the exit snapshot.
    pub fn snapshot() -> (u64, u64, u64, u64) {
        STATS.with(|s| {
            let s = s.borrow();
            (
                s.alloc_count,
                s.alloc_bytes,
                s.dealloc_count,
                s.dealloc_bytes,
            )
        })
    }
}

#[cfg(feature = "profiling-alloc")]
#[allow(unsafe_code)]
#[global_allocator]
static GLOBAL: alloc_counter::CountingAllocator = alloc_counter::CountingAllocator;

mod acp;
mod agent_setup;
mod bootstrap;
mod channel;
#[cfg(feature = "otel")]
mod circuit_breaker_exporter;
mod cli;
mod commands;
mod daemon;
mod db_url;
mod execution_mode;
mod gateway_spawn;
mod init;
#[cfg(feature = "prometheus")]
mod metrics_export;
#[cfg(feature = "profiling-pyroscope")]
mod pyroscope_push;
#[cfg(feature = "otel")]
mod redacting_span_processor;
mod runner;
mod scheduler;
#[cfg(feature = "scheduler")]
mod scheduler_executor;
mod startup_checks;
mod tracing_init;
mod tui_bridge;
mod tui_remote;

use clap::Parser;
use cli::Cli;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    Box::pin(runner::run(Cli::parse())).await
}

#[cfg(test)]
mod tests;
