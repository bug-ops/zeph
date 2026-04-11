// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Continuous CPU profiling via pprof-rs and HTTP push to the Pyroscope ingest API.
//!
//! This module starts a `pprof::ProfilerGuard` (CPU sampling at 100 Hz using `SIGPROF`)
//! and spawns a background task that periodically serialises the collected profile to the
//! pprof protobuf format and POSTs it to `{endpoint}/ingest`.
//!
//! # SIGPROF notice
//!
//! `pprof-rs` installs a `SIGPROF` signal handler for CPU sampling. Do not register any
//! other `SIGPROF` handlers while this guard is alive; they will conflict and produce
//! incorrect profiles or crashes.
//!
//! # Known Limitations
//!
//! `pprof-rs` uses `SIGPROF` + `_Unwind_Backtrace` for stack unwinding, which is not
//! async-signal-safe. If `SIGPROF` fires while a thread holds an allocator lock (glibc
//! ptmalloc per-arena mutex), stack unwinding may deadlock. This is a known limitation of
//! `pprof-rs` documented in their README. Risk is low in practice (signal must arrive during
//! the narrow allocator-locked window), but cannot be fully eliminated with `SIGPROF`-based
//! sampling. For production systems with strict reliability requirements, consider
//! `perf_event_open`-based profilers that are async-signal-safe.
//!
//! # Trace correlation
//!
//! Profile labels include `service.name` as a session-level tag. Per-span trace-ID
//! labelling is not supported because pprof sampling is asynchronous and cannot be tied
//! to individual tracing spans. Use Grafana's native Tempo-to-Pyroscope time-range
//! linking to correlate traces with CPU profiles.

use pprof::ProfilerGuardBuilder;
use tokio::sync::watch;

const PUSH_INTERVAL_SECS: u64 = 10;
const SAMPLE_FREQUENCY_HZ: i32 = 100;

/// RAII guard that manages the pprof profiler lifecycle and periodic HTTP push to Pyroscope.
///
/// On construction, starts the `pprof::ProfilerGuard` and spawns a background task that
/// pushes profiles every [`PUSH_INTERVAL_SECS`] seconds.
///
/// On drop, signals the background task to stop. Profile data collected after the last
/// push interval may not be flushed — shutdown is best-effort to avoid blocking the process.
pub(crate) struct PyroscopeGuard {
    shutdown_tx: watch::Sender<bool>,
    #[allow(dead_code)]
    handle: tokio::task::JoinHandle<()>,
}

impl Drop for PyroscopeGuard {
    fn drop(&mut self) {
        let _ = self.shutdown_tx.send(true);
        // Do not block on the handle — best-effort shutdown.
    }
}

/// Start continuous profiling and periodic push to the Pyroscope ingest API.
///
/// Builds a `pprof::ProfilerGuard` at [`SAMPLE_FREQUENCY_HZ`] Hz and spawns a background
/// task that collects the profile, encodes it as pprof protobuf, and POSTs it to
/// `{endpoint}/ingest?name={service_name}.cpu{{sampleRate=100}}&format=pprof`.
///
/// Returns `None` if the profiler fails to initialise (error is logged at `error` level).
///
/// # Single-Instance Requirement
///
/// This function must be called at most once per process. Multiple concurrent
/// `PyroscopeGuard`s will conflict on `SIGPROF` registration, producing incorrect
/// profiles or undefined behaviour. Call once at process startup and hold the returned
/// guard for the process lifetime.
///
/// # Errors
///
/// Returns `None` (not `Err`) because profiler availability is best-effort; the agent
/// should continue operating even if profiling is unavailable.
///
/// # Examples
///
/// ```no_run
/// # async fn example() {
/// if let Some(_guard) = zeph::pyroscope_push::start_pyroscope_push(
///     "http://pyroscope:4040",
///     "zeph-agent",
/// ) {
///     // profiler is running; guard keeps it alive
/// }
/// # }
/// ```
pub(crate) fn start_pyroscope_push(endpoint: &str, service_name: &str) -> Option<PyroscopeGuard> {
    let endpoint = endpoint.to_owned();
    let service_name = service_name.to_owned();

    let guard = match ProfilerGuardBuilder::default()
        .frequency(SAMPLE_FREQUENCY_HZ)
        .build()
    {
        Ok(g) => g,
        Err(e) => {
            tracing::error!("failed to start pprof profiler: {e}");
            return None;
        }
    };

    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);

    let handle = tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(PUSH_INTERVAL_SECS));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        // 10s request timeout prevents a slow Pyroscope instance from blocking
        // the push task and missing subsequent intervals.
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .unwrap_or_default();

        loop {
            tokio::select! {
                _ = ticker.tick() => {}
                _ = shutdown_rx.changed() => break,
            }

            push_profile(&guard, &client, &endpoint, &service_name).await;
        }

        tracing::debug!("pyroscope push task shutting down");
    });

    Some(PyroscopeGuard {
        shutdown_tx,
        handle,
    })
}

async fn push_profile(
    guard: &pprof::ProfilerGuard<'static>,
    client: &reqwest::Client,
    endpoint: &str,
    service_name: &str,
) {
    use pprof::protos::Message as _;

    let report = match guard.report().build() {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!("pprof report build failed: {e}");
            return;
        }
    };

    let profile = match report.pprof() {
        Ok(p) => p,
        Err(e) => {
            tracing::debug!("pprof profile encode failed: {e}");
            return;
        }
    };

    let mut body = Vec::new();
    if let Err(e) = profile.encode(&mut body) {
        tracing::debug!("pprof protobuf encode failed: {e}");
        return;
    }

    // Pyroscope ingest URL format with profile type for correct UI categorisation.
    let url = format!(
        "{endpoint}/ingest?name={service_name}.cpu{{sampleRate={SAMPLE_FREQUENCY_HZ}}}&format=pprof"
    );

    match client.post(&url).body(body).send().await {
        Err(e) => tracing::debug!("pyroscope push transport error: {e}"),
        Ok(resp) => {
            if let Err(e) = resp.error_for_status() {
                tracing::warn!("pyroscope push rejected (non-2xx): {e}");
            }
        }
    }
}
