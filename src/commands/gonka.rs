// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `zeph gonka doctor` — Gonka network connectivity and credential diagnostics.
//!
//! Checks vault key resolution, signer construction, and per-node HTTP reachability.
//! Exit code is 0 if no failures, 1 otherwise.

use std::collections::HashSet;
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::RwLock;
use tokio::task::JoinSet;
use tracing::Instrument as _;
use zeph_config::ProviderKind;
use zeph_core::redact::scrub_content;
use zeph_core::vault::{AgeVaultProvider, ArcAgeVaultProvider, VaultProvider};
use zeph_llm::gonka::RequestSigner;

use crate::commands::doctor::{CheckResult, CheckStatus, DoctorReport};

fn elapsed_ms(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn finish(report: &DoctorReport, json: bool) -> anyhow::Result<i32> {
    let mut out = io::stdout();
    if json {
        report.render_json(&mut out)?;
    } else {
        report.render_plain(&mut out)?;
    }
    let has_fail = report.results.iter().any(|r| r.status == CheckStatus::Fail);
    Ok(i32::from(has_fail))
}

/// Attempt to build a vault provider and resolve the gonka secrets.
///
/// Returns `(private_key_result, private_key_opt, address_result, address_opt)`.
/// `address_opt` is the raw vault-stored bech32 address, used to verify against the derived one.
async fn resolve_vault_secrets(
    config: &zeph_core::config::Config,
) -> (CheckResult, Option<String>, CheckResult, Option<String>) {
    let _span = tracing::info_span!("cli.gonka.doctor.vault").entered();
    let vault_args = crate::bootstrap::parse_vault_args(config, None, None, None);

    let vault: Box<dyn VaultProvider> = match vault_args.backend.as_str() {
        "age" => {
            let (Some(key), Some(path)) = (
                vault_args.key_path.as_deref(),
                vault_args.vault_path.as_deref(),
            ) else {
                let start = Instant::now();
                let r = CheckResult::fail(
                    "gonka.vault.private_key",
                    "age vault paths not configured; run `zeph vault init`",
                    elapsed_ms(start),
                );
                let addr_r =
                    CheckResult::fail("gonka.vault.address", "skipped (vault unavailable)", 0);
                return (r, None, addr_r, None);
            };
            match AgeVaultProvider::new(Path::new(key), Path::new(path)) {
                Ok(p) => Box::new(ArcAgeVaultProvider(Arc::new(RwLock::new(p)))),
                Err(e) => {
                    let start = Instant::now();
                    let r = CheckResult::fail(
                        "gonka.vault.private_key",
                        format!("vault open failed: {e}; run `zeph vault init`"),
                        elapsed_ms(start),
                    );
                    let addr_r =
                        CheckResult::fail("gonka.vault.address", "skipped (vault unavailable)", 0);
                    return (r, None, addr_r, None);
                }
            }
        }
        #[cfg(feature = "env-vault")]
        "env" => Box::new(zeph_core::vault::EnvVaultProvider),
        _ => {
            let start = Instant::now();
            let r = CheckResult::warn(
                "gonka.vault.private_key",
                format!(
                    "unknown vault backend '{}'; cannot resolve secrets",
                    vault_args.backend
                ),
                elapsed_ms(start),
            );
            let addr_r = CheckResult::warn("gonka.vault.address", "skipped (unknown backend)", 0);
            return (r, None, addr_r, None);
        }
    };

    // Resolve private key
    let start_key = Instant::now();
    let priv_key_opt = vault
        .get_secret("ZEPH_GONKA_PRIVATE_KEY")
        .await
        .ok()
        .flatten();
    let priv_key_result = if priv_key_opt.is_some() {
        CheckResult::ok(
            "gonka.vault.private_key",
            "ZEPH_GONKA_PRIVATE_KEY present",
            elapsed_ms(start_key),
        )
    } else {
        CheckResult::fail(
            "gonka.vault.private_key",
            "ZEPH_GONKA_PRIVATE_KEY not found in vault; run `zeph vault set ZEPH_GONKA_PRIVATE_KEY <hex>`",
            elapsed_ms(start_key),
        )
    };

    // Resolve address (optional)
    let start_addr = Instant::now();
    let addr_opt = vault.get_secret("ZEPH_GONKA_ADDRESS").await.ok().flatten();
    let addr_result = if addr_opt.is_some() {
        CheckResult::ok(
            "gonka.vault.address",
            "ZEPH_GONKA_ADDRESS present",
            elapsed_ms(start_addr),
        )
    } else {
        CheckResult::warn(
            "gonka.vault.address",
            "ZEPH_GONKA_ADDRESS not set; derived address will be used",
            elapsed_ms(start_addr),
        )
    };

    (priv_key_result, priv_key_opt, addr_result, addr_opt)
}

/// Build a probe request body for `/chat/completions`.
fn build_probe_body(model: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "model": model,
        "messages": [{"role": "user", "content": "ping"}],
        "max_tokens": 1,
    }))
    .expect("static JSON body serialization never fails")
}

/// Probe a single Gonka node with a signed POST to `/chat/completions`.
///
/// Returns a `CheckResult` with HTTP status and latency. On 401 responses,
/// checks the `Date` response header for clock skew > 30 seconds.
/// The caller instruments this future with a tracing span.
async fn probe_node(
    check_name: String,
    node_url: String,
    node_label: String,
    model: String,
    signer: Arc<RequestSigner>,
    client: Arc<reqwest::Client>,
    timeout_secs: u64,
) -> (String, CheckResult) {
    let start = Instant::now();
    let body_bytes = build_probe_body(&model);

    let timestamp_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();

    let sig = match signer.sign(&body_bytes, timestamp_ns, signer.address()) {
        Ok(s) => s,
        Err(e) => {
            return (
                check_name.clone(),
                CheckResult::fail(
                    &check_name,
                    scrub_content(&format!("{node_label}: signing failed: {e}")).into_owned(),
                    elapsed_ms(start),
                ),
            );
        }
    };

    let url = format!("{}/chat/completions", node_url.trim_end_matches('/'));

    let request = client
        .post(&url)
        .header("Content-Type", "application/json")
        .header("X-Timestamp", timestamp_ns.to_string())
        .header("X-Signature", &sig)
        .header("X-Address", signer.address())
        .body(body_bytes);

    let result = tokio::time::timeout(Duration::from_secs(timeout_secs), request.send()).await;
    let check_result = classify_probe_result(result, &check_name, &node_label, timeout_secs, start);
    (check_name, check_result)
}

/// Turn a raw reqwest result into a `CheckResult`.
fn classify_probe_result(
    result: Result<Result<reqwest::Response, reqwest::Error>, tokio::time::error::Elapsed>,
    check_name: &str,
    node_label: &str,
    timeout_secs: u64,
    start: Instant,
) -> CheckResult {
    match result {
        Err(_) => CheckResult::fail(
            check_name,
            format!("{node_label}: timed out after {timeout_secs}s"),
            elapsed_ms(start),
        ),
        Ok(Err(e)) => {
            let msg = if e.is_connect() {
                format!("{node_label}: connection refused or DNS resolution failed")
            } else {
                format!(
                    "{node_label}: request error: {}",
                    scrub_content(&e.to_string())
                )
            };
            CheckResult::fail(check_name, msg, elapsed_ms(start))
        }
        Ok(Ok(resp)) => {
            let status = resp.status();
            let headers = resp.headers();
            classify_http_response(status, headers, check_name, node_label, start)
        }
    }
}

/// Classify an HTTP response status + headers into a `CheckResult`.
fn classify_http_response(
    status: reqwest::StatusCode,
    headers: &reqwest::header::HeaderMap,
    check_name: &str,
    node_label: &str,
    start: Instant,
) -> CheckResult {
    let latency = elapsed_ms(start);

    if status.is_success() {
        return CheckResult::ok(
            check_name,
            format!("{node_label}: HTTP {} ({latency} ms)", status.as_u16()),
            latency,
        );
    }

    if status.as_u16() == 401 {
        if let Some(skew_msg) = headers
            .get(reqwest::header::DATE)
            .and_then(|v| v.to_str().ok())
            .and_then(detect_clock_skew)
        {
            return CheckResult::warn(
                check_name,
                format!("{node_label}: auth rejected — {skew_msg}"),
                latency,
            );
        }
        return CheckResult::fail(
            check_name,
            format!("{node_label}: HTTP 401 auth rejected (check private key or node address)"),
            latency,
        );
    }

    CheckResult::warn(
        check_name,
        format!("{node_label}: HTTP {} ({latency} ms)", status.as_u16()),
        latency,
    )
}

/// Parse an HTTP `Date` header and return a clock skew description if |delta| > 30s.
fn detect_clock_skew(date_str: &str) -> Option<String> {
    let server_time = chrono::DateTime::parse_from_rfc2822(date_str)
        .ok()
        .map(|dt| dt.timestamp())?;
    let local_time = chrono::Utc::now().timestamp();
    let delta = local_time - server_time;
    if delta.unsigned_abs() <= 30 {
        return None;
    }
    let direction = if delta > 0 { "ahead of" } else { "behind" };
    Some(format!(
        "clock skew detected: local is {}s {direction} server",
        delta.unsigned_abs()
    ))
}

/// Probe all nodes from all gonka providers concurrently, deduplicating by URL.
///
/// Uses a `JoinSet` so all probes run in parallel. Results are re-ordered by
/// `node_index` before being appended to `results`.
async fn probe_all_nodes(
    gonka_providers: &[&zeph_config::ProviderEntry],
    signer: Arc<RequestSigner>,
    client: Arc<reqwest::Client>,
    timeout_secs: u64,
    results: &mut Vec<CheckResult>,
) {
    let mut seen_urls: HashSet<String> = HashSet::new();
    let mut set: JoinSet<(usize, String, CheckResult)> = JoinSet::new();
    let mut node_index = 0usize;

    for entry in gonka_providers {
        if entry.gonka_nodes.is_empty() {
            let start = Instant::now();
            let name = entry.name.as_deref().unwrap_or("<unnamed>");
            results.push(CheckResult::warn(
                format!("gonka.node.{name}"),
                "provider has no gonka_nodes configured",
                elapsed_ms(start),
            ));
            continue;
        }

        let model = entry.model.as_deref().unwrap_or("gpt-4o").to_owned();

        for node in &entry.gonka_nodes {
            if !seen_urls.insert(node.url.clone()) {
                continue;
            }
            let idx = node_index;
            node_index += 1;

            let label = node.name.clone().unwrap_or_else(|| node.url.clone());
            let check_name = format!("gonka.node[{idx}]");
            let node_url = node.url.clone();
            let span_url = node_url.clone();
            let signer = Arc::clone(&signer);
            let client = Arc::clone(&client);
            let model = model.clone();

            set.spawn(
                async move {
                    let (name, result) = probe_node(
                        check_name,
                        node_url,
                        label,
                        model,
                        signer,
                        client,
                        timeout_secs,
                    )
                    .await;
                    (idx, name, result)
                }
                .instrument(tracing::info_span!("cli.gonka.doctor.probe", node = %span_url)),
            );
        }
    }

    // Collect and re-order by node_index for deterministic output
    let mut indexed: Vec<(usize, String, CheckResult)> = set.join_all().await;
    indexed.sort_by_key(|(i, _, _)| *i);
    results.extend(indexed.into_iter().map(|(_, _, r)| r));
}

/// Run Gonka doctor diagnostics.
///
/// # Errors
///
/// Returns an error if config parsing or I/O fails at the top level.
#[allow(clippy::too_many_lines)]
pub(crate) async fn run_gonka_doctor(
    config_path: &Path,
    json: bool,
    timeout_secs: u64,
) -> anyhow::Result<i32> {
    let _span = tracing::info_span!("cli.gonka.doctor").entered();
    let total_start = Instant::now();
    let mut results: Vec<CheckResult> = Vec::new();

    // 1. Config parse + find gonka providers
    let start = Instant::now();
    let config = match zeph_core::config::Config::load(config_path) {
        Ok(c) => {
            results.push(CheckResult::ok(
                "gonka.config",
                format!("loaded {}", config_path.display()),
                elapsed_ms(start),
            ));
            c
        }
        Err(e) => {
            results.push(CheckResult::fail(
                "gonka.config",
                format!("failed to load config: {e}"),
                elapsed_ms(start),
            ));
            let report = DoctorReport {
                results,
                elapsed_ms: elapsed_ms(total_start),
            };
            return finish(&report, json);
        }
    };

    let gonka_providers: Vec<&zeph_config::ProviderEntry> = config
        .llm
        .providers
        .iter()
        .filter(|e| e.provider_type == ProviderKind::Gonka)
        .collect();

    if gonka_providers.is_empty() {
        let start = Instant::now();
        results.push(CheckResult::warn(
            "gonka.config",
            "no [[llm.providers]] entries with type=\"gonka\" found; nothing to probe",
            elapsed_ms(start),
        ));
        let report = DoctorReport {
            results,
            elapsed_ms: elapsed_ms(total_start),
        };
        return finish(&report, json);
    }

    // 2 + 3. Vault: resolve private key and optional address
    let (priv_key_result, priv_key_opt, addr_result, vault_addr_opt) =
        resolve_vault_secrets(&config).await;
    let priv_key_failed = priv_key_result.status == CheckStatus::Fail;
    results.push(priv_key_result);
    results.push(addr_result);

    if priv_key_failed {
        let report = DoctorReport {
            results,
            elapsed_ms: elapsed_ms(total_start),
        };
        return finish(&report, json);
    }

    let priv_key = priv_key_opt.expect("priv_key_opt is Some when !priv_key_failed");

    // 4. Signer construction + optional address mismatch check
    let chain_prefix = gonka_providers.first().map_or_else(
        || "gonka".to_owned(),
        |e| e.effective_gonka_chain_prefix().to_owned(),
    );

    let start = Instant::now();
    let signer = match RequestSigner::from_hex(&priv_key, &chain_prefix) {
        Ok(s) => {
            results.push(CheckResult::ok(
                "gonka.signer",
                format!("derived address: {}", s.address()),
                elapsed_ms(start),
            ));
            s
        }
        Err(e) => {
            results.push(CheckResult::fail(
                "gonka.signer",
                format!("key parse failed: {e}"),
                elapsed_ms(start),
            ));
            let report = DoctorReport {
                results,
                elapsed_ms: elapsed_ms(total_start),
            };
            return finish(&report, json);
        }
    };

    // If vault stored an explicit address, verify it matches the derived one.
    if let Some(ref vault_addr) = vault_addr_opt {
        let derived = signer.address();
        if vault_addr != derived {
            let start = Instant::now();
            results.push(CheckResult::fail(
                "gonka.signer",
                format!("vault address does not match derived address: vault={vault_addr}, derived={derived}"),
                elapsed_ms(start),
            ));
            let report = DoctorReport {
                results,
                elapsed_ms: elapsed_ms(total_start),
            };
            return finish(&report, json);
        }
    }

    // 5. Build shared HTTP client (once, not per probe)
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(timeout_secs))
        .build()
    {
        Ok(c) => Arc::new(c),
        Err(e) => {
            tracing::warn!(error = %e, "gonka: client build failed");
            results.push(CheckResult::fail(
                "gonka.client",
                "HTTP client build failed",
                0,
            ));
            let report = DoctorReport {
                results,
                elapsed_ms: elapsed_ms(total_start),
            };
            return finish(&report, json);
        }
    };

    // 6. Per-node probes — concurrent via JoinSet
    let signer = Arc::new(signer);
    probe_all_nodes(&gonka_providers, signer, client, timeout_secs, &mut results).await;

    let report = DoctorReport {
        results,
        elapsed_ms: elapsed_ms(total_start),
    };
    finish(&report, json)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "gonka")]
    #[test]
    fn gonka_doctor_cli_parses() {
        use crate::cli::{Cli, Command, GonkaCommand};
        use clap::Parser;

        let cli = Cli::try_parse_from(["zeph", "gonka", "doctor"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Gonka {
                command: GonkaCommand::Doctor {
                    json: false,
                    timeout_secs: 10
                }
            })
        ));
    }

    #[cfg(feature = "gonka")]
    #[test]
    fn gonka_doctor_cli_parses_json_flag() {
        use crate::cli::{Cli, Command, GonkaCommand};
        use clap::Parser;

        let cli = Cli::try_parse_from(["zeph", "gonka", "doctor", "--json"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Gonka {
                command: GonkaCommand::Doctor { json: true, .. }
            })
        ));
    }

    #[test]
    fn gonka_detect_clock_skew_none_within_threshold() {
        let now = chrono::Utc::now();
        let date_str = now.format("%a, %d %b %Y %H:%M:%S GMT").to_string();
        assert!(detect_clock_skew(&date_str).is_none());
    }

    #[test]
    fn gonka_detect_clock_skew_detects_large_delta() {
        let past = chrono::Utc::now() - chrono::Duration::seconds(120);
        let date_str = past.format("%a, %d %b %Y %H:%M:%S GMT").to_string();
        let result = detect_clock_skew(&date_str);
        assert!(result.is_some(), "expected skew detection for 120s delta");
        let msg = result.unwrap();
        assert!(msg.contains("clock skew"), "unexpected: {msg}");
    }

    #[test]
    fn gonka_detect_clock_skew_returns_none_for_invalid_date() {
        assert!(detect_clock_skew("not a date").is_none());
    }
}
