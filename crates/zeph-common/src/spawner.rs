// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Dependency-inversion trait for supervised blocking thread spawns.
//!
//! Crates that cannot depend on `zeph-core` (e.g. `zeph-index`) accept an
//! `Option<Arc<dyn BlockingSpawner>>` and fall back to raw
//! `tokio::task::spawn_blocking` when `None`. This breaks the cyclic
//! dependency that would otherwise arise from `zeph-index` importing
//! `TaskSupervisor` from `zeph-core`.

/// Trait for spawning CPU-bound work on a supervised blocking thread pool.
///
/// Implementors register each spawned task in their supervision layer so it
/// is visible to lifecycle management (snapshots, graceful shutdown, metrics).
/// Callers that do not have a supervised spawner may fall back to
/// `tokio::task::spawn_blocking` directly.
///
/// The trait is object-safe: it accepts a `Box<dyn FnOnce() + Send + 'static>`
/// and returns a `JoinHandle<()>`. Callers that need a typed return value
/// must communicate results through a channel or shared state.
///
/// # Examples
///
/// ```no_run
/// use std::sync::Arc;
/// use zeph_common::BlockingSpawner;
///
/// fn do_work(spawner: Arc<dyn BlockingSpawner>) {
///     let handle = spawner.spawn_blocking_named(Arc::from("my_task"), Box::new(|| {
///         // CPU-bound work
///     }));
///     // Caller can `.await` the handle.
///     let _ = handle;
/// }
/// ```
pub trait BlockingSpawner: Send + Sync + 'static {
    /// Spawn a named blocking closure and return a `JoinHandle<()>` for completion.
    ///
    /// Pass an `Arc<str>` for the task name — this avoids the need to leak memory
    /// when constructing dynamic task names. Static literals can be converted with
    /// `Arc::from("my_task")`.
    ///
    /// The implementation registers the task in its supervision layer before
    /// the closure begins executing. Results must be communicated via channels
    /// or shared state if needed.
    ///
    /// If the closure panics, the implementation should log the error rather than
    /// propagating a panic to the caller. The returned `JoinHandle<()>` resolves
    /// to `Ok(())` in all non-abort cases; it resolves to `Err(JoinError)` only
    /// if the bridge task itself is aborted externally.
    #[must_use]
    fn spawn_blocking_named(
        &self,
        name: std::sync::Arc<str>,
        f: Box<dyn FnOnce() + Send + 'static>,
    ) -> tokio::task::JoinHandle<()>;
}
