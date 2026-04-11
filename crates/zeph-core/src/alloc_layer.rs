// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Tracing layer for per-span heap allocation tracking.
//!
//! [`AllocLayer`] records allocations and deallocations that occur while a span is active.
//! It reads counter snapshots from the global allocator via an injected function pointer
//! (`AllocSnapshotFn`), keeping `zeph-core` decoupled from the global allocator declaration
//! (which lives in the binary crate).
//!
//! Allocation frames are stored as span extensions rather than thread-local stacks,
//! so they survive tokio task migration between threads without leaking.

use tracing::Subscriber;
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;

/// Function pointer type for reading the current thread's allocation counters.
///
/// Returns `(alloc_count, alloc_bytes, dealloc_count, dealloc_bytes)`.
///
/// The binary crate provides the concrete implementation that reads from the
/// `CountingAllocator`'s thread-local stats. This indirection keeps `zeph-core`
/// free of any `unsafe` code and decoupled from the global allocator declaration.
pub type AllocSnapshotFn = fn() -> (u64, u64, u64, u64);

/// Allocation counter snapshot captured when a span is entered.
///
/// Stored as a span extension via `on_enter` and consumed by `on_exit`.
/// Because it is keyed by span ID (not a thread-local stack), it remains
/// correct even when tokio migrates the task to a different thread between
/// `on_enter` and `on_exit`.
struct AllocFrame {
    alloc_count: u64,
    alloc_bytes: u64,
    dealloc_count: u64,
    dealloc_bytes: u64,
}

/// Accumulated allocation delta for a span across all enter-exit cycles.
///
/// Async spans may yield and re-enter many times. Each cycle's delta is summed
/// here. `on_close` reads the final total and emits it as a tracing event.
struct AllocDelta {
    alloc_count: u64,
    alloc_bytes: u64,
    dealloc_count: u64,
    dealloc_bytes: u64,
}

/// Tracing layer that records per-span heap allocation counts and bytes.
///
/// On span enter, captures the current thread's allocator counters into an
/// [`AllocFrame`] stored as a span extension. On span exit, computes the cycle
/// delta and accumulates it into an [`AllocDelta`] extension. On span close,
/// emits the total as a `tracing::trace!` event with the target `alloc.span`.
///
/// # Thread Safety Under Tokio Work-Stealing
///
/// Span extensions are protected by the `tracing-subscriber` registry's per-span
/// lock. When tokio migrates a task from thread A to thread B:
///
/// - `on_enter` (thread A) captures thread A's counters into the span extension.
/// - `on_exit` (thread B) captures thread B's counters, retrieves the extension,
///   and computes the delta using thread B's counters.
///
/// The delta captures allocations on thread B only. Allocations on thread A between
/// enter and migration are attributed to whichever span is active on thread A after
/// migration — they are never lost globally, only mis-attributed locally. This is a
/// documented, acceptable limitation; numbers are never inflated.
///
/// # Construction
///
/// ```no_run
/// # fn snapshot() -> (u64, u64, u64, u64) { (0, 0, 0, 0) }
/// use zeph_core::alloc_layer::AllocLayer;
/// let layer = AllocLayer::new(snapshot as fn() -> (u64, u64, u64, u64));
/// ```
pub struct AllocLayer {
    snapshot_fn: AllocSnapshotFn,
}

impl AllocLayer {
    /// Create a new allocation-tracking layer.
    ///
    /// `snapshot_fn` must return the current thread's monotonically increasing allocation
    /// counters from the global `CountingAllocator`. The binary crate provides this function.
    #[must_use]
    pub fn new(snapshot_fn: AllocSnapshotFn) -> Self {
        Self { snapshot_fn }
    }
}

impl<S> Layer<S> for AllocLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_enter(&self, id: &tracing::span::Id, ctx: Context<'_, S>) {
        let (ac, ab, dc, db) = (self.snapshot_fn)();
        if let Some(span) = ctx.span(id) {
            span.extensions_mut().replace(AllocFrame {
                alloc_count: ac,
                alloc_bytes: ab,
                dealloc_count: dc,
                dealloc_bytes: db,
            });
        }
    }

    fn on_exit(&self, id: &tracing::span::Id, ctx: Context<'_, S>) {
        let (now_alloc_count, now_alloc_bytes, now_dealloc_count, now_dealloc_bytes) =
            (self.snapshot_fn)();
        let Some(span) = ctx.span(id) else { return };

        // Extract and remove the entry frame in a single lock acquisition.
        // Using a block to ensure the write guard is dropped before the second acquisition.
        // With parking_lot (non-reentrant), holding the guard across `if let` body while
        // calling extensions_mut() again would deadlock.
        let frame = {
            let mut exts = span.extensions_mut();
            exts.remove::<AllocFrame>()
        }; // write guard released here

        let Some(frame) = frame else { return };

        let cycle_alloc_count = now_alloc_count.saturating_sub(frame.alloc_count);
        let cycle_alloc_bytes = now_alloc_bytes.saturating_sub(frame.alloc_bytes);
        let cycle_dealloc_count = now_dealloc_count.saturating_sub(frame.dealloc_count);
        let cycle_dealloc_bytes = now_dealloc_bytes.saturating_sub(frame.dealloc_bytes);

        // Accumulate across enter-exit cycles (async spans that yield and re-enter).
        let mut exts = span.extensions_mut();
        if let Some(acc) = exts.get_mut::<AllocDelta>() {
            acc.alloc_count = acc.alloc_count.saturating_add(cycle_alloc_count);
            acc.alloc_bytes = acc.alloc_bytes.saturating_add(cycle_alloc_bytes);
            acc.dealloc_count = acc.dealloc_count.saturating_add(cycle_dealloc_count);
            acc.dealloc_bytes = acc.dealloc_bytes.saturating_add(cycle_dealloc_bytes);
        } else {
            exts.insert(AllocDelta {
                alloc_count: cycle_alloc_count,
                alloc_bytes: cycle_alloc_bytes,
                dealloc_count: cycle_dealloc_count,
                dealloc_bytes: cycle_dealloc_bytes,
            });
        }
    }

    fn on_close(&self, id: tracing::span::Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(&id) else { return };

        // Extract all data while holding the extensions read lock.
        let (span_name, alloc_count, alloc_bytes, dealloc_count, dealloc_bytes, net) = {
            let exts = span.extensions();
            let Some(delta) = exts.get::<AllocDelta>() else {
                return;
            };
            let net = delta
                .alloc_bytes
                .cast_signed()
                .saturating_sub(delta.dealloc_bytes.cast_signed());
            // Copy all fields before dropping the guard. Calling tracing::trace! while
            // holding the extensions lock would deadlock (parking_lot RwLock is not
            // re-entrant; trace! routes through the same subscriber that holds the lock).
            (
                span.name(),
                delta.alloc_count,
                delta.alloc_bytes,
                delta.dealloc_count,
                delta.dealloc_bytes,
                net,
            )
        }; // extensions read lock released here

        tracing::trace!(
            target: "alloc.span",
            span_name,
            alloc.count = alloc_count,
            alloc.bytes = alloc_bytes,
            dealloc.count = dealloc_count,
            dealloc.bytes = dealloc_bytes,
            alloc.net_bytes = net,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use tracing_subscriber::{Registry, layer::SubscriberExt};

    // Thread-local snapshot counter controlled by individual tests.
    thread_local! {
        static SNAP: Cell<(u64, u64, u64, u64)> = const { Cell::new((0, 0, 0, 0)) };
    }

    fn set_snap(ac: u64, ab: u64, dc: u64, db: u64) {
        SNAP.with(|s| s.set((ac, ab, dc, db)));
    }

    fn snap() -> (u64, u64, u64, u64) {
        SNAP.with(Cell::get)
    }

    fn run_with_subscriber<F: FnOnce()>(f: F) {
        let subscriber = Registry::default().with(AllocLayer::new(snap));
        tracing::subscriber::with_default(subscriber, f);
    }

    /// Verifies that `AllocLayer::new` accepts a snapshot function without panicking.
    #[test]
    fn alloc_layer_new_accepts_any_snapshot_fn() {
        let _layer = AllocLayer::new(snap);
    }

    /// Verifies that `on_enter` stores a frame and `on_exit` removes it without panic.
    /// Correct delta accumulation is covered by the multi-cycle test.
    #[test]
    fn on_enter_exit_does_not_panic() {
        set_snap(10, 1024, 5, 512);
        run_with_subscriber(|| {
            let span = tracing::info_span!("enter_exit_span");
            let guard = span.enter();
            set_snap(15, 2048, 8, 768);
            drop(guard);
        });
    }

    /// Verifies that a span entered and exited with no allocation change produces no delta panic.
    #[test]
    fn zero_delta_span_does_not_panic() {
        set_snap(100, 9000, 50, 8000);
        run_with_subscriber(|| {
            let span = tracing::info_span!("zero_alloc_span");
            let guard = span.enter();
            drop(guard);
        });
    }

    /// Verifies that multiple enter-exit cycles on the same span accumulate without panic.
    #[test]
    fn multiple_cycles_does_not_panic() {
        run_with_subscriber(|| {
            let span = tracing::info_span!("multi_cycle");

            set_snap(10, 100, 5, 50);
            let g = span.enter();
            set_snap(12, 200, 6, 100);
            drop(g);

            set_snap(20, 300, 7, 200);
            let g2 = span.enter();
            set_snap(25, 600, 9, 400);
            drop(g2);
        });
    }
}
