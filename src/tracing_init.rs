// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use zeph_core::config::{LogRotation, LoggingConfig, TelemetryConfig};

/// Guards that must be kept alive for the process lifetime.
///
/// Dropping any guard flushes and closes the corresponding writer.
/// Pass this struct to the top-level `run()` and hold it until the process exits.
// All fields intentionally share the `_guard` postfix to reflect their shared purpose.
#[allow(clippy::struct_field_names)]
pub(crate) struct TracingGuards {
    /// Async file-writer guard for the rolling log file, held in a shared take-once cell.
    /// `None` when file logging is disabled.
    ///
    /// The cell is shared with [`spawn_sigterm_flush_task`] (SIGTERM) and
    /// [`flush_and_exit_on_ctrlc`] (Ctrl-C) so that whichever of the three racing paths —
    /// normal `TracingGuards` drop, a SIGTERM arriving mid-session, or Ctrl-C — runs first
    /// flushes the guard; the other two find `None` and no-op. This is unconditional (unlike
    /// `chrome_guard`) because file logging is not gated behind the `profiling` feature
    /// (generalizes the `chrome_guard` pattern from #6683; see #6693, #6696).
    pub(crate) log_guard: Option<LogGuardCell>,
    /// Chrome trace flush guard, held in a shared take-once cell. `None` when the `profiling`
    /// feature is absent or telemetry is disabled. Dropping the inner guard writes the final
    /// `]` to the JSON trace file.
    ///
    /// The cell is shared with [`spawn_sigterm_flush_task`] (SIGTERM) and
    /// [`flush_and_exit_on_ctrlc`] (Ctrl-C) so that whichever of the three racing paths —
    /// normal `TracingGuards` drop, a SIGTERM arriving mid-session, or Ctrl-C — runs first
    /// flushes the guard; the other two find `None` and no-op (see #6683, #6696).
    #[cfg(feature = "profiling")]
    pub(crate) chrome_guard: Option<ChromeGuardCell>,
    /// Pyroscope push guard. `None` when the `profiling-pyroscope` feature is absent,
    /// telemetry is disabled, or no endpoint is configured.
    /// Dropping this guard signals the background push task to stop.
    #[cfg(feature = "profiling-pyroscope")]
    #[allow(dead_code)]
    pub(crate) pyroscope_guard: Option<crate::pyroscope_push::PyroscopeGuard>,
    /// OTLP tracer provider shutdown handle. `None` when the `otel` feature is absent or
    /// telemetry backend is not `Otlp`. Dropping this guard flushes the `BatchSpanProcessor`
    /// queue and shuts down the provider cleanly.
    #[cfg(feature = "otel")]
    pub(crate) otel_provider: Option<opentelemetry_sdk::trace::SdkTracerProvider>,
}

// Drop order: otel_provider shuts down first (flushes pending spans), then
// take_and_flush_guard_cells takes and flushes log_guard, then chrome_guard, each from its
// shared cell (rather than relying on Arc refcounting or struct-field-declaration-order drop
// glue, since spawn_sigterm_flush_task and flush_and_exit_on_ctrlc each hold their own clone of
// both cells). The log-then-chrome order is not load-bearing — the two writers are independent
// files, so either order is correct — it is simply the order take_and_flush_guard_cells checks
// them in. What IS load-bearing is the explicit take-and-drop itself: once a SIGTERM-flush task
// or the Ctrl-C flush path is live, each holds a second `Arc` clone of both cells, so merely
// dropping `TracingGuards`'s own clone would not run the inner guard's `Drop` (the refcount
// would still be nonzero) — see #6693, #6696.
impl Drop for TracingGuards {
    fn drop(&mut self) {
        #[cfg(feature = "otel")]
        if let Some(provider) = self.otel_provider.take()
            && let Err(e) = provider.shutdown()
        {
            eprintln!("zeph: OTLP provider shutdown error: {e}");
        }
        take_and_flush_guard_cells(
            self.log_guard.as_ref(),
            #[cfg(feature = "profiling")]
            self.chrome_guard.as_ref(),
        );
    }
}

/// Takes each guard out of its shared cell (if still present) and drops it in place, running
/// the writer's flush-on-drop logic. Returns `true` if at least one guard was actually flushed.
///
/// Shared by [`TracingGuards::drop`], [`spawn_sigterm_flush_task`], and
/// [`flush_and_exit_on_ctrlc`] so the load-bearing lock-then-flush pattern is defined exactly
/// once instead of being copy-pasted a third time. For each cell, `locked` is an explicit
/// binding (never the shorthand `drop(cell.lock().take())`) so the lock is held across the
/// flush by ordinary block scoping — this is load-bearing: it serializes this path against
/// whichever other path (SIGTERM flush, Ctrl-C flush, or `TracingGuards::drop`) is racing on the
/// same cell, so neither can ever observe or produce a half-flushed writer. Do NOT split a
/// branch into two statements (`let taken = cell.lock().take(); drop(taken);`) — that releases
/// the lock before the flush runs and reintroduces the truncation bug #6683 fixes (critic
/// finding S2).
fn take_and_flush_guard_cells(
    log_guard_cell: Option<&LogGuardCell>,
    #[cfg(feature = "profiling")] chrome_guard_cell: Option<&ChromeGuardCell>,
) -> bool {
    let mut flushed_something = false;
    if let Some(cell) = log_guard_cell {
        let mut locked = cell.lock();
        if let Some(guard) = locked.take() {
            drop(guard);
            flushed_something = true;
        }
    }
    #[cfg(feature = "profiling")]
    if let Some(cell) = chrome_guard_cell {
        let mut locked = cell.lock();
        if let Some(guard) = locked.take() {
            drop(guard);
            flushed_something = true;
        }
    }
    flushed_something
}

/// Drops `guards` — running [`TracingGuards`]'s take-and-flush `Drop` logic — and then exits
/// with `code`.
///
/// Use this instead of a bare `std::process::exit` at any call site where a `TracingGuards`
/// value is still alive on the stack: `std::process::exit` runs no destructors, so it silently
/// truncates the log file and/or local trace file without this wrapper (#6696).
pub(crate) fn exit_with_flush(guards: TracingGuards, code: i32) -> ! {
    drop(guards);
    std::process::exit(code);
}

/// Flushes whichever of `log_guard_cell` / `chrome_guard_cell` still holds a guard, then exits
/// with `code`.
///
/// Mirrors [`spawn_sigterm_flush_task`]'s flush step but for Ctrl-C (`SIGINT`): unlike SIGTERM,
/// Ctrl-C is handled by `runner.rs`'s `early_ctrlc` task, which previously called
/// `std::process::exit` directly — running no destructors and skipping `TracingGuards::drop`'s
/// flush entirely (#6696). Callers pass clones of the same cells stored in `TracingGuards`, so
/// this races safely against the normal drop path exactly like `spawn_sigterm_flush_task` does.
///
/// Cross-platform: `tokio::signal::ctrl_c()` works on every platform including non-Unix, unlike
/// `spawn_sigterm_flush_task` which is `#[cfg(unix)]`-only — so wiring this into `early_ctrlc`
/// closes the non-Unix Ctrl-C flush gap that `spawn_sigterm_flush_task`'s doc comment used to
/// flag as an open follow-up.
///
/// Does not flush `otel_provider` or `pyroscope_guard` — same scope as
/// `spawn_sigterm_flush_task`, which covers only the file-log and local-trace writers.
pub(crate) fn flush_and_exit_on_ctrlc(
    log_guard_cell: Option<&LogGuardCell>,
    #[cfg(feature = "profiling")] chrome_guard_cell: Option<&ChromeGuardCell>,
    code: i32,
) -> ! {
    take_and_flush_guard_cells(
        log_guard_cell,
        #[cfg(feature = "profiling")]
        chrome_guard_cell,
    );
    std::process::exit(code);
}

/// Resolve the effective log file path from CLI and config sources.
///
/// Priority: CLI `--log-file` > config `logging.file` > disabled (empty string → `None`).
/// An explicit empty CLI value disables file logging even if config has a path.
#[cfg(test)]
fn resolve_log_path(
    cli: Option<&std::path::Path>,
    config_file: &str,
) -> Option<std::path::PathBuf> {
    let file = match cli {
        Some(p) => p.to_string_lossy().into_owned(),
        None => config_file.to_owned(),
    };
    if file.is_empty() {
        None
    } else {
        Some(std::path::PathBuf::from(file))
    }
}

/// Initialise the global tracing subscriber.
///
/// Builds independent layers with separate filters and registers them in a single subscriber:
/// - stderr fmt layer controlled by `RUST_LOG` (default: `info`)
/// - optional file layer controlled by `logging.file` / `logging.level`
/// - optional Chrome JSON trace layer when `profiling` feature is enabled and
///   `telemetry.enabled = true` with `backend = "local"`
/// - optional `MetricsBridge` layer when `profiling` feature is enabled and
///   `metrics_collector` is `Some`
///
/// The CLI override and env vars must already be applied to `logging` before calling.
/// The returned [`TracingGuards`] **must** be held for the entire process lifetime;
/// dropping it flushes all async writers.
///
/// When `tui_mode` is true the stderr layer is omitted because ratatui owns
/// stdout (alternate screen) and any text written to stderr bleeds through
/// raw-mode, corrupting the TUI rendering. Logs still go to the file layer
/// when a log file is configured.
///
/// When `tui_mode` is true and no file log sink is configured, a warning is printed to
/// stderr before the TUI takes over, because the OTLP layer becomes the sole subscriber.
///
/// `owns_sigterm_elsewhere` must be `true` whenever some other code path reached by this
/// invocation will install its own SIGTERM handler and own graceful shutdown — `--daemon`
/// mode, `zeph serve-sessions`, and `zeph scheduler serve` all do (see `src/daemon.rs`,
/// `src/serve/mod.rs`, `src/commands/scheduler_daemon.rs`). The caller computes this from
/// already-parsed CLI/config state before dispatch, since `init_tracing` runs before the
/// subcommand match. Passing `false` for a path that in fact owns SIGTERM itself would let the
/// SIGTERM flush task (installed by [`spawn_sigterm_flush_task`] — covers the file-log writer
/// unconditionally, plus the local Chrome trace writer when the `profiling` feature is enabled)
/// race and beat that path's graceful drain with a hard exit — this is exactly critic finding
/// C1 for #6683, and the same hazard applies to the file-log writer (#6693).
#[allow(clippy::too_many_lines)]
pub(crate) fn init_tracing(
    logging: &LoggingConfig,
    runtime_ctx: zeph_core::RuntimeContext,
    telemetry: &TelemetryConfig,
    redact_secrets: bool,
    // When true (`--json` mode), the stderr fmt layer is suppressed so no human-readable
    // text can interleave with the machine-readable JSONL stream on stdout.
    json_mode: bool,
    owns_sigterm_elsewhere: bool,
    #[cfg(feature = "profiling")] metrics_collector: Option<
        std::sync::Arc<zeph_core::metrics::MetricsCollector>,
    >,
) -> TracingGuards {
    // Type alias for a boxed dynamic layer to allow composing heterogeneous layer types.
    type BoxedLayer =
        Box<dyn tracing_subscriber::Layer<tracing_subscriber::Registry> + Send + Sync + 'static>;

    let mut layers: Vec<BoxedLayer> = Vec::new();

    // Determine whether OTLP will be the active trace sink.
    // When the `otel` feature is absent, OTLP is never active.
    #[cfg(feature = "otel")]
    let otlp_active = {
        use zeph_core::config::TelemetryBackend;
        telemetry.enabled && telemetry.backend == TelemetryBackend::Otlp
    };
    #[cfg(not(feature = "otel"))]
    let otlp_active = false;

    // Stderr layer — omitted in TUI mode (corrupts raw-mode rendering) and in JSON mode
    // (keeps stderr clean so `--json | jq` works without non-JSON lines).
    if !runtime_ctx.tui_mode && !json_mode {
        let stderr_filter = tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
        layers.push(Box::new(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stderr)
                .with_filter(stderr_filter),
        ));
    }

    // Optional file layer.
    let mut log_guard: Option<WorkerGuard> = None;

    // In TUI mode the stderr layer is suppressed to avoid corrupting raw-mode rendering.
    // If logging.file was explicitly set to "" and no OTLP is configured the process would run
    // completely silent. Activate the platform default log path so traces are always reachable.
    if runtime_ctx.tui_mode && logging.file.is_empty() && !otlp_active {
        let fallback_path = std::path::PathBuf::from(zeph_core::config::default_log_file_path());
        let log_dir = fallback_path.parent().map_or_else(
            || std::path::PathBuf::from("."),
            std::path::Path::to_path_buf,
        );
        let filename = fallback_path.file_name().map_or_else(
            || "zeph.log".to_owned(),
            |n| n.to_string_lossy().into_owned(),
        );
        if let Err(e) = std::fs::create_dir_all(&log_dir) {
            eprintln!(
                "[zeph] warning: could not create fallback log directory {}: {e}",
                log_dir.display()
            );
        } else {
            let file_appender = tracing_appender::rolling::never(&log_dir, &filename);
            let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);
            let fallback_filter = tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
            layers.push(Box::new(
                tracing_subscriber::fmt::layer()
                    .with_writer(non_blocking)
                    .with_ansi(false)
                    .with_filter(fallback_filter),
            ));
            log_guard = Some(guard);
            eprintln!(
                "[zeph] info: TUI mode: no log sink configured, falling back to {}",
                fallback_path.display()
            );
        }
    }
    if !logging.file.is_empty() {
        let path = std::path::PathBuf::from(&logging.file);
        let dir = path.parent().map_or_else(
            || std::path::PathBuf::from("."),
            std::path::Path::to_path_buf,
        );
        let filename_prefix = path
            .file_stem()
            .map_or_else(|| "zeph".to_owned(), |s| s.to_string_lossy().into_owned());
        let filename_suffix = path
            .extension()
            .map_or_else(|| "log".to_owned(), |s| s.to_string_lossy().into_owned());

        if let Err(e) = std::fs::create_dir_all(&dir) {
            if !runtime_ctx.tui_mode {
                eprintln!("zeph: log directory creation failed, file logging disabled: {e}");
            }
        } else {
            let rotation = match logging.rotation {
                LogRotation::Daily => Rotation::DAILY,
                LogRotation::Hourly => Rotation::HOURLY,
                _ => Rotation::NEVER,
            };
            match RollingFileAppender::builder()
                .rotation(rotation)
                .max_log_files(logging.max_files)
                .filename_prefix(&filename_prefix)
                .filename_suffix(&filename_suffix)
                .build(&dir)
            {
                Err(e) => {
                    if !runtime_ctx.tui_mode {
                        eprintln!(
                            "zeph: log file appender init failed, file logging disabled: {e}"
                        );
                    }
                }
                Ok(appender) => {
                    let (non_blocking, guard) = tracing_appender::non_blocking(appender);
                    let file_filter = tracing_subscriber::EnvFilter::try_new(&logging.level)
                        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
                    layers.push(Box::new(
                        tracing_subscriber::fmt::layer()
                            .with_writer(non_blocking)
                            .with_ansi(false)
                            .with_filter(file_filter),
                    ));
                    log_guard = Some(guard);
                }
            }
        }
    }

    // Optional Chrome JSON trace layer (compiled in only with the profiling feature).
    #[cfg(feature = "profiling")]
    let chrome_guard = build_chrome_layer(telemetry, &mut layers);

    // Wrap log_guard in a shared take-once cell so the SIGTERM flush task below and the normal
    // `TracingGuards::drop` path can race safely for the file-log writer, mirroring
    // `chrome_guard`'s `ChromeGuardCell` pattern (see `TracingGuards::drop`'s doc comment).
    let log_guard: Option<LogGuardCell> =
        log_guard.map(|guard| std::sync::Arc::new(parking_lot::Mutex::new(Some(guard))));

    // Installing the SIGTERM flush task is a process-global side effect (it registers a signal
    // listener that overrides SIGTERM's default disposition for the whole process) — kept
    // visible here at the process-level function rather than buried inside a helper (critic
    // finding M2). See `should_install_sigterm_flush_task`'s doc comment for the decision logic
    // (critic finding S3: this gate is unconditional, not `profiling`-gated, so it is unit-tested).
    #[cfg(feature = "profiling")]
    let has_guard_to_flush = log_guard.is_some() || chrome_guard.is_some();
    #[cfg(not(feature = "profiling"))]
    let has_guard_to_flush = log_guard.is_some();
    if should_install_sigterm_flush_task(owns_sigterm_elsewhere, has_guard_to_flush) {
        spawn_sigterm_flush_task(
            log_guard.clone(),
            #[cfg(feature = "profiling")]
            chrome_guard.clone(),
        );
    }

    // Optional OTLP gRPC trace layer — active only when the `otel` feature is compiled in
    // AND `telemetry.backend == Otlp`. Layers are mutually selected by backend variant:
    // `build_chrome_layer` returns None for non-Local backends; `build_otlp_layer` activates
    // only for Otlp. Both can coexist in the layer vec without conflict.
    #[cfg(feature = "otel")]
    let otel_provider = build_otlp_layer(telemetry, &mut layers, true, redact_secrets);

    // Suppress unused warning when otel feature is inactive.
    #[cfg(not(feature = "otel"))]
    let _ = redact_secrets;

    // Optional MetricsBridge layer — derives TurnTimings from span durations.
    #[cfg(feature = "profiling")]
    if let Some(collector) = metrics_collector {
        layers.push(Box::new(zeph_core::metrics_bridge::MetricsBridge::new(
            collector,
        )));
    }

    // Optional AllocLayer — records per-span heap allocation counts and bytes.
    // Reads thread-local counters from CountingAllocator via the snapshot function pointer.
    #[cfg(feature = "profiling-alloc")]
    if telemetry.enabled {
        layers.push(Box::new(zeph_core::alloc_layer::AllocLayer::new(
            crate::alloc_counter::snapshot,
        )));
    }

    // Suppress unused warning when neither profiling nor otel features are active.
    #[cfg(not(any(feature = "profiling", feature = "otel")))]
    let _ = telemetry;

    tracing_subscriber::registry().with(layers).init();

    // Start Pyroscope continuous profiling push (after subscriber init so tracing works).
    #[cfg(feature = "profiling-pyroscope")]
    let pyroscope_guard = if telemetry.enabled {
        telemetry
            .pyroscope_endpoint
            .as_deref()
            .and_then(|ep| crate::pyroscope_push::start_pyroscope_push(ep, &telemetry.service_name))
    } else {
        None
    };

    TracingGuards {
        log_guard,
        #[cfg(feature = "profiling")]
        chrome_guard,
        #[cfg(feature = "profiling-pyroscope")]
        pyroscope_guard,
        #[cfg(feature = "otel")]
        otel_provider,
    }
}

/// Shared take-once cell holding the file-log [`WorkerGuard`].
///
/// Cloned between [`TracingGuards::log_guard`](TracingGuards) and, when nothing else on the
/// current invocation's path already owns SIGTERM, the task spawned by
/// [`spawn_sigterm_flush_task`] — whichever path runs first takes the guard and drops it,
/// flushing pending log lines to the file; the other finds `None` and no-ops. Mirrors
/// `ChromeGuardCell` but is unconditional: file logging is not gated behind the `profiling`
/// feature (see #6693). Not an intra-doc link because `ChromeGuardCell` is itself
/// `#[cfg(feature = "profiling")]`-gated and the default/CI feature set does not enable it.
type LogGuardCell = std::sync::Arc<parking_lot::Mutex<Option<WorkerGuard>>>;

/// Shared take-once cell holding the Chrome trace [`FlushGuard`](tracing_chrome::FlushGuard).
///
/// Cloned between [`TracingGuards::chrome_guard`](TracingGuards) and, when nothing else on the
/// current invocation's path already owns SIGTERM, the task spawned by
/// [`spawn_sigterm_flush_task`] — whichever path runs first takes the guard and drops it,
/// closing the trace file's JSON array; the other finds `None` and no-ops.
#[cfg(feature = "profiling")]
type ChromeGuardCell = std::sync::Arc<parking_lot::Mutex<Option<tracing_chrome::FlushGuard>>>;

/// Build the Chrome JSON trace layer and append it to `layers`.
///
/// Returns a shared cell wrapping the `FlushGuard`, which must be held (via
/// [`TracingGuards`]) until process exit. Returns `None` when telemetry is disabled or backend
/// is not `Local`.
///
/// This function only constructs the layer and the guard cell — it does not decide whether to
/// install the SIGTERM flush handler; see [`init_tracing`]'s `owns_sigterm_elsewhere` parameter
/// and [`spawn_sigterm_flush_task`] for that (critic finding M2: that decision is a
/// process-global side effect and belongs at the process-level call site, not hidden here).
#[cfg(feature = "profiling")]
fn build_chrome_layer(
    telemetry: &TelemetryConfig,
    layers: &mut Vec<
        Box<dyn tracing_subscriber::Layer<tracing_subscriber::Registry> + Send + Sync + 'static>,
    >,
) -> Option<ChromeGuardCell> {
    use zeph_core::config::TelemetryBackend;

    if !telemetry.enabled {
        return None;
    }

    if telemetry.backend == TelemetryBackend::Pyroscope {
        tracing::warn!(
            "telemetry backend 'pyroscope' is not yet implemented (Phase 4); no traces will be written"
        );
        return None;
    }

    if telemetry.backend != TelemetryBackend::Local {
        return None;
    }

    if let Err(e) = std::fs::create_dir_all(&telemetry.trace_dir) {
        eprintln!(
            "zeph: failed to create trace directory {}: {e}",
            telemetry.trace_dir.display()
        );
        return None;
    }

    let session_id = uuid::Uuid::new_v4().simple();
    let timestamp = chrono::Utc::now().format("%Y%m%dT%H%M%S");
    let filename = format!("{session_id}_{timestamp}.json");
    let trace_path = telemetry.trace_dir.join(filename);

    // TraceStyle::Async records on_new_span/on_close once per span lifetime (true wall-clock
    // duration) instead of Threaded's on_enter/on_exit, which fire on every poll of an async
    // span — fragmenting any span with an internal `.await` into hundreds of short on-CPU
    // slices instead of one continuous duration (#6682). The paired `b`/`e` events this
    // produces share an `id` (the span tree's root) rather than pairing complete "X" events;
    // see the reworked jq recipes in `.claude/rules/continuous-improvement.md`.
    let (chrome_layer, guard) = tracing_chrome::ChromeLayerBuilder::new()
        .file(trace_path)
        .include_args(telemetry.include_args)
        .trace_style(tracing_chrome::TraceStyle::Async)
        .build();

    let guard_cell: ChromeGuardCell = std::sync::Arc::new(parking_lot::Mutex::new(Some(guard)));

    layers.push(Box::new(chrome_layer));
    Some(guard_cell)
}

/// Decides whether `init_tracing` should call [`spawn_sigterm_flush_task`].
///
/// `true` only when both hold: nothing else on this invocation's path already owns SIGTERM
/// (`owns_sigterm_elsewhere` is `false` — see [`init_tracing`]'s parameter doc, critic finding
/// C1), and there is at least one guard cell worth flushing (`has_guard_to_flush`) — installing
/// the task when neither cell holds a guard would needlessly override SIGTERM's default
/// disposition for no benefit. Extracted as a pure function (critic finding S3) because this gate
/// is unconditional (not `profiling`-gated) as of #6693, so a mis-wire would silently regress
/// graceful shutdown in every default/CI build, not just `profiling` builds.
fn should_install_sigterm_flush_task(
    owns_sigterm_elsewhere: bool,
    has_guard_to_flush: bool,
) -> bool {
    !owns_sigterm_elsewhere && has_guard_to_flush
}

/// Installs a SIGTERM handler that flushes pending log/trace writers and then re-raises SIGTERM.
///
/// Registering a `tokio::signal::unix::signal` listener replaces SIGTERM's default
/// terminate-immediately disposition, so once installed, this task becomes responsible for
/// actually ending the process. It does so by flushing whichever of `log_guard_cell` /
/// `chrome_guard_cell` still holds a guard (bounded: each guard's `Drop` blocks briefly on its
/// writer thread's join) and then calling [`signal_hook::low_level::emulate_default_handler`],
/// which resets SIGTERM's disposition back to `SIG_DFL` and re-raises it — the process therefore
/// still dies *by the signal* (`WIFSIGNALED`, `WTERMSIG == SIGTERM`), matching what external
/// supervisors (systemd `SuccessExitStatus`/`Restart=on-failure`, launchd, container runtimes,
/// `zeph scheduler stop`'s wait loop) expect from an unhandled `SIGTERM`, rather than a plain
/// `exit(143)` (`WIFEXITED`) that reads identically via `$?` but is distinguishable via
/// `wait`/`waitpid` (critic finding S3, point 1, originally for #6683). If
/// `emulate_default_handler` itself errors (documented as occurring only for an unrecognized
/// signal, which `SIGTERM` never is), `std::process::exit` is used as a fallback so the process
/// still terminates rather than hanging forever.
///
/// `log_guard_cell` covers the file-log writer unconditionally (#6693); `chrome_guard_cell` is
/// only present when the `profiling` feature is enabled. Both cells use `Mutex<Option<_>>::take`,
/// which is idempotent, so this task and `TracingGuards::drop` racing on the same cell(s) can
/// never double-flush or observe a half-flushed state.
///
/// **Caller must not install this task for a path that owns SIGTERM itself** — see
/// [`init_tracing`]'s `owns_sigterm_elsewhere` parameter (critic finding C1).
///
/// Known limitation, not fixable by this task (critic finding S3, point 2): the
/// `tokio::signal::unix::signal` registration installs a process-global handler that outlives
/// this task and is never explicitly removed. If the tokio runtime cannot schedule this task —
/// e.g. every worker thread is blocked in CPU-bound or blocking synchronous work — the process
/// cannot react to `SIGTERM` at all until scheduling resumes, and `SIGKILL` is required instead.
/// This directly affects the project's own `pkill -f "target/.*zeph"` live-testing teardown habit
/// (`.claude/rules/continuous-improvement.md`) in that specific pathological case.
///
/// Unix only: `pkill`'s default signal (the reproduction path for #6683/#6693) is a Unix concept:
/// on non-Unix platforms this is a no-op.
///
/// Ctrl-C is handled separately by `early_ctrlc` (`src/runner.rs`), which now calls
/// [`flush_and_exit_on_ctrlc`] instead of a bare `std::process::exit` (#6696), so both signals
/// flush `log_guard_cell`/`chrome_guard_cell` before the process terminates — including on
/// non-Unix platforms, where this SIGTERM task is a no-op but the Ctrl-C path still runs since
/// `tokio::signal::ctrl_c()` is cross-platform.
#[cfg(unix)]
fn spawn_sigterm_flush_task(
    log_guard_cell: Option<LogGuardCell>,
    #[cfg(feature = "profiling")] chrome_guard_cell: Option<ChromeGuardCell>,
) {
    let fut = async move {
        let Ok(mut sigterm) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        else {
            return;
        };
        sigterm.recv().await;
        let flushed_something = take_and_flush_guard_cells(
            log_guard_cell.as_ref(),
            #[cfg(feature = "profiling")]
            chrome_guard_cell.as_ref(),
        );
        if flushed_something {
            eprintln!("[zeph] received SIGTERM: flushed pending log/trace writers before exit");
        }
        if let Err(e) =
            signal_hook::low_level::emulate_default_handler(signal_hook::consts::SIGTERM)
        {
            eprintln!(
                "[zeph] warning: failed to re-raise SIGTERM after flush ({e}); exiting directly"
            );
            std::process::exit(143); // 128 + SIGTERM(15): conventional signal-termination exit code
        }
    };
    tokio::spawn(fut); // EXEMPT(#5143): TaskSupervisor is not yet constructed at tracing-init time
}

#[cfg(not(unix))]
fn spawn_sigterm_flush_task(
    _log_guard_cell: Option<LogGuardCell>,
    #[cfg(feature = "profiling")] _chrome_guard_cell: Option<ChromeGuardCell>,
) {
}

/// Build the OTLP gRPC trace layer and append it to `layers`.
///
/// Returns the `SdkTracerProvider` shutdown handle (stored in [`TracingGuards`]) or `None`
/// when telemetry is disabled or `telemetry.backend` is not `"otlp"`.
///
/// The `set_global` parameter controls whether `opentelemetry::global::set_tracer_provider` is
/// called. Pass `true` in production (`init_tracing`) and `false` in tests to avoid polluting
/// the global state and leaking `BatchSpanProcessor` background tasks.
///
/// The `redact_secrets` parameter controls whether a `RedactingSpanProcessor` wrapper is
/// inserted between the BSP and the exporter to scrub string attribute values before export.
///
// RESERVED_KEYS lists resource attribute keys managed by the builder itself.
// Only "service.name" is listed because Resource::builder_empty() (not builder()) does not
// attach SDK-detected attributes. If the project later switches to Resource::builder(),
// this list must be extended with "service.version", "telemetry.sdk.*", etc.
#[cfg(feature = "otel")]
const RESERVED_KEYS: &[&str] = &["service.name"];

/// Returns the subset of `metadata` whose keys are not in `RESERVED_KEYS`.
///
/// Keys in `RESERVED_KEYS` are printed via `eprintln!` and excluded.
/// Used by `build_otlp_layer` to build OTLP resource attributes.
#[cfg(feature = "otel")]
fn filter_trace_metadata(
    metadata: &std::collections::HashMap<String, String>,
) -> Vec<(String, String)> {
    metadata
        .iter()
        .filter_map(|(k, v)| {
            if RESERVED_KEYS.contains(&k.as_str()) {
                eprintln!(
                    "[zeph] warning: telemetry.trace_metadata key '{k}' is reserved and will be ignored"
                );
                None
            } else {
                Some((k.clone(), v.clone()))
            }
        })
        .collect()
}

/// # Panics
///
/// Does not panic. OTLP pipeline errors are logged via `tracing::warn!` and `None` is returned.
#[cfg(feature = "otel")]
#[allow(clippy::too_many_lines)]
fn build_otlp_layer(
    telemetry: &TelemetryConfig,
    layers: &mut Vec<
        Box<dyn tracing_subscriber::Layer<tracing_subscriber::Registry> + Send + Sync + 'static>,
    >,
    set_global: bool,
    redact_secrets: bool,
) -> Option<opentelemetry_sdk::trace::SdkTracerProvider> {
    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry_otlp::{SpanExporter, WithExportConfig as _};
    use opentelemetry_sdk::trace::{
        BatchConfigBuilder, BatchSpanProcessor, Sampler, SdkTracerProvider,
    };
    use tracing_subscriber::EnvFilter;
    use zeph_core::config::TelemetryBackend;

    if !telemetry.enabled || telemetry.backend != TelemetryBackend::Otlp {
        return None;
    }

    // All tracing::warn! calls inside this function fire before the subscriber is initialized
    // (subscriber.init() is called in init_tracing after this function returns). Use eprintln!
    // so diagnostic messages are not silently dropped.
    if telemetry.otlp_headers_vault_key.is_some() {
        eprintln!(
            "[zeph] warning: telemetry.otlp_headers_vault_key is set but not yet wired; \
             OTLP exporter connects unauthenticated"
        );
    }

    let endpoint = telemetry
        .otlp_endpoint
        .as_deref()
        .unwrap_or("http://localhost:4317");

    // #3001: warn when OTLP endpoint uses plaintext HTTP on a non-local host.
    if let Ok(url) = endpoint.parse::<url::Url>() {
        let host = url.host_str();
        // url::Url::host_str() returns IPv6 addresses with brackets: "[::1]".
        let is_local = host.is_none()
            || host == Some("localhost")
            || host == Some("127.0.0.1")
            || host == Some("[::1]");
        if url.scheme() == "http" && !is_local {
            eprintln!(
                "[zeph] warning: OTLP endpoint {endpoint} uses plaintext HTTP on a non-local host; \
                 consider using https:// to protect span data in transit"
            );
        }
    }

    let sample_rate = {
        let r = telemetry.sample_rate;
        if (0.0..=1.0).contains(&r) {
            r
        } else {
            let clamped = r.clamp(0.0, 1.0);
            eprintln!(
                "[zeph] warning: telemetry.sample_rate {r} is outside [0.0, 1.0]; clamping to {clamped}"
            );
            clamped
        }
    };

    // #2996: set a 3-second export timeout so the process does not block indefinitely
    // when the OTLP collector is unreachable.
    let exporter = match SpanExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint)
        .with_timeout(std::time::Duration::from_secs(3))
        .build()
    {
        Ok(e) => e,
        Err(e) => {
            eprintln!("[zeph] warning: OTLP exporter init failed, tracing disabled: {e}");
            return None;
        }
    };

    // "service.name" is the canonical OTel semconv key (opentelemetry_semantic_conventions::resource::SERVICE_NAME).
    // We inline the string to avoid a new dependency on that crate.
    let mut resource_builder = opentelemetry_sdk::Resource::builder_empty()
        .with_service_name(telemetry.service_name.clone());
    for (k, v) in filter_trace_metadata(&telemetry.trace_metadata) {
        resource_builder = resource_builder.with_attribute(opentelemetry::KeyValue::new(k, v));
    }
    let resource = resource_builder.build();

    // #2998: raise BSP queue size from the default 2048 to 4096 to absorb bursts during
    // high-throughput agent turns without dropping spans. This directly addresses the
    // CPU/RAM regression caused by unfiltered OTLP span creation (#2996).
    let batch_config = BatchConfigBuilder::default()
        .with_max_queue_size(4096)
        .build();
    // #3011: wrap with a circuit breaker to prevent CPU burn when the OTLP collector
    // is unavailable. After 3 consecutive export failures the circuit opens and spans
    // are silently dropped until the back-off window expires.
    let exporter = crate::circuit_breaker_exporter::CircuitBreakerExporter::new(exporter);
    let bsp = BatchSpanProcessor::builder(exporter)
        .with_batch_config(batch_config)
        .build();

    // #2999: optionally wrap BSP with a redacting processor to scrub string attributes.
    // Two builder paths avoid the `Box<dyn SpanProcessor>` indirection since
    // `SdkTracerProvider::with_span_processor` requires a concrete type bound.
    let provider = if redact_secrets {
        let redacting = crate::redacting_span_processor::RedactingSpanProcessor::new(bsp);
        SdkTracerProvider::builder()
            .with_span_processor(redacting)
            .with_sampler(Sampler::TraceIdRatioBased(sample_rate))
            .with_resource(resource)
            .build()
    } else {
        SdkTracerProvider::builder()
            .with_span_processor(bsp)
            .with_sampler(Sampler::TraceIdRatioBased(sample_rate))
            .with_resource(resource)
            .build()
    };

    if set_global {
        opentelemetry::global::set_tracer_provider(provider.clone());
    }

    // #2996: attach an EnvFilter to the OTLP layer to suppress transport-layer spans
    // (tonic, tower, hyper, h2, opentelemetry internal) from feeding back into the exporter,
    // which was the root cause of the 100% CPU / 20 GB RAM regression in TUI mode.
    let base = telemetry.otel_filter.as_deref().unwrap_or("info");
    let filter_str = format!(
        "{base},tonic=warn,tower=warn,hyper=warn,h2=warn,\
         opentelemetry=warn,rmcp=warn,sqlx=warn,want=warn"
    );
    let otel_filter = EnvFilter::builder()
        .with_default_directive(tracing::Level::INFO.into())
        .parse_lossy(&filter_str);

    let tracer = provider.tracer(telemetry.service_name.clone());
    layers.push(Box::new(
        tracing_opentelemetry::layer()
            .with_tracer(tracer)
            .with_filter(otel_filter),
    ));

    Some(provider)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify the full truth table of `should_install_sigterm_flush_task` (critic finding S3):
    /// the gate is now unconditional (not `profiling`-gated), so a mis-wire here would silently
    /// regress graceful shutdown in every default/CI build. The task must be installed if and
    /// only if nothing else owns SIGTERM AND there is at least one guard cell to flush.
    #[test]
    fn should_install_sigterm_flush_task_truth_table() {
        assert!(
            should_install_sigterm_flush_task(false, true),
            "must install: nothing else owns SIGTERM, and there is a guard to flush"
        );
        assert!(
            !should_install_sigterm_flush_task(true, true),
            "must not install: another path (e.g. --daemon) already owns SIGTERM"
        );
        assert!(
            !should_install_sigterm_flush_task(false, false),
            "must not install: nothing to flush, no need to override SIGTERM's disposition"
        );
        assert!(
            !should_install_sigterm_flush_task(true, false),
            "must not install: neither condition is met"
        );
    }

    #[test]
    fn resolve_log_path_no_cli_empty_config_returns_none() {
        assert!(resolve_log_path(None, "").is_none());
    }

    #[test]
    fn resolve_log_path_no_cli_config_set_returns_config_path() {
        let result = resolve_log_path(None, ".zeph/logs/zeph.log");
        assert_eq!(
            result.as_deref(),
            Some(std::path::Path::new(".zeph/logs/zeph.log"))
        );
    }

    #[test]
    fn resolve_log_path_cli_empty_disables_logging() {
        // Explicit empty CLI value overrides even a non-empty config.
        let result = resolve_log_path(Some(std::path::Path::new("")), ".zeph/logs/zeph.log");
        assert!(result.is_none());
    }

    #[test]
    fn resolve_log_path_cli_path_overrides_config() {
        let result = resolve_log_path(
            Some(std::path::Path::new("/tmp/custom.log")),
            ".zeph/logs/zeph.log",
        );
        assert_eq!(
            result.as_deref(),
            Some(std::path::Path::new("/tmp/custom.log"))
        );
    }

    /// Verify that `build_otlp_layer` returns `None` when telemetry is disabled, regardless of
    /// the backend setting, and that no layers are appended.
    #[cfg(feature = "otel")]
    #[test]
    fn build_otlp_layer_disabled_returns_none() {
        use zeph_core::config::{TelemetryBackend, TelemetryConfig};
        let telemetry = TelemetryConfig {
            enabled: false,
            backend: TelemetryBackend::Otlp,
            ..TelemetryConfig::default()
        };
        let mut layers: Vec<
            Box<dyn tracing_subscriber::Layer<tracing_subscriber::Registry> + Send + Sync>,
        > = Vec::new();
        let provider = build_otlp_layer(&telemetry, &mut layers, false, false);
        assert!(
            provider.is_none(),
            "expected None when telemetry is disabled"
        );
        assert!(
            layers.is_empty(),
            "no layer should be appended when disabled"
        );
    }

    /// Verify that `build_otlp_layer` returns `None` when the backend is not Otlp.
    #[cfg(feature = "otel")]
    #[test]
    fn build_otlp_layer_non_otlp_backend_returns_none() {
        use zeph_core::config::{TelemetryBackend, TelemetryConfig};
        let telemetry = TelemetryConfig {
            enabled: true,
            backend: TelemetryBackend::Local,
            ..TelemetryConfig::default()
        };
        let mut layers: Vec<
            Box<dyn tracing_subscriber::Layer<tracing_subscriber::Registry> + Send + Sync>,
        > = Vec::new();
        let provider = build_otlp_layer(&telemetry, &mut layers, false, false);
        assert!(provider.is_none(), "expected None when backend is not Otlp");
        assert!(layers.is_empty(), "no layer should be appended");
    }

    /// Verify that the `sample_rate` clamp expression correctly bounds values to `[0.0, 1.0]`.
    /// The clamp logic runs before the network exporter is built — no live collector required.
    #[cfg(feature = "otel")]
    #[test]
    #[allow(clippy::float_cmp)]
    fn build_otlp_layer_sample_rate_out_of_range_is_clamped() {
        let clamp = |r: f64| {
            if (0.0..=1.0).contains(&r) {
                r
            } else {
                r.clamp(0.0, 1.0)
            }
        };
        assert_eq!(clamp(50.0), 1.0, "value > 1.0 must clamp to 1.0");
        assert_eq!(clamp(-0.5), 0.0, "negative value must clamp to 0.0");
        assert_eq!(
            clamp(0.5),
            0.5,
            "in-range value must pass through unchanged"
        );
        assert_eq!(clamp(0.0), 0.0, "boundary 0.0 must pass through unchanged");
        assert_eq!(clamp(1.0), 1.0, "boundary 1.0 must pass through unchanged");
    }

    /// Verify that the OTLP `EnvFilter` string is constructed correctly and suppresses
    /// transport-layer crates at `warn` level.
    ///
    /// Tests the filter construction logic from `build_otlp_layer` without requiring a live
    /// OTLP collector. Both the absence of parse errors and the presence of every exclusion
    /// directive are verified.
    ///
    /// Background: the absence of this filter was the root cause of the 100% CPU / 20 GB RAM
    /// regression in TUI mode (issue #2996). The feedback loop occurred because tonic/tower/hyper
    /// spans emitted during export were themselves captured by the OTLP layer.
    #[test]
    fn otlp_filter_suppresses_transport_crates() {
        use tracing_subscriber::EnvFilter;

        let base = "info";
        let filter_str = format!(
            "{base},tonic=warn,tower=warn,hyper=warn,h2=warn,\
             opentelemetry=warn,rmcp=warn,sqlx=warn,want=warn"
        );

        // Filter must parse without error.
        let filter = EnvFilter::builder()
            .with_default_directive(tracing::Level::INFO.into())
            .parse_lossy(&filter_str);

        // Verify all required exclusions are present in the formatted filter.
        let filter_repr = format!("{filter}");
        for crate_name in &[
            "tonic",
            "tower",
            "hyper",
            "h2",
            "opentelemetry",
            "rmcp",
            "sqlx",
            "want",
        ] {
            assert!(
                filter_repr.contains(crate_name),
                "filter missing exclusion for '{crate_name}': {filter_repr}"
            );
        }
    }

    /// Verify that the OTLP filter correctly merges a custom base directive with the hardcoded
    /// transport exclusions, and that the custom directive is preserved.
    #[test]
    fn otlp_filter_custom_base_preserved() {
        use tracing_subscriber::EnvFilter;

        let base = "debug,myapp=trace";
        let filter_str = format!(
            "{base},tonic=warn,tower=warn,hyper=warn,h2=warn,\
             opentelemetry=warn,rmcp=warn,sqlx=warn,want=warn"
        );

        // Must parse without panic even with a complex base.
        let filter = EnvFilter::builder()
            .with_default_directive(tracing::Level::INFO.into())
            .parse_lossy(&filter_str);

        let filter_repr = format!("{filter}");
        assert!(
            filter_repr.contains("tonic"),
            "tonic=warn must be present: {filter_repr}"
        );
        assert!(
            filter_repr.contains("myapp"),
            "custom base directive must be preserved: {filter_repr}"
        );
    }

    /// Verify the plaintext HTTP endpoint warning predicate used in `build_otlp_layer`.
    ///
    /// Tests the URL classification logic (local vs non-local, http vs https) that determines
    /// whether the `eprintln!` warning for unencrypted OTLP transport is emitted.
    #[test]
    fn plaintext_http_warning_predicate() {
        // Helper that mirrors the classification logic in build_otlp_layer.
        let should_warn = |endpoint: &str| -> bool {
            if let Ok(url) = endpoint.parse::<url::Url>() {
                let host = url.host_str();
                // url::Url::host_str() returns IPv6 addresses with brackets: "[::1]".
                let is_local = host.is_none()
                    || host == Some("localhost")
                    || host == Some("127.0.0.1")
                    || host == Some("[::1]");
                url.scheme() == "http" && !is_local
            } else {
                false
            }
        };

        // Local addresses must not warn even with http.
        assert!(
            !should_warn("http://localhost:4317"),
            "localhost http must not warn"
        );
        assert!(
            !should_warn("http://127.0.0.1:4317"),
            "loopback IPv4 http must not warn"
        );
        assert!(
            !should_warn("http://[::1]:4317"),
            "loopback IPv6 http must not warn"
        );

        // Non-local http must warn.
        assert!(
            should_warn("http://collector.internal:4317"),
            "non-local http must warn"
        );
        assert!(
            should_warn("http://10.0.0.5:4317"),
            "private IP http must warn"
        );

        // https must never warn regardless of host.
        assert!(
            !should_warn("https://collector.internal:4317"),
            "https must not warn"
        );
        assert!(
            !should_warn("https://localhost:4317"),
            "https localhost must not warn"
        );
    }

    /// Verify full `build_otlp_layer` pipeline with a live collector.
    /// Skipped in CI — run manually with Jaeger: `docker compose -f docker/docker-compose.tracing.yml up -d`
    #[cfg(feature = "otel")]
    #[test]
    #[ignore = "requires a live OTLP collector on localhost:4317"]
    fn build_otlp_layer_live_pipeline_returns_provider() {
        use zeph_core::config::{TelemetryBackend, TelemetryConfig};
        let telemetry = TelemetryConfig {
            enabled: true,
            backend: TelemetryBackend::Otlp,
            sample_rate: 1.0,
            otlp_endpoint: Some("http://localhost:4317".into()),
            ..TelemetryConfig::default()
        };
        let mut layers: Vec<
            Box<dyn tracing_subscriber::Layer<tracing_subscriber::Registry> + Send + Sync>,
        > = Vec::new();
        let provider = build_otlp_layer(&telemetry, &mut layers, false, false);
        assert!(provider.is_some(), "expected Some with valid endpoint");
        assert_eq!(layers.len(), 1, "one OTLP layer should be appended");
    }

    /// Verify that `TracingGuards` drops without panic when `otel_provider` is `Some`.
    /// Uses a no-exporter `SdkTracerProvider` (no network required).
    #[cfg(feature = "otel")]
    #[test]
    fn tracing_guards_drop_with_otel_provider_does_not_panic() {
        use opentelemetry_sdk::trace::SdkTracerProvider;
        let provider = SdkTracerProvider::builder().build();
        let guards = TracingGuards {
            log_guard: None,
            #[cfg(feature = "profiling")]
            chrome_guard: None,
            #[cfg(feature = "profiling-pyroscope")]
            pyroscope_guard: None,
            otel_provider: Some(provider),
        };
        drop(guards); // must not panic
    }

    /// Verify that `build_chrome_layer` returns `None` without creating files when telemetry
    /// is disabled, and that no layers are appended.
    #[cfg(feature = "profiling")]
    #[test]
    fn build_chrome_layer_disabled_returns_none() {
        use zeph_core::config::{TelemetryBackend, TelemetryConfig};
        let telemetry = TelemetryConfig {
            enabled: false,
            backend: TelemetryBackend::Local,
            trace_dir: std::path::PathBuf::from("/tmp/zeph-test-disabled"),
            ..TelemetryConfig::default()
        };
        let mut layers: Vec<
            Box<dyn tracing_subscriber::Layer<tracing_subscriber::Registry> + Send + Sync>,
        > = Vec::new();
        let guard = build_chrome_layer(&telemetry, &mut layers);
        assert!(guard.is_none(), "expected None when telemetry is disabled");
        assert!(
            layers.is_empty(),
            "no layer should be appended when disabled"
        );
    }

    /// Verify that `build_chrome_layer` returns a guard cell and creates a `.json` trace file
    /// when telemetry is enabled with `backend = Local`.
    #[cfg(feature = "profiling")]
    #[test]
    fn build_chrome_layer_enabled_local_creates_file() {
        use zeph_core::config::{TelemetryBackend, TelemetryConfig};
        let dir = tempfile::TempDir::new().expect("tempdir");
        let telemetry = TelemetryConfig {
            enabled: true,
            backend: TelemetryBackend::Local,
            trace_dir: dir.path().to_path_buf(),
            ..TelemetryConfig::default()
        };
        let mut layers: Vec<
            Box<dyn tracing_subscriber::Layer<tracing_subscriber::Registry> + Send + Sync>,
        > = Vec::new();
        let guard = build_chrome_layer(&telemetry, &mut layers);
        assert!(
            guard.is_some(),
            "expected a guard cell when telemetry is enabled"
        );
        assert_eq!(layers.len(), 1, "one chrome layer should be appended");
        // Take and drop the guard from its cell to flush and close the file, mirroring what
        // TracingGuards::drop and spawn_sigterm_flush_task each do.
        let taken = guard.and_then(|cell| cell.lock().take());
        assert!(taken.is_some(), "expected the guard cell to hold a guard");
        drop(taken);
        let json_files: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read dir")
            .filter_map(std::result::Result::ok)
            .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("json"))
            .collect();
        assert!(
            !json_files.is_empty(),
            "expected at least one .json trace file"
        );
    }

    /// Verify that `TracingGuards::drop` takes and flushes the chrome guard from its shared
    /// cell, closing the trace file's JSON array, even while a second clone of the cell (standing
    /// in for the SIGTERM task's clone in [`spawn_sigterm_flush_task`]) is still alive (#6683).
    #[cfg(feature = "profiling")]
    #[test]
    fn tracing_guards_drop_flushes_chrome_guard_cell() {
        use zeph_core::config::{TelemetryBackend, TelemetryConfig};
        let dir = tempfile::TempDir::new().expect("tempdir");
        let telemetry = TelemetryConfig {
            enabled: true,
            backend: TelemetryBackend::Local,
            trace_dir: dir.path().to_path_buf(),
            ..TelemetryConfig::default()
        };
        let mut layers: Vec<
            Box<dyn tracing_subscriber::Layer<tracing_subscriber::Registry> + Send + Sync>,
        > = Vec::new();
        let cell = build_chrome_layer(&telemetry, &mut layers).expect("guard cell");
        let cell_clone = std::sync::Arc::clone(&cell);

        let guards = TracingGuards {
            log_guard: None,
            chrome_guard: Some(cell),
            #[cfg(feature = "profiling-pyroscope")]
            pyroscope_guard: None,
            #[cfg(feature = "otel")]
            otel_provider: None,
        };
        drop(guards);

        assert!(
            cell_clone.lock().is_none(),
            "TracingGuards::drop must take the guard out of the shared cell"
        );
        let json_files: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read dir")
            .filter_map(std::result::Result::ok)
            .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("json"))
            .collect();
        assert!(!json_files.is_empty(), "expected a .json trace file");
        let contents = std::fs::read_to_string(json_files[0].path()).expect("read trace file");
        assert!(
            contents.trim_end().ends_with(']'),
            "flushed trace file must be a well-formed, closed JSON array: {contents}"
        );
    }

    /// Verify the core race invariant behind #6683 under genuine OS-thread concurrency (not
    /// just sequential simulation, per critic finding M3): two threads race to take the guard
    /// out of the shared cell at the same instant (synchronized via a `Barrier`), and exactly
    /// one of them must retrieve it — mutual exclusion must hold even under real contention.
    #[cfg(feature = "profiling")]
    #[test]
    fn chrome_guard_cell_take_is_idempotent_under_concurrent_access() {
        use zeph_core::config::{TelemetryBackend, TelemetryConfig};
        let dir = tempfile::TempDir::new().expect("tempdir");
        let telemetry = TelemetryConfig {
            enabled: true,
            backend: TelemetryBackend::Local,
            trace_dir: dir.path().to_path_buf(),
            ..TelemetryConfig::default()
        };
        let mut layers: Vec<
            Box<dyn tracing_subscriber::Layer<tracing_subscriber::Registry> + Send + Sync>,
        > = Vec::new();
        let cell = build_chrome_layer(&telemetry, &mut layers).expect("guard cell");

        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let cell_a = std::sync::Arc::clone(&cell);
        let barrier_a = std::sync::Arc::clone(&barrier);
        let racer = std::thread::spawn(move || {
            barrier_a.wait();
            cell_a.lock().take()
        });
        barrier.wait();
        let here = cell.lock().take();
        let there = racer.join().expect("racer thread panicked");

        let taken_count = usize::from(here.is_some()) + usize::from(there.is_some());
        assert_eq!(
            taken_count, 1,
            "exactly one of two concurrent takers must retrieve the guard, never zero or both"
        );
    }

    /// Verify that `TracingGuards::drop` takes and flushes the log guard from its shared cell,
    /// persisting pending log lines to disk, even while a second clone of the cell (standing in
    /// for the SIGTERM task's clone in [`spawn_sigterm_flush_task`]) is still alive. Generalizes
    /// `tracing_guards_drop_flushes_chrome_guard_cell` to the unconditional file-log cell (#6693).
    #[test]
    fn tracing_guards_drop_flushes_log_guard_cell() {
        use std::io::Write as _;

        let dir = tempfile::TempDir::new().expect("tempdir");
        let appender = tracing_appender::rolling::never(dir.path(), "test.log");
        let (mut non_blocking, guard) = tracing_appender::non_blocking(appender);
        non_blocking
            .write_all(b"hello from tracing_guards_drop_flushes_log_guard_cell\n")
            .expect("write to non-blocking log writer");

        let cell: LogGuardCell = std::sync::Arc::new(parking_lot::Mutex::new(Some(guard)));
        let cell_clone = std::sync::Arc::clone(&cell);

        let guards = TracingGuards {
            log_guard: Some(cell),
            #[cfg(feature = "profiling")]
            chrome_guard: None,
            #[cfg(feature = "profiling-pyroscope")]
            pyroscope_guard: None,
            #[cfg(feature = "otel")]
            otel_provider: None,
        };
        drop(guards);

        assert!(
            cell_clone.lock().is_none(),
            "TracingGuards::drop must take the guard out of the shared cell"
        );
        let contents = std::fs::read_to_string(dir.path().join("test.log")).expect("read log file");
        assert!(
            contents.contains("hello from tracing_guards_drop_flushes_log_guard_cell"),
            "flushed log file must contain the written line: {contents}"
        );
    }

    /// Verify the core race invariant behind #6693 under genuine OS-thread concurrency (not just
    /// sequential simulation), mirroring `chrome_guard_cell_take_is_idempotent_under_concurrent_access`
    /// for the unconditional log guard cell: two threads race to take the guard out of the shared
    /// cell at the same instant, and exactly one of them must retrieve it.
    #[test]
    fn log_guard_cell_take_is_idempotent_under_concurrent_access() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let appender = tracing_appender::rolling::never(dir.path(), "test.log");
        let (_non_blocking, guard) = tracing_appender::non_blocking(appender);
        let cell: LogGuardCell = std::sync::Arc::new(parking_lot::Mutex::new(Some(guard)));

        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let cell_a = std::sync::Arc::clone(&cell);
        let barrier_a = std::sync::Arc::clone(&barrier);
        let racer = std::thread::spawn(move || {
            barrier_a.wait();
            cell_a.lock().take()
        });
        barrier.wait();
        let here = cell.lock().take();
        let there = racer.join().expect("racer thread panicked");

        let taken_count = usize::from(here.is_some()) + usize::from(there.is_some());
        assert_eq!(
            taken_count, 1,
            "exactly one of two concurrent takers must retrieve the guard, never zero or both"
        );
    }

    /// Verify that `spawn_sigterm_flush_task` installs its listener without panicking and
    /// without touching the chrome guard cell before an actual signal arrives — the normal flush
    /// path (`TracingGuards::drop`) must still work undisturbed while the task is parked waiting.
    ///
    /// This necessarily registers a real, process-wide SIGTERM listener for the remainder of
    /// the test process (critic finding M3: unavoidable for testing installation at all — the
    /// task is inert until a real SIGTERM arrives, which this test never sends, so it cannot
    /// affect other tests in the same nextest-per-process test binary).
    #[cfg(feature = "profiling")]
    #[tokio::test]
    async fn spawn_sigterm_flush_task_does_not_touch_chrome_guard_before_signal() {
        use zeph_core::config::{TelemetryBackend, TelemetryConfig};
        let dir = tempfile::TempDir::new().expect("tempdir");
        let telemetry = TelemetryConfig {
            enabled: true,
            backend: TelemetryBackend::Local,
            trace_dir: dir.path().to_path_buf(),
            ..TelemetryConfig::default()
        };
        let mut layers: Vec<
            Box<dyn tracing_subscriber::Layer<tracing_subscriber::Registry> + Send + Sync>,
        > = Vec::new();
        let cell = build_chrome_layer(&telemetry, &mut layers).expect("guard cell");
        spawn_sigterm_flush_task(None, Some(std::sync::Arc::clone(&cell)));
        // Give the spawned listener task a chance to register before this test ends.
        tokio::task::yield_now().await;
        let taken = cell.lock().take();
        assert!(
            taken.is_some(),
            "normal flush path must still work with the SIGTERM task installed"
        );
    }

    /// Verify that `spawn_sigterm_flush_task` installs its listener without panicking and
    /// without touching the log guard cell before an actual signal arrives — mirrors
    /// `spawn_sigterm_flush_task_does_not_touch_chrome_guard_before_signal` but for the
    /// unconditional file-log cell (#6693).
    #[tokio::test]
    async fn spawn_sigterm_flush_task_does_not_touch_log_guard_before_signal() {
        let (_non_blocking, log_guard) = tracing_appender::non_blocking(std::io::sink());
        let cell: LogGuardCell = std::sync::Arc::new(parking_lot::Mutex::new(Some(log_guard)));
        spawn_sigterm_flush_task(
            Some(std::sync::Arc::clone(&cell)),
            #[cfg(feature = "profiling")]
            None,
        );
        // Give the spawned listener task a chance to register before this test ends.
        tokio::task::yield_now().await;
        let taken = cell.lock().take();
        assert!(
            taken.is_some(),
            "normal flush path must still work with the SIGTERM task installed"
        );
    }

    /// Verify #6682: `TraceStyle::Async` records paired `ph:"b"`/`"e"` events sharing an `id`
    /// (never a fragmented `ph:"X"` complete event), confirming the layer is actually configured
    /// as documented rather than only asserting it via code reading. Drives a real span through
    /// a scoped (non-global) subscriber via `tracing::subscriber::with_default`.
    #[cfg(feature = "profiling")]
    #[test]
    fn chrome_layer_async_style_records_paired_b_e_events_with_shared_id() {
        use zeph_core::config::{TelemetryBackend, TelemetryConfig};
        let dir = tempfile::TempDir::new().expect("tempdir");
        let telemetry = TelemetryConfig {
            enabled: true,
            backend: TelemetryBackend::Local,
            trace_dir: dir.path().to_path_buf(),
            ..TelemetryConfig::default()
        };
        let mut layers: Vec<
            Box<dyn tracing_subscriber::Layer<tracing_subscriber::Registry> + Send + Sync>,
        > = Vec::new();
        let cell = build_chrome_layer(&telemetry, &mut layers).expect("guard cell");
        let subscriber = tracing_subscriber::registry().with(layers);

        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!("test.nested.span");
            let _entered = span.enter();
            tracing::info!("inside span");
        });

        let guard = cell
            .lock()
            .take()
            .expect("guard cell still holds the guard");
        drop(guard); // flush and close the JSON array

        let json_files: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read dir")
            .filter_map(std::result::Result::ok)
            .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("json"))
            .collect();
        assert!(!json_files.is_empty(), "expected a .json trace file");
        let contents = std::fs::read_to_string(json_files[0].path()).expect("read trace file");
        let events: Vec<serde_json::Value> =
            serde_json::from_str(&contents).expect("trace file must be a valid JSON array");

        assert!(
            events.iter().all(|e| e["ph"] != "X"),
            "TraceStyle::Async must never emit complete ph:\"X\" events: {events:?}"
        );
        let b_events: Vec<_> = events.iter().filter(|e| e["ph"] == "b").collect();
        let e_events: Vec<_> = events.iter().filter(|e| e["ph"] == "e").collect();
        assert!(!b_events.is_empty(), "expected at least one ph:\"b\" event");
        assert_eq!(
            b_events.len(),
            e_events.len(),
            "every b event must have a matching e event for a cleanly-closed span: {events:?}"
        );
        let b_id = b_events[0]["id"]
            .as_u64()
            .expect("b event must carry a numeric id");
        assert!(
            e_events.iter().any(|e| e["id"].as_u64() == Some(b_id)),
            "b and e events for the same span tree must share an id: {events:?}"
        );
    }

    /// Verify that `filter_trace_metadata` excludes reserved keys and passes through valid ones.
    ///
    /// Tests the filtering logic used in `build_otlp_layer` without requiring a live OTLP
    /// collector. Confirms that `service.name` is dropped and that other keys are preserved.
    #[cfg(feature = "otel")]
    #[test]
    fn filter_trace_metadata_excludes_reserved_keys() {
        let mut meta = std::collections::HashMap::new();
        meta.insert("service.name".to_owned(), "should-be-dropped".to_owned());
        meta.insert("deployment.environment".to_owned(), "staging".to_owned());
        meta.insert("team.name".to_owned(), "platform".to_owned());

        let result = filter_trace_metadata(&meta);

        // Reserved key must not appear in result.
        assert!(
            result.iter().all(|(k, _)| *k != "service.name"),
            "service.name must be excluded from filtered metadata"
        );
        // Valid keys must be present.
        assert!(
            result
                .iter()
                .any(|(k, v)| *k == "deployment.environment" && *v == "staging"),
            "deployment.environment must be preserved"
        );
        assert!(
            result
                .iter()
                .any(|(k, v)| *k == "team.name" && *v == "platform"),
            "team.name must be preserved"
        );
        // Total: 2 valid keys, 1 reserved → 2 entries.
        assert_eq!(result.len(), 2);
    }

    /// Verify that `filter_trace_metadata` returns all entries when no reserved keys are present.
    #[cfg(feature = "otel")]
    #[test]
    fn filter_trace_metadata_passes_all_non_reserved() {
        let mut meta = std::collections::HashMap::new();
        meta.insert("vcs.revision".to_owned(), "abc1234".to_owned());
        let result = filter_trace_metadata(&meta);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], ("vcs.revision".to_owned(), "abc1234".to_owned()));
    }
}
