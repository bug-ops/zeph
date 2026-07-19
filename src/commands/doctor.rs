// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `zeph doctor` preflight connectivity and configuration checks.
//!
//! Runs a sequence of read-only checks and prints `[OK]`, `[WARN]`, or `[FAIL]` per check.
//! Exit code is 0 if no failures, 1 otherwise.

use std::io;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use zeph_core::redact::scrub_content;
use zeph_core::vault::{AgeVaultProvider, VaultProvider};

/// Individual check outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum CheckStatus {
    Ok,
    Warn,
    Fail,
}

impl CheckStatus {
    fn label(self) -> &'static str {
        match self {
            Self::Ok => "OK  ",
            Self::Warn => "WARN",
            Self::Fail => "FAIL",
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warn => "warn",
            Self::Fail => "fail",
        }
    }
}

/// Result of a single doctor check.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CheckResult {
    /// Dot-namespaced check name (e.g. `config.parse`).
    pub name: String,
    pub status: CheckStatus,
    /// Human-readable detail, redacted through `redact_secrets`.
    pub detail: String,
    /// Wall-clock time for this check in milliseconds.
    pub elapsed_ms: u64,
}

impl CheckResult {
    pub(crate) fn ok(name: impl Into<String>, detail: impl Into<String>, elapsed_ms: u64) -> Self {
        Self {
            name: name.into(),
            status: CheckStatus::Ok,
            detail: scrub_content(&detail.into()).into_owned(),
            elapsed_ms,
        }
    }

    pub(crate) fn warn(
        name: impl Into<String>,
        detail: impl Into<String>,
        elapsed_ms: u64,
    ) -> Self {
        Self {
            name: name.into(),
            status: CheckStatus::Warn,
            detail: scrub_content(&detail.into()).into_owned(),
            elapsed_ms,
        }
    }

    pub(crate) fn fail(
        name: impl Into<String>,
        detail: impl Into<String>,
        elapsed_ms: u64,
    ) -> Self {
        Self {
            name: name.into(),
            status: CheckStatus::Fail,
            detail: scrub_content(&detail.into()).into_owned(),
            elapsed_ms,
        }
    }
}

/// Aggregated doctor report.
pub(crate) struct DoctorReport {
    pub results: Vec<CheckResult>,
    pub elapsed_ms: u64,
}

impl DoctorReport {
    /// Render in human-readable plain-text format.
    ///
    /// # Errors
    ///
    /// Returns an `io::Error` if writing to `w` fails.
    pub fn render_plain(&self, w: &mut impl Write) -> io::Result<()> {
        for r in &self.results {
            writeln!(
                w,
                "[{}] {:<40} ({} ms)  {}",
                r.status.label(),
                r.name,
                r.elapsed_ms,
                r.detail
            )?;
        }
        let failures = self.failure_count();
        let warnings = self.warning_count();
        writeln!(w)?;
        if failures == 0 && warnings == 0 {
            writeln!(w, "All checks passed.")?;
        } else {
            let mut parts = Vec::new();
            if failures > 0 {
                parts.push(format!("{failures} check(s) failed"));
            }
            if warnings > 0 {
                parts.push(format!("{warnings} warning(s)"));
            }
            writeln!(w, "{}", parts.join(", "))?;
        }
        Ok(())
    }

    /// Render as a JSON envelope (`schema_version` = 1).
    ///
    /// # Errors
    ///
    /// Returns an `io::Error` if writing to `w` fails.
    pub fn render_json(&self, w: &mut impl Write) -> io::Result<()> {
        #[derive(Serialize)]
        struct JsonCheck<'a> {
            name: &'a str,
            status: &'a str,
            detail: &'a str,
            elapsed_ms: u64,
        }

        #[derive(Serialize)]
        struct JsonReport<'a> {
            schema_version: u8,
            overall: &'static str,
            failures: usize,
            warnings: usize,
            elapsed_ms: u64,
            checks: Vec<JsonCheck<'a>>,
        }

        let failures = self.failure_count();
        let warnings = self.warning_count();
        let overall = if failures > 0 {
            "fail"
        } else if warnings > 0 {
            "warn"
        } else {
            "ok"
        };

        let checks: Vec<JsonCheck<'_>> = self
            .results
            .iter()
            .map(|r| JsonCheck {
                name: &r.name,
                status: r.status.as_str(),
                detail: &r.detail,
                elapsed_ms: r.elapsed_ms,
            })
            .collect();

        let report = JsonReport {
            schema_version: 1,
            overall,
            failures,
            warnings,
            elapsed_ms: self.elapsed_ms,
            checks,
        };

        let json = serde_json::to_string_pretty(&report).map_err(io::Error::other)?;
        writeln!(w, "{json}")
    }

    /// Returns true if any check has `Fail` status.
    #[must_use]
    pub fn has_failures(&self) -> bool {
        self.results.iter().any(|r| r.status == CheckStatus::Fail)
    }

    fn failure_count(&self) -> usize {
        self.results
            .iter()
            .filter(|r| r.status == CheckStatus::Fail)
            .count()
    }

    fn warning_count(&self) -> usize {
        self.results
            .iter()
            .filter(|r| r.status == CheckStatus::Warn)
            .count()
    }
}

// ---------------------------------------------------------------------------
// Individual check helpers
// ---------------------------------------------------------------------------

fn elapsed_ms(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn check_config_parse(config_path: &Path) -> (CheckResult, Option<zeph_core::config::Config>) {
    let start = Instant::now();
    match zeph_core::config::Config::load(config_path) {
        Ok(config) => {
            if let Err(e) = config.validate() {
                return (
                    CheckResult::warn("config.parse", e.to_string(), elapsed_ms(start)),
                    None,
                );
            }
            if let Err(e) = config.llm.check_legacy_format() {
                return (
                    CheckResult::warn("config.parse", e.to_string(), elapsed_ms(start)),
                    None,
                );
            }
            let ms = elapsed_ms(start);
            (CheckResult::ok("config.parse", "valid", ms), Some(config))
        }
        Err(e) => (
            CheckResult::fail("config.parse", e.to_string(), elapsed_ms(start)),
            None,
        ),
    }
}

/// Reports whether vault-anchor downgrade-resistance (issue #6449) is active for this
/// deployment. `[integrity] anchor = "none"` is a documented, valid opt-out (reported `OK`, not
/// `WARN`) — see `zeph_config::AnchorMode`.
///
/// `key_path`/`vault_path` are the CLI-resolved vault location (`--vault-key`/`--vault-path`
/// overrides, falling back to `default_vault_dir()`) — the same resolution already used by
/// [`check_vault_file_exists`]/[`check_vault_file_mode`]/[`check_vault_key_mode`], so this check
/// never inspects an unrelated default vault when an override is in effect.
fn check_integrity_anchor(
    config: &zeph_core::config::Config,
    key_path: &Path,
    vault_path: &Path,
) -> CheckResult {
    let start = Instant::now();
    match config.integrity.anchor {
        zeph_config::AnchorMode::None => CheckResult::ok(
            "integrity.anchor",
            "anchor = \"none\" (explicit opt-out — chain-verified but not downgrade-resistant)",
            elapsed_ms(start),
        ),
        zeph_config::AnchorMode::Vault => {
            if key_path.exists() && vault_path.exists() {
                CheckResult::ok(
                    "integrity.anchor",
                    format!(
                        "anchor = \"vault\" (max_session_anchors = {})",
                        config.integrity.max_session_anchors
                    ),
                    elapsed_ms(start),
                )
            } else {
                CheckResult::warn(
                    "integrity.anchor",
                    "anchor = \"vault\" but no age vault found at bootstrap — sessions/transcripts \
                     are tamper-evident (issue #6360) but not downgrade-resistant until a vault \
                     exists (run `zeph --init`)",
                    elapsed_ms(start),
                )
            }
        }
    }
}

/// Reports the durable integrity seal status (issue #6449): sealed/unsealed, and how many
/// resumable pre-feature executions remain (blocking `zeph durable seal-integrity`).
///
/// `key_path`/`vault_path` are the CLI-resolved vault location — see
/// [`check_integrity_anchor`]'s doc comment for the resolution rationale.
fn check_durable_integrity_seal(
    config: &zeph_core::config::Config,
    key_path: &Path,
    vault_path: &Path,
) -> CheckResult {
    let start = Instant::now();
    if !config.durable.enabled {
        return CheckResult::ok(
            "durable.integrity_seal",
            "durable execution disabled",
            elapsed_ms(start),
        );
    }
    let Ok(provider) = AgeVaultProvider::load(key_path, vault_path) else {
        return CheckResult::warn(
            "durable.integrity_seal",
            "durable enabled but no age vault found — cannot resolve seal status",
            elapsed_ms(start),
        );
    };
    let (sealed, grandfather) = zeph_core::anchor_store::load_durable_integrity_seal(&provider);
    if sealed {
        CheckResult::ok(
            "durable.integrity_seal",
            format!("sealed ({} execution(s) grandfathered)", grandfather.len()),
            elapsed_ms(start),
        )
    } else {
        CheckResult::warn(
            "durable.integrity_seal",
            "not sealed — an absent integrity row on a resumed keyed execution is still trusted \
             as legacy; run `zeph durable seal-integrity` once no pre-feature resumable \
             executions remain (issue #6449)",
            elapsed_ms(start),
        )
    }
}

/// Reports whether `[gateway] enabled = true` has a resolvable, non-empty bearer token
/// (#6487). `GatewayServer::serve` now refuses to start in this exact state (`/webhook` would
/// otherwise forward unauthenticated external content directly into the agent's turn loop), so
/// this check surfaces the misconfiguration at `doctor` time instead of only at a failed launch.
///
/// `key_path`/`vault_path` are the same CLI-resolved vault location used by the other vault-aware
/// checks (see [`check_integrity_anchor`]'s doc comment for the resolution rationale) — this
/// check never inspects an unrelated default vault when an override is in effect.
///
/// Non-emptiness is checked after trimming, matching `AuthConfig::new`'s own normalization
/// (`zeph_common::http_middleware`) and `GatewayServer::serve`'s startup gate (critic M2): a
/// whitespace-only token must report the same `Fail` as no token at all, not a misleading `Ok`
/// for a value that `AuthConfig` will itself treat as "not configured" at runtime.
async fn check_gateway_auth(
    config: &zeph_core::config::Config,
    key_path: &Path,
    vault_path: &Path,
) -> CheckResult {
    let start = Instant::now();
    if !config.gateway.enabled {
        return CheckResult::ok("gateway.auth", "gateway disabled", elapsed_ms(start));
    }
    if config
        .gateway
        .auth_token
        .as_deref()
        .is_some_and(|t| !t.trim().is_empty())
    {
        return CheckResult::ok(
            "gateway.auth",
            "auth_token configured directly in config.toml",
            elapsed_ms(start),
        );
    }
    let Ok(provider) = AgeVaultProvider::load(key_path, vault_path) else {
        return CheckResult::fail(
            "gateway.auth",
            "gateway enabled but no auth_token and no age vault found to resolve \
             ZEPH_GATEWAY_TOKEN from — the gateway will refuse to start; run `zeph vault set \
             ZEPH_GATEWAY_TOKEN <token>`",
            elapsed_ms(start),
        );
    };
    match provider.get_secret("ZEPH_GATEWAY_TOKEN").await {
        Ok(Some(val)) if !val.trim().is_empty() => CheckResult::ok(
            "gateway.auth",
            "ZEPH_GATEWAY_TOKEN resolved from vault",
            elapsed_ms(start),
        ),
        Ok(_) => CheckResult::fail(
            "gateway.auth",
            "gateway enabled but ZEPH_GATEWAY_TOKEN is not set in the vault — the gateway will \
             refuse to start; run `zeph vault set ZEPH_GATEWAY_TOKEN <token>`",
            elapsed_ms(start),
        ),
        Err(e) => CheckResult::warn(
            "gateway.auth",
            format!("could not resolve ZEPH_GATEWAY_TOKEN from vault: {e}"),
            elapsed_ms(start),
        ),
    }
}

fn check_vault_file_exists(vault_path: &str) -> CheckResult {
    let start = Instant::now();
    let p = Path::new(vault_path);
    if p.exists() {
        CheckResult::ok("vault.file_exists", "present", elapsed_ms(start))
    } else {
        // Do not include the raw path in detail — it may contain username components.
        CheckResult::fail(
            "vault.file_exists",
            "vault key file not found (check vault.key_file in config)",
            elapsed_ms(start),
        )
    }
}

#[cfg(unix)]
fn check_vault_key_mode(key_path: &str) -> CheckResult {
    use std::os::unix::fs::PermissionsExt;
    let start = Instant::now();
    match std::fs::metadata(key_path) {
        Ok(meta) => {
            let mode = meta.permissions().mode();
            if mode & 0o077 != 0 {
                CheckResult::fail(
                    "vault.key_mode",
                    format!(
                        "key file has group/world permissions (mode {mode:#o}) — fix: chmod 600 {key_path}"
                    ),
                    elapsed_ms(start),
                )
            } else {
                CheckResult::ok(
                    "vault.key_mode",
                    format!("permissions ok (mode {mode:#o})"),
                    elapsed_ms(start),
                )
            }
        }
        Err(e) => CheckResult::fail("vault.key_mode", e.to_string(), elapsed_ms(start)),
    }
}

#[cfg(not(unix))]
fn check_vault_key_mode(_key_path: &str) -> CheckResult {
    CheckResult::ok("vault.key_mode", "skipped on non-unix", 0)
}

#[cfg(unix)]
fn check_vault_file_mode(vault_path: &str, check_name: &str) -> CheckResult {
    use std::os::unix::fs::PermissionsExt as _;
    let start = Instant::now();
    match std::fs::metadata(vault_path) {
        Ok(meta) => {
            let mode = meta.permissions().mode();
            if mode & 0o077 != 0 {
                CheckResult::fail(
                    check_name,
                    format!(
                        "vault file has group/world permissions (mode {mode:#o}) — fix: chmod 600 {vault_path}"
                    ),
                    elapsed_ms(start),
                )
            } else {
                CheckResult::ok(
                    check_name,
                    format!("permissions ok (mode {mode:#o})"),
                    elapsed_ms(start),
                )
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            CheckResult::ok(check_name, "file not found (skipped)", elapsed_ms(start))
        }
        Err(e) => CheckResult::fail(
            check_name,
            format!("could not read metadata: {e}"),
            elapsed_ms(start),
        ),
    }
}

#[cfg(not(unix))]
fn check_vault_file_mode(_vault_path: &str, check_name: &str) -> CheckResult {
    CheckResult::ok(check_name, "skipped on non-unix", 0)
}

fn check_fs_writable(name: &str, dir: &Path) -> CheckResult {
    let start = Instant::now();
    match tempfile::NamedTempFile::new_in(dir) {
        Ok(_) => CheckResult::ok(name, "writable", elapsed_ms(start)),
        Err(e) => CheckResult::fail(name, e.to_string(), elapsed_ms(start)),
    }
}

fn sqlite_parent_path(config: &zeph_core::config::Config) -> PathBuf {
    let db_path = config.memory.database_url.as_ref().map_or(
        config.memory.sqlite_path.as_str(),
        zeph_common::secret::Secret::expose,
    );
    Path::new(db_path)
        .parent()
        .unwrap_or(Path::new("."))
        .to_path_buf()
}

async fn check_sqlite(config: &zeph_core::config::Config, timeout_secs: u64) -> CheckResult {
    let start = Instant::now();
    let db_path = config.memory.database_url.as_ref().map_or(
        config.memory.sqlite_path.as_str(),
        zeph_common::secret::Secret::expose,
    );

    // Strip the sqlite:// or sqlite:/// prefix to get the raw file path.
    let file_path = db_path
        .strip_prefix("sqlite:///")
        .or_else(|| db_path.strip_prefix("sqlite://"))
        .unwrap_or(db_path)
        .to_owned();

    if file_path == ":memory:" {
        return CheckResult::ok(
            "sqlite.accessible",
            "in-memory database (no file)",
            elapsed_ms(start),
        );
    }

    let timeout = Duration::from_secs(timeout_secs);

    // Open read-only: read the SQLite magic header without calling connect() or running
    // migrations, so doctor never creates or modifies the database file.
    let result = tokio::time::timeout(
        timeout,
        tokio::task::spawn_blocking(move || {
            use std::io::Read as _;
            let path = std::path::Path::new(&file_path);
            if !path.exists() {
                return Err("not found — run zeph once to initialize");
            }
            let mut buf = [0u8; 16];
            let mut f = std::fs::File::open(path).map_err(|_| "cannot open (check permissions)")?;
            f.read_exact(&mut buf).map_err(|_| "read error")?;
            // SQLite databases start with "SQLite format 3\000" (16 bytes).
            if &buf[..6] != b"SQLite" {
                return Err("file is not a valid SQLite database");
            }
            Ok(())
        }),
    )
    .await;

    match result {
        Ok(Ok(Ok(()))) => CheckResult::ok("sqlite.accessible", "readable", elapsed_ms(start)),
        Ok(Ok(Err(msg))) => CheckResult::warn("sqlite.accessible", msg, elapsed_ms(start)),
        Ok(Err(join_err)) => {
            tracing::warn!(error = %join_err, "sqlite: spawn_blocking panicked");
            CheckResult::fail("sqlite.accessible", "internal error", elapsed_ms(start))
        }
        Err(_) => CheckResult::fail(
            "sqlite.accessible",
            format!("timeout after {timeout_secs}s"),
            elapsed_ms(start),
        ),
    }
}

#[allow(clippy::too_many_lines)]
async fn check_llm_provider(
    entry: &zeph_core::config::ProviderEntry,
    timeout_secs: u64,
) -> CheckResult {
    use zeph_core::config::ProviderKind;

    let provider_name = entry.effective_name();
    let check_name = format!("llm.{provider_name}");
    let start = Instant::now();

    // Candle / Gonka: no base_url to probe
    if entry.provider_type == ProviderKind::Candle {
        return CheckResult::ok(&check_name, "candle (local inference)", elapsed_ms(start));
    }
    if entry.provider_type == ProviderKind::Gonka {
        return CheckResult::ok(
            &check_name,
            "gonka (use `zeph gonka doctor` for detailed diagnostics)",
            elapsed_ms(start),
        );
    }

    let base_url = match entry.provider_type {
        ProviderKind::Ollama => entry
            .base_url
            .as_deref()
            .unwrap_or("http://localhost:11434")
            .to_owned(),
        ProviderKind::Claude => entry
            .base_url
            .as_deref()
            .unwrap_or("https://api.anthropic.com")
            .to_owned(),
        ProviderKind::OpenAi => entry
            .base_url
            .as_deref()
            .unwrap_or("https://api.openai.com")
            .to_owned(),
        ProviderKind::Gemini => entry
            .base_url
            .as_deref()
            .unwrap_or("https://generativelanguage.googleapis.com")
            .to_owned(),
        ProviderKind::Candle | ProviderKind::Gonka => unreachable!(),
        ProviderKind::Cocoon => entry
            .cocoon_client_url
            .clone()
            .unwrap_or_else(|| "http://localhost:10000".to_owned()),
        _ => entry.base_url.clone().unwrap_or_default(),
    };

    if base_url.is_empty() {
        return CheckResult::warn(&check_name, "no base_url configured", elapsed_ms(start));
    }

    let probe_url = match entry.provider_type {
        ProviderKind::Ollama => format!("{}/api/tags", base_url.trim_end_matches('/')),
        _ => format!("{}/v1/models", base_url.trim_end_matches('/')),
    };

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(timeout_secs))
        .build();

    let client = match client {
        Ok(c) => c,
        Err(e) => {
            // Log the real error (may contain URL) only to tracing; emit a generic message.
            tracing::warn!(error = %e, check = %check_name, "llm: client build failed");
            return CheckResult::fail(&check_name, "client build error", elapsed_ms(start));
        }
    };

    let result = tokio::time::timeout(
        Duration::from_secs(timeout_secs),
        client.get(&probe_url).send(),
    )
    .await;

    match result {
        Ok(Ok(resp)) => {
            let status = resp.status();
            let class = match status.as_u16() {
                200..=299 => "2xx",
                300..=399 => "3xx",
                400..=499 => "4xx",
                500..=599 => "5xx",
                _ => "other",
            };
            let detail = format!("{class}, {}ms", elapsed_ms(start));
            if status.is_success() {
                CheckResult::ok(&check_name, detail, elapsed_ms(start))
            } else if status.as_u16() == 401 || status.as_u16() == 403 {
                // Probe is anonymous; 401/403 proves reachability but auth is unverified.
                CheckResult::warn(
                    &check_name,
                    format!("{class} reachable, auth not verified (probe is anonymous)"),
                    elapsed_ms(start),
                )
            } else {
                CheckResult::warn(&check_name, detail, elapsed_ms(start))
            }
        }
        Ok(Err(e)) => {
            let msg = if e.is_timeout() {
                format!("timeout after {timeout_secs}s")
            } else {
                "connect_error".to_owned()
            };
            CheckResult::fail(&check_name, msg, elapsed_ms(start))
        }
        Err(_) => CheckResult::fail(
            &check_name,
            format!("timeout after {timeout_secs}s"),
            elapsed_ms(start),
        ),
    }
}

fn check_skills_dir(config: &zeph_core::config::Config) -> Vec<CheckResult> {
    let start = Instant::now();
    let paths = if config.skills.paths.is_empty() {
        vec![zeph_config::default_skills_dir()]
    } else {
        config.skills.paths.clone()
    };

    let mut results = Vec::new();
    for (idx, path_str) in paths.iter().enumerate() {
        let p = Path::new(path_str);
        // Use an index-based name — not the raw path — to avoid leaking filesystem layout.
        let check_name = format!("skills.dir[{idx}]");
        let s = Instant::now();
        if !p.exists() {
            results.push(CheckResult::warn(
                &check_name,
                "skills directory does not exist",
                elapsed_ms(s),
            ));
            continue;
        }
        match std::fs::read_dir(p) {
            Ok(_entries) => {
                let skill_count = count_skill_files(p);
                results.push(CheckResult::ok(
                    &check_name,
                    format!("{skill_count} SKILL.md file(s)"),
                    elapsed_ms(s),
                ));
            }
            Err(e) => results.push(CheckResult::fail(
                &check_name,
                format!("read error: {}", e.kind()),
                elapsed_ms(s),
            )),
        }
    }
    let _ = elapsed_ms(start);
    results
}

fn count_skill_files(dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut count = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            count += count_skill_files(&path);
        } else if path.file_name().and_then(|n| n.to_str()) == Some("SKILL.md") {
            count += 1;
        }
    }
    count
}

async fn check_qdrant(config: &zeph_core::config::Config, timeout_secs: u64) -> CheckResult {
    use zeph_core::config::VectorBackend;
    let start = Instant::now();
    if config.memory.vector_backend != VectorBackend::Qdrant {
        return CheckResult::ok(
            "qdrant.not_configured",
            "sqlite backend in use",
            elapsed_ms(start),
        );
    }
    let url = config.memory.qdrant_url.clone();
    let api_key: Option<String> = config
        .memory
        .qdrant_api_key
        .as_ref()
        .map(|s| s.expose().to_owned());
    let qdrant_timeout_secs = config.memory.qdrant_timeout_secs;
    let result = tokio::time::timeout(Duration::from_secs(timeout_secs), async move {
        use zeph_memory::vector_store::VectorStore as _;
        let ops = zeph_memory::QdrantOps::new(&url, api_key.as_deref())
            .map_err(|e| e.to_string())?
            .with_timeout(Duration::from_secs(qdrant_timeout_secs));
        ops.health_check()
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    })
    .await;
    match result {
        Ok(Ok(())) => CheckResult::ok("qdrant.reachable", "healthy", elapsed_ms(start)),
        // Qdrant is optional — unreachable server is WARN, not FAIL.
        Ok(Err(_)) => CheckResult::warn(
            "qdrant.reachable",
            "connection refused or health check failed",
            elapsed_ms(start),
        ),
        Err(_) => CheckResult::warn(
            "qdrant.reachable",
            format!("timeout after {timeout_secs}s"),
            elapsed_ms(start),
        ),
    }
}

async fn check_mcp_server(
    server: &zeph_config::McpServerConfig,
    mcp_config: &zeph_config::McpConfig,
    mcp_timeout_secs: u64,
) -> CheckResult {
    use zeph_config::McpPolicy;
    use zeph_mcp::{McpManager, PolicyEnforcer, ServerEntry};

    let check_name = format!("mcp.{}", server.id);
    let start = Instant::now();

    let transport = build_doctor_transport(server);
    let roots = server
        .roots
        .iter()
        .map(|r| zeph_mcp::roots::make_root(&r.uri, r.name.as_deref()))
        .collect::<Vec<_>>();

    let elicitation_enabled = server
        .elicitation_enabled
        .unwrap_or(mcp_config.elicitation_enabled);

    let entry = ServerEntry {
        id: server.id.clone(),
        transport,
        timeout: Duration::from_secs(server.timeout),
        trust_level: server.trust_level,
        tool_allowlist: server.tool_allowlist.clone(),
        allow_untrusted_without_allowlist: server.allow_untrusted_without_allowlist,
        expected_tools: server.expected_tools.clone(),
        roots,
        tool_metadata: server.tool_metadata.clone(),
        elicitation_enabled,
        elicitation_timeout_secs: mcp_config.elicitation_timeout,
        env_isolation: server
            .env_isolation
            .unwrap_or(mcp_config.default_env_isolation),
        media_passthrough: server.media_passthrough,
    };

    let enforcer = PolicyEnforcer::new(vec![(server.id.clone(), McpPolicy::default())]);
    let manager =
        McpManager::with_elicitation_capacity(vec![entry.clone()], Vec::new(), enforcer, 1);

    let entry_clone = entry;
    let handle = tokio::spawn(async move {
        // EXEMPT(#5143): immediately awaited at :692 to catch panics; one-off CLI check
        tokio::time::timeout(
            Duration::from_secs(mcp_timeout_secs),
            manager.add_server(&entry_clone),
        )
        .await
    });

    match handle.await {
        Ok(Ok(Ok(tools))) => CheckResult::ok(
            &check_name,
            format!("{} tool(s)", tools.len()),
            elapsed_ms(start),
        ),
        Ok(Ok(Err(e))) => CheckResult::fail(
            &check_name,
            scrub_content(&e.to_string()).into_owned(),
            elapsed_ms(start),
        ),
        Ok(Err(_elapsed)) => CheckResult::fail(
            &check_name,
            format!("timeout after {mcp_timeout_secs}s"),
            elapsed_ms(start),
        ),
        Err(join_err) if join_err.is_panic() => CheckResult::fail(
            &check_name,
            "panicked (see log)".to_owned(),
            elapsed_ms(start),
        ),
        Err(join_err) => CheckResult::fail(
            &check_name,
            scrub_content(&join_err.to_string()).into_owned(),
            elapsed_ms(start),
        ),
    }
}

fn build_doctor_transport(server: &zeph_config::McpServerConfig) -> zeph_mcp::McpTransport {
    if let Some(url) = &server.url {
        return zeph_mcp::McpTransport::Http {
            url: url.clone(),
            headers: server.headers.clone(),
        };
    }
    zeph_mcp::McpTransport::Stdio {
        command: server.command.clone().unwrap_or_default(),
        args: server.args.clone(),
        env: server.env.clone(),
    }
}

// ---------------------------------------------------------------------------
// URL scheme check
// ---------------------------------------------------------------------------

/// Check whether the `zeph://` URL scheme is registered and not stale.
///
/// Returns a [`CheckResult`] with status `Ok` when registration is current, `Warn` when the
/// scheme is not registered (non-fatal — user may not have run `url-scheme register`), and
/// `Fail` when the registration exists but points to a binary that is missing or does not match
/// the currently running executable.
#[cfg(feature = "deep-link")]
fn check_url_scheme() -> CheckResult {
    use crate::url_scheme::register;
    let start = Instant::now();
    let current_exe = std::env::current_exe().ok();

    let stale = register::scheme_registration_status(current_exe.as_deref());

    match stale {
        register::SchemeStatus::Ok => CheckResult::ok(
            "url_scheme.registration",
            "registered and current",
            elapsed_ms(start),
        ),
        register::SchemeStatus::NotRegistered => CheckResult::warn(
            "url_scheme.registration",
            "not registered — run `zeph url-scheme register` to enable zeph:// URIs",
            elapsed_ms(start),
        ),
        register::SchemeStatus::Stale(reason) => {
            CheckResult::fail("url_scheme.registration", reason, elapsed_ms(start))
        }
    }
}

// ---------------------------------------------------------------------------
// Main entry point
// ---------------------------------------------------------------------------

/// Runs every doctor check and assembles the aggregate report, without rendering it.
///
/// Split out from [`run_doctor`] so the full check pipeline — including the `--vault-path`/
/// `--vault-key` override resolution shared by checks 2-4 and 17 (#6479) — can be exercised
/// end-to-end in tests against an in-memory [`DoctorReport`], without needing to capture the
/// process's real stdout (`run_doctor`'s [`finish`] writes there directly).
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
async fn build_doctor_report(
    config_path: &Path,
    llm_timeout_secs: u64,
    mcp_timeout_secs: u64,
    vault_override: Option<&str>,
    vault_key_override: Option<&Path>,
    vault_path_override: Option<&Path>,
) -> DoctorReport {
    let total_start = Instant::now();
    let mut results: Vec<CheckResult> = Vec::new();

    // 1. config.parse
    let (config_result, config_opt) = check_config_parse(config_path);
    let config_failed = config_result.status == CheckStatus::Fail;
    results.push(config_result);

    let Some(config) = config_opt else {
        return DoctorReport {
            elapsed_ms: elapsed_ms(total_start),
            results,
        };
    };

    // 2 + 3 + 4. Vault checks
    let vault_args_result = crate::bootstrap::parse_vault_args(
        &config,
        vault_override,
        vault_key_override,
        vault_path_override,
    );
    {
        let start = Instant::now();
        match &vault_args_result {
            Ok(vault_args) => {
                if vault_args.backend == zeph_config::VaultBackend::Age {
                    if let Some(ref vault_path) = vault_args.vault_path {
                        results.push(check_vault_file_exists(vault_path));
                        results.push(check_vault_file_mode(vault_path, "vault.file_mode"));
                    }
                    if let Some(ref key_path) = vault_args.key_path {
                        results.push(check_vault_key_mode(key_path));
                    }
                }
            }
            Err(e) => {
                results.push(CheckResult::fail(
                    "vault.backend",
                    e.clone(),
                    elapsed_ms(start),
                ));
            }
        }
    }

    // 5. filesystem.sqlite_parent
    {
        let parent = sqlite_parent_path(&config);
        if !parent.as_os_str().is_empty() && parent.exists() {
            results.push(check_fs_writable("filesystem.sqlite_parent", &parent));
        } else {
            let start = Instant::now();
            results.push(CheckResult::warn(
                "filesystem.sqlite_parent",
                "sqlite data directory does not exist",
                elapsed_ms(start),
            ));
        }
    }

    // 6. filesystem.skills.*
    results.extend(check_skills_dir(&config));

    // 7. filesystem.logging
    if !config.logging.file.is_empty() {
        let log_path = Path::new(&config.logging.file);
        if let Some(parent) = log_path.parent() {
            if parent.as_os_str().is_empty() || parent.exists() {
                results.push(check_fs_writable(
                    "filesystem.logging",
                    if parent.as_os_str().is_empty() {
                        Path::new(".")
                    } else {
                        parent
                    },
                ));
            } else {
                let start = Instant::now();
                results.push(CheckResult::warn(
                    "filesystem.logging",
                    "log directory does not exist",
                    elapsed_ms(start),
                ));
            }
        }
    }

    // 8. filesystem.audit_log
    if config.tools.audit.enabled
        && let zeph_config::AuditDestination::File(ref audit_file_path) =
            config.tools.audit.destination
    {
        let audit_path = audit_file_path.as_path();
        if let Some(parent) = audit_path.parent() {
            let check_dir = if parent.as_os_str().is_empty() {
                Path::new(".")
            } else {
                parent
            };
            if check_dir.exists() {
                results.push(check_fs_writable("filesystem.audit_log", check_dir));
            }
        }
    }

    // 9. filesystem.debug_dir
    if config.debug.enabled {
        let debug_dir = &config.debug.output_dir;
        if debug_dir.exists() {
            results.push(check_fs_writable("filesystem.debug_dir", debug_dir));
        } else {
            let start = Instant::now();
            results.push(CheckResult::warn(
                "filesystem.debug_dir",
                "debug output directory does not exist",
                elapsed_ms(start),
            ));
        }
    }

    // 10. filesystem.trace_dir
    if config.telemetry.enabled {
        let trace_dir = &config.telemetry.trace_dir;
        if trace_dir.exists() {
            results.push(check_fs_writable("filesystem.trace_dir", trace_dir));
        }
    }

    // 11. llm.<provider_name> — one per [[llm.providers]]
    if !config_failed {
        for entry in &config.llm.providers {
            results.push(check_llm_provider(entry, llm_timeout_secs).await);
        }
    }

    // 12. sqlite.accessible
    results.push(check_sqlite(&config, llm_timeout_secs).await);

    // 13. qdrant
    results.push(check_qdrant(&config, llm_timeout_secs).await);

    // 14. skills.registry — aggregate count (already reported per-path above, add summary)
    {
        let start = Instant::now();
        let paths = if config.skills.paths.is_empty() {
            vec![zeph_config::default_skills_dir()]
        } else {
            config.skills.paths.clone()
        };
        let total: usize = paths.iter().map(|p| count_skill_files(Path::new(p))).sum();
        let status = if total == 0 {
            CheckStatus::Warn
        } else {
            CheckStatus::Ok
        };
        results.push(CheckResult {
            name: "skills.registry".into(),
            status,
            detail: format!("{total} total SKILL.md file(s)"),
            elapsed_ms: elapsed_ms(start),
        });
    }

    // 15. mcp.<server_id>
    for server in &config.mcp.servers {
        results.push(check_mcp_server(server, &config.mcp, mcp_timeout_secs).await);
    }

    // 16. url_scheme.registration (deep-link feature only)
    #[cfg(feature = "deep-link")]
    results.push(check_url_scheme());

    // 17. integrity.anchor / durable.seal (issue #6449)
    //
    // Resolve the same vault_args computed for checks 2-4 above (--vault-key/--vault-path
    // overrides, falling back to default_vault_dir()) so these two checks never diverge from
    // the vault the rest of this invocation is configured against (#6479).
    let (anchor_key_path, anchor_vault_path) = {
        let default_dir = zeph_core::vault::default_vault_dir();
        let (resolved_key, resolved_vault) = vault_args_result
            .as_ref()
            .map(|va| (va.key_path.clone(), va.vault_path.clone()))
            .unwrap_or_default();
        (
            resolved_key.map_or_else(|| default_dir.join("vault-key.txt"), PathBuf::from),
            resolved_vault.map_or_else(|| default_dir.join("secrets.age"), PathBuf::from),
        )
    };
    results.push(check_integrity_anchor(
        &config,
        &anchor_key_path,
        &anchor_vault_path,
    ));
    results.push(check_durable_integrity_seal(
        &config,
        &anchor_key_path,
        &anchor_vault_path,
    ));

    // 18. gateway.auth (issue #6487)
    results.push(check_gateway_auth(&config, &anchor_key_path, &anchor_vault_path).await);

    DoctorReport {
        elapsed_ms: elapsed_ms(total_start),
        results,
    }
}

/// Runs every doctor check and renders the report to stdout (plain text or JSON).
///
/// # Errors
///
/// Returns an error if config resolution or I/O fails at the top level.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_doctor(
    config_path: &Path,
    json: bool,
    llm_timeout_secs: u64,
    mcp_timeout_secs: u64,
    vault_override: Option<&str>,
    vault_key_override: Option<&Path>,
    vault_path_override: Option<&Path>,
) -> anyhow::Result<i32> {
    let report = Box::pin(build_doctor_report(
        config_path,
        llm_timeout_secs,
        mcp_timeout_secs,
        vault_override,
        vault_key_override,
        vault_path_override,
    ))
    .await;
    finish(&report, json)
}

fn finish(report: &DoctorReport, json: bool) -> anyhow::Result<i32> {
    let stdout = io::stdout();
    let mut handle = stdout.lock();
    if json {
        report.render_json(&mut handle)?;
    } else {
        report.render_plain(&mut handle)?;
    }
    Ok(i32::from(report.has_failures()))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn synthetic_report(statuses: &[(&str, CheckStatus, &str)]) -> DoctorReport {
        DoctorReport {
            elapsed_ms: 42,
            results: statuses
                .iter()
                .map(|(name, status, detail)| CheckResult {
                    name: name.to_string(),
                    status: *status,
                    detail: detail.to_string(),
                    elapsed_ms: 1,
                })
                .collect(),
        }
    }

    #[test]
    fn doctor_command_parses_json_flag() {
        // Verify CheckStatus serialises as expected
        let s = serde_json::to_string(&CheckStatus::Ok).unwrap();
        assert_eq!(s, "\"ok\"");
        let s = serde_json::to_string(&CheckStatus::Fail).unwrap();
        assert_eq!(s, "\"fail\"");
    }

    #[test]
    fn doctor_exits_with_code_zero_on_all_ok() {
        let report = synthetic_report(&[
            ("config.parse", CheckStatus::Ok, "valid"),
            ("sqlite.accessible", CheckStatus::Ok, "3 migrations"),
        ]);
        assert!(!report.has_failures());
    }

    #[test]
    fn doctor_reports_failure_exit_code_when_any_fail() {
        let report = synthetic_report(&[
            ("config.parse", CheckStatus::Ok, "valid"),
            ("sqlite.accessible", CheckStatus::Fail, "database locked"),
        ]);
        assert!(report.has_failures());
    }

    #[test]
    fn doctor_json_output_is_valid_json() {
        let report = synthetic_report(&[
            ("config.parse", CheckStatus::Ok, "valid"),
            ("vault.file_exists", CheckStatus::Warn, "missing"),
        ]);
        let mut buf = Vec::new();
        report.render_json(&mut buf).unwrap();
        let _: serde_json::Value = serde_json::from_slice(&buf).expect("must be valid JSON");
    }

    #[test]
    fn doctor_json_envelope_has_schema_version_one() {
        let report = synthetic_report(&[("config.parse", CheckStatus::Ok, "valid")]);
        let mut buf = Vec::new();
        report.render_json(&mut buf).unwrap();
        let val: serde_json::Value = serde_json::from_slice(&buf).unwrap();
        assert_eq!(val["schema_version"], 1);
    }

    #[test]
    fn doctor_json_overall_fail_when_any_fail() {
        let report = synthetic_report(&[("sqlite.accessible", CheckStatus::Fail, "locked")]);
        let mut buf = Vec::new();
        report.render_json(&mut buf).unwrap();
        let val: serde_json::Value = serde_json::from_slice(&buf).unwrap();
        assert_eq!(val["overall"], "fail");
        assert_eq!(val["failures"], 1);
    }

    #[test]
    fn doctor_redaction_snapshot_plain() {
        let fake_key = "sk-REDACTMEAKIA1234567890abcdef";
        let aws_key = "AKIAIOSFODNN7EXAMPLE";
        let result = CheckResult::fail(
            "test.check",
            format!("error: api_key={fake_key} aws={aws_key}"),
            5,
        );
        assert!(
            !result.detail.contains(fake_key),
            "openai key must be redacted"
        );
        assert!(!result.detail.contains(aws_key), "aws key must be redacted");

        let report = DoctorReport {
            elapsed_ms: 10,
            results: vec![result],
        };
        let mut buf = Vec::new();
        report.render_plain(&mut buf).unwrap();
        let rendered = String::from_utf8(buf).unwrap();
        assert!(!rendered.contains(fake_key));
        assert!(!rendered.contains(aws_key));
        // Insta snapshot: captures the redacted shape so regressions are caught.
        insta::assert_snapshot!("redaction_plain", rendered.lines().next().unwrap_or(""));
    }

    #[test]
    fn doctor_redaction_snapshot_json() {
        let fake_key = "sk-REDACTMEAKIA1234567890abcdef";
        let aws_key = "AKIAIOSFODNN7EXAMPLE";
        let result =
            CheckResult::fail("test.check", format!("api_key={fake_key} aws={aws_key}"), 5);
        let report = DoctorReport {
            elapsed_ms: 10,
            results: vec![result],
        };
        let mut buf = Vec::new();
        report.render_json(&mut buf).unwrap();
        let rendered = String::from_utf8(buf).unwrap();
        assert!(!rendered.contains(fake_key));
        assert!(!rendered.contains(aws_key));

        // Parse JSON and snapshot the detail field only (strip elapsed_ms which varies).
        let val: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        let detail = val["checks"][0]["detail"].as_str().unwrap_or("");
        insta::assert_snapshot!("redaction_json_detail", detail);
    }

    #[test]
    fn doctor_detail_does_not_contain_filesystem_paths() {
        // Paths like /Users/alice/... must not appear verbatim in detail.
        let result = CheckResult::warn(
            "vault.file_exists",
            "vault key file not found (check vault.key_file in config)",
            1,
        );
        assert!(
            !result.detail.contains("/Users/"),
            "Unix home path must not be in detail"
        );
        assert!(
            !result.detail.contains("/home/"),
            "Linux home path must not be in detail"
        );
    }

    #[test]
    fn doctor_json_warns_on_warn_status() {
        let report = synthetic_report(&[("llm.fast", CheckStatus::Warn, "4xx, 200ms")]);
        let mut buf = Vec::new();
        report.render_json(&mut buf).unwrap();
        let val: serde_json::Value = serde_json::from_slice(&buf).unwrap();
        assert_eq!(val["overall"], "warn");
        assert_eq!(val["warnings"], 1);
        assert_eq!(val["failures"], 0);
    }

    #[cfg(unix)]
    #[test]
    fn doctor_vault_check_fails_on_group_readable_key() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join("vault-key.txt");
        std::fs::write(&key_path, "key").unwrap();
        // Set group-readable permissions
        std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o640)).unwrap();
        let result = check_vault_key_mode(key_path.to_str().unwrap());
        assert_eq!(
            result.status,
            CheckStatus::Fail,
            "group-readable key must FAIL"
        );
        assert!(
            result.detail.contains("chmod 600"),
            "FAIL message must include remediation command, got: {}",
            result.detail
        );
    }

    #[cfg(unix)]
    #[test]
    fn doctor_vault_check_ok_on_private_key() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join("vault-key.txt");
        std::fs::write(&key_path, "key").unwrap();
        std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let result = check_vault_key_mode(key_path.to_str().unwrap());
        assert_eq!(result.status, CheckStatus::Ok);
    }

    #[cfg(unix)]
    #[test]
    fn doctor_vault_file_mode_fail_includes_chmod_hint() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let vault_path = dir.path().join("secrets.age");
        std::fs::write(&vault_path, "data").unwrap();
        std::fs::set_permissions(&vault_path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let result = check_vault_file_mode(vault_path.to_str().unwrap(), "vault.file_mode");
        assert_eq!(result.status, CheckStatus::Fail);
        assert!(
            result.detail.contains("chmod 600"),
            "FAIL message must include remediation command, got: {}",
            result.detail
        );
    }

    // #6479: check_integrity_anchor/check_durable_integrity_seal must inspect the vault path
    // they're given, not an unconditionally hardcoded default_vault_dir().
    #[test]
    fn check_integrity_anchor_ok_when_vault_present_at_given_path() {
        let dir = tempfile::tempdir().unwrap();
        zeph_core::vault::AgeVaultProvider::init_vault(dir.path()).unwrap();
        let mut config = zeph_core::config::Config::default();
        config.integrity.anchor = zeph_config::AnchorMode::Vault;

        let result = check_integrity_anchor(
            &config,
            &dir.path().join("vault-key.txt"),
            &dir.path().join("secrets.age"),
        );
        assert_eq!(result.status, CheckStatus::Ok);
    }

    #[test]
    fn check_integrity_anchor_warns_when_given_path_has_no_vault() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = zeph_core::config::Config::default();
        config.integrity.anchor = zeph_config::AnchorMode::Vault;

        // No vault written at `dir` — an override pointing here must WARN, independent of
        // whether a real vault happens to exist at the machine's default_vault_dir().
        let result = check_integrity_anchor(
            &config,
            &dir.path().join("vault-key.txt"),
            &dir.path().join("secrets.age"),
        );
        assert_eq!(result.status, CheckStatus::Warn);
    }

    #[test]
    fn check_durable_integrity_seal_resolves_given_vault_path() {
        let dir = tempfile::tempdir().unwrap();
        zeph_core::vault::AgeVaultProvider::init_vault(dir.path()).unwrap();
        let mut config = zeph_core::config::Config::default();
        config.durable.enabled = true;

        let result = check_durable_integrity_seal(
            &config,
            &dir.path().join("vault-key.txt"),
            &dir.path().join("secrets.age"),
        );
        // A freshly-initialized vault has no seal secret yet — WARN (not sealed), never a
        // hard load failure, proving the given path was actually loaded.
        assert_eq!(result.status, CheckStatus::Warn);
        assert!(result.detail.contains("not sealed"));
    }

    #[test]
    fn check_durable_integrity_seal_warns_when_given_path_has_no_vault() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = zeph_core::config::Config::default();
        config.durable.enabled = true;

        let result = check_durable_integrity_seal(
            &config,
            &dir.path().join("vault-key.txt"),
            &dir.path().join("secrets.age"),
        );
        assert_eq!(result.status, CheckStatus::Warn);
        assert!(result.detail.contains("no age vault found"));
    }

    // #6479: end-to-end regression — `build_doctor_report` (the full pipeline `run_doctor`
    // drives) must actually thread a `--vault-path`/`--vault-key` override into checks 17/18,
    // not just the standalone `check_integrity_anchor`/`check_durable_integrity_seal` functions
    // tested above. This is the exact wiring gap #6479 originally had: each check function was
    // individually correct in isolation, but the call site in `run_doctor` passed it the wrong
    // (hardcoded default) path.
    #[tokio::test]
    async fn build_doctor_report_threads_vault_override_into_integrity_checks() {
        let config_dir = tempfile::tempdir().unwrap();
        let config_path = config_dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            zeph_core::config::Config::dump_defaults().expect("dump defaults"),
        )
        .unwrap();

        // Deliberately empty — no vault-key.txt/secrets.age here. If the override were ignored
        // (the pre-#6479 bug), this check would instead report whatever status the real machine's
        // default_vault_dir() happens to have, which is exactly the bug this test guards against.
        let vault_dir = tempfile::tempdir().unwrap();
        let key_path = vault_dir.path().join("vault-key.txt");
        let vault_path = vault_dir.path().join("secrets.age");

        let report = Box::pin(build_doctor_report(
            &config_path,
            1, // llm_timeout_secs — bounds any localhost provider probe in dumped defaults
            1, // mcp_timeout_secs
            None,
            Some(&key_path),
            Some(&vault_path),
        ))
        .await;

        let anchor = report
            .results
            .iter()
            .find(|r| r.name == "integrity.anchor")
            .expect("integrity.anchor check must run");
        assert_eq!(
            anchor.status,
            CheckStatus::Warn,
            "override points at an empty dir — must WARN via the override path, not silently \
             report OK against the real default vault; got detail: {}",
            anchor.detail
        );

        let seal = report
            .results
            .iter()
            .find(|r| r.name == "durable.integrity_seal")
            .expect("durable.integrity_seal check must run");
        // `[durable] enabled` is false in dumped defaults, so this reports OK ("durable
        // execution disabled") regardless of the vault override — confirms the check still runs
        // and the override didn't cause a panic/short-circuit, without depending on `[durable]`
        // being enabled in the default config.
        assert_eq!(seal.status, CheckStatus::Ok);
        assert!(seal.detail.contains("disabled"));

        // #6487 (critic M5): confirms `check_gateway_auth` is actually wired into
        // `build_doctor_report`'s pipeline, not just independently correct when called directly
        // (all 5 dedicated `check_gateway_auth_*` tests below call it directly). Dumped defaults
        // have `[gateway] enabled = false`, so this reports OK — the point is that the check ran
        // at all through the real pipeline; a removed or misplaced wiring line would make this
        // `find` return `None` instead.
        let gateway_auth = report
            .results
            .iter()
            .find(|r| r.name == "gateway.auth")
            .expect("gateway.auth check must run as part of build_doctor_report");
        assert_eq!(gateway_auth.status, CheckStatus::Ok);
        assert!(gateway_auth.detail.contains("disabled"));
    }

    // #6487 (critic M5): dedicated end-to-end regression — `build_doctor_report` must surface a
    // `Fail` for `gateway.auth` when the *parsed config* (not a directly-constructed one) has
    // gateway enabled with no token anywhere, proving the check_gateway_auth(...).await call at
    // the `results.push` site actually executes with the right config/vault-path plumbing.
    #[tokio::test]
    async fn build_doctor_report_wires_gateway_auth_check_and_fails_when_unsafe() {
        let config_dir = tempfile::tempdir().unwrap();
        let config_path = config_dir.path().join("config.toml");
        // Mutate the struct and re-serialize, rather than appending a second `[gateway]` TOML
        // header onto `dump_defaults()`'s output — the defaults already emit an active
        // `[gateway]` section (`enabled = false`), so appending another would be a duplicate
        // table and fail `check_config_parse` before `check_gateway_auth` ever ran.
        let mut config = zeph_core::config::Config::default();
        config.gateway.enabled = true;
        let toml_src = toml::to_string_pretty(&config).expect("serialize config");
        std::fs::write(&config_path, toml_src).unwrap();

        // No vault at all — ZEPH_GATEWAY_TOKEN cannot resolve from anywhere.
        let vault_dir = tempfile::tempdir().unwrap();
        let key_path = vault_dir.path().join("vault-key.txt");
        let vault_path = vault_dir.path().join("secrets.age");

        let report = Box::pin(build_doctor_report(
            &config_path,
            1,
            1,
            None,
            Some(&key_path),
            Some(&vault_path),
        ))
        .await;

        let gateway_auth = report
            .results
            .iter()
            .find(|r| r.name == "gateway.auth")
            .expect("gateway.auth check must run as part of build_doctor_report");
        assert_eq!(
            gateway_auth.status,
            CheckStatus::Fail,
            "gateway enabled with no resolvable token anywhere must Fail through the real \
             pipeline, got detail: {}",
            gateway_auth.detail
        );
        assert!(gateway_auth.detail.contains("vault set ZEPH_GATEWAY_TOKEN"));
    }

    // #6487: gateway.auth doctor check.

    #[tokio::test]
    async fn check_gateway_auth_ok_when_gateway_disabled() {
        let config = zeph_core::config::Config::default();
        let dir = tempfile::tempdir().unwrap();
        let result = check_gateway_auth(
            &config,
            &dir.path().join("vault-key.txt"),
            &dir.path().join("secrets.age"),
        )
        .await;
        assert_eq!(result.status, CheckStatus::Ok);
        assert!(result.detail.contains("disabled"));
    }

    #[tokio::test]
    async fn check_gateway_auth_ok_when_token_set_directly_in_config() {
        let mut config = zeph_core::config::Config::default();
        config.gateway.enabled = true;
        config.gateway.auth_token = Some("inline-secret".into());
        let dir = tempfile::tempdir().unwrap();
        let result = check_gateway_auth(
            &config,
            &dir.path().join("vault-key.txt"),
            &dir.path().join("secrets.age"),
        )
        .await;
        assert_eq!(result.status, CheckStatus::Ok);
        assert!(result.detail.contains("config.toml"));
    }

    #[tokio::test]
    async fn check_gateway_auth_fails_when_enabled_with_no_token_and_no_vault() {
        let mut config = zeph_core::config::Config::default();
        config.gateway.enabled = true;
        let dir = tempfile::tempdir().unwrap();
        let result = check_gateway_auth(
            &config,
            &dir.path().join("vault-key.txt"),
            &dir.path().join("secrets.age"),
        )
        .await;
        assert_eq!(result.status, CheckStatus::Fail);
        assert!(result.detail.contains("vault set ZEPH_GATEWAY_TOKEN"));
    }

    #[tokio::test]
    async fn check_gateway_auth_fails_when_enabled_with_vault_present_but_no_token_key() {
        let mut config = zeph_core::config::Config::default();
        config.gateway.enabled = true;
        let dir = tempfile::tempdir().unwrap();
        zeph_core::vault::AgeVaultProvider::init_vault(dir.path()).unwrap();
        let result = check_gateway_auth(
            &config,
            &dir.path().join("vault-key.txt"),
            &dir.path().join("secrets.age"),
        )
        .await;
        assert_eq!(result.status, CheckStatus::Fail);
        assert!(result.detail.contains("ZEPH_GATEWAY_TOKEN is not set"));
    }

    #[tokio::test]
    async fn check_gateway_auth_ok_when_token_resolves_from_vault() {
        let mut config = zeph_core::config::Config::default();
        config.gateway.enabled = true;
        let dir = tempfile::tempdir().unwrap();
        zeph_core::vault::AgeVaultProvider::init_vault(dir.path()).unwrap();
        let key_path = dir.path().join("vault-key.txt");
        let vault_path = dir.path().join("secrets.age");
        let mut provider =
            zeph_core::vault::AgeVaultProvider::load(&key_path, &vault_path).unwrap();
        provider
            .set_secret_mut("ZEPH_GATEWAY_TOKEN".into(), "vault-secret".into(), false)
            .unwrap();
        provider.save().unwrap();

        let result = check_gateway_auth(&config, &key_path, &vault_path).await;
        assert_eq!(result.status, CheckStatus::Ok);
        assert!(result.detail.contains("resolved from vault"));
    }

    // #6487 (critic M2): whitespace-only tokens must Fail, not silently pass as "configured" —
    // `AuthConfig::new` (zeph-common) trims before checking emptiness, so a whitespace-only
    // value would otherwise report OK here while the gateway 401s every real request at runtime.

    #[tokio::test]
    async fn check_gateway_auth_fails_when_inline_config_token_is_whitespace_only() {
        let mut config = zeph_core::config::Config::default();
        config.gateway.enabled = true;
        config.gateway.auth_token = Some("   ".into());
        let dir = tempfile::tempdir().unwrap();
        let result = check_gateway_auth(
            &config,
            &dir.path().join("vault-key.txt"),
            &dir.path().join("secrets.age"),
        )
        .await;
        assert_eq!(result.status, CheckStatus::Fail);
    }

    #[tokio::test]
    async fn check_gateway_auth_fails_when_vault_token_is_whitespace_only() {
        let mut config = zeph_core::config::Config::default();
        config.gateway.enabled = true;
        let dir = tempfile::tempdir().unwrap();
        zeph_core::vault::AgeVaultProvider::init_vault(dir.path()).unwrap();
        let key_path = dir.path().join("vault-key.txt");
        let vault_path = dir.path().join("secrets.age");
        let mut provider =
            zeph_core::vault::AgeVaultProvider::load(&key_path, &vault_path).unwrap();
        provider
            .set_secret_mut("ZEPH_GATEWAY_TOKEN".into(), "   ".into(), false)
            .unwrap();
        provider.save().unwrap();

        let result = check_gateway_auth(&config, &key_path, &vault_path).await;
        assert_eq!(result.status, CheckStatus::Fail);
        assert!(result.detail.contains("ZEPH_GATEWAY_TOKEN is not set"));
    }

    #[test]
    fn doctor_fs_check_warns_on_unwritable_skills_dir() {
        // Non-existent directory → NamedTempFile::new_in fails
        let result = check_fs_writable(
            "filesystem.skills",
            Path::new("/nonexistent/path/that/cannot/exist"),
        );
        assert_eq!(result.status, CheckStatus::Fail);
    }

    #[test]
    fn count_skill_files_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(count_skill_files(dir.path()), 0);
    }

    #[test]
    fn count_skill_files_finds_nested() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("SKILL.md"), "").unwrap();
        std::fs::write(dir.path().join("SKILL.md"), "").unwrap();
        assert_eq!(count_skill_files(dir.path()), 2);
    }
}
