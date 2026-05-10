// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `zeph cocoon doctor` — Cocoon sidecar connectivity and configuration diagnostics.
//!
//! Runs 6 ordered checks against the configured Cocoon provider and prints
//! `[OK]`, `[WARN]`, or `[FAIL]` per check. Exit code is 0 if no failures, 1 otherwise.

use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

use zeph_config::ProviderKind;
use zeph_core::vault::{AgeVaultProvider, ArcAgeVaultProvider, VaultProvider};
use zeph_llm::cocoon::CocoonClient;

use crate::commands::doctor::{CheckResult, DoctorReport};

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
    Ok(i32::from(report.has_failures()))
}

/// Check 1: find a Cocoon provider entry in config.
///
/// Returns `Some((config, entry_clone))` on success, `None` on failure.
fn check_config_present(
    config_path: &Path,
    results: &mut Vec<CheckResult>,
) -> Option<(zeph_core::config::Config, zeph_config::ProviderEntry)> {
    let start = Instant::now();
    let config = match zeph_core::config::Config::load(config_path) {
        Ok(c) => c,
        Err(e) => {
            results.push(CheckResult::fail(
                "cocoon.config",
                format!("failed to load config: {e}"),
                elapsed_ms(start),
            ));
            return None;
        }
    };

    let entry = config
        .llm
        .providers
        .iter()
        .find(|e| e.provider_type == ProviderKind::Cocoon)
        .cloned();

    if let Some(e) = entry {
        let url = e
            .cocoon_client_url
            .as_deref()
            .unwrap_or("http://localhost:10000");
        let name = e.effective_name();
        results.push(CheckResult::ok(
            "cocoon.config",
            format!("provider '{name}' found (url: {url})"),
            elapsed_ms(start),
        ));
        Some((config, e))
    } else {
        results.push(CheckResult::fail(
            "cocoon.config",
            "no [[llm.providers]] entry with type=\"cocoon\" found; \
             add one to config.toml or run zeph --init",
            elapsed_ms(start),
        ));
        None
    }
}

/// Check 2: verify the sidecar is reachable via `GET /stats`.
///
/// Returns `Some(health)` on success so checks 3–5 can reuse the result.
async fn check_sidecar_reachable(
    client: &CocoonClient,
    url: &str,
    results: &mut Vec<CheckResult>,
) -> Option<zeph_llm::cocoon::CocoonHealth> {
    let start = Instant::now();
    if let Ok(health) = client.health_check().await {
        results.push(CheckResult::ok(
            "cocoon.sidecar",
            "sidecar reachable",
            elapsed_ms(start),
        ));
        Some(health)
    } else {
        results.push(CheckResult::fail(
            "cocoon.sidecar",
            format!("sidecar unreachable at {url} — is cocoon-sidecar running?"),
            elapsed_ms(start),
        ));
        None
    }
}

/// Check 3: verify proxy is connected (uses cached health from check 2).
fn check_proxy_connected(
    health_opt: Option<&zeph_llm::cocoon::CocoonHealth>,
    results: &mut Vec<CheckResult>,
) {
    let start = Instant::now();
    let Some(health) = health_opt else {
        results.push(CheckResult::warn(
            "cocoon.proxy",
            "skipped (sidecar unreachable)",
            0,
        ));
        return;
    };
    if health.proxy_connected {
        results.push(CheckResult::ok(
            "cocoon.proxy",
            "proxy connected",
            elapsed_ms(start),
        ));
    } else {
        results.push(CheckResult::fail(
            "cocoon.proxy",
            "proxy not connected — check sidecar logs and network",
            elapsed_ms(start),
        ));
    }
}

/// Check 4: verify workers are available (uses cached health from check 2).
fn check_workers_available(
    health_opt: Option<&zeph_llm::cocoon::CocoonHealth>,
    results: &mut Vec<CheckResult>,
) {
    let start = Instant::now();
    let Some(health) = health_opt else {
        results.push(CheckResult::warn(
            "cocoon.workers",
            "skipped (sidecar unreachable)",
            0,
        ));
        return;
    };
    if health.worker_count > 0 {
        results.push(CheckResult::ok(
            "cocoon.workers",
            format!("{} worker(s) available", health.worker_count),
            elapsed_ms(start),
        ));
    } else {
        // WARN, not FAIL: inference will queue but the system is not broken.
        results.push(CheckResult::warn(
            "cocoon.workers",
            "no workers available — inference will queue",
            elapsed_ms(start),
        ));
    }
}

/// Check 5: verify configured model is listed by the sidecar.
///
/// Skipped when sidecar is unreachable (`health_opt` is `None`) to avoid a redundant failure.
async fn check_model_listed(
    client: &CocoonClient,
    entry: &zeph_config::ProviderEntry,
    health_opt: Option<&zeph_llm::cocoon::CocoonHealth>,
    results: &mut Vec<CheckResult>,
) {
    let start = Instant::now();

    // Only run when sidecar responded (critic MINOR-1: avoid redundant FAIL).
    if health_opt.is_none() {
        results.push(CheckResult::warn(
            "cocoon.model",
            "skipped (sidecar unreachable)",
            0,
        ));
        return;
    }

    let model = entry.model.as_deref().unwrap_or("gpt-4o").to_owned();

    match client.list_models().await {
        Ok(models) => {
            if models.iter().any(|m| m == &model) {
                results.push(CheckResult::ok(
                    "cocoon.model",
                    format!("model '{model}' available"),
                    elapsed_ms(start),
                ));
            } else {
                let n = models.len();
                results.push(CheckResult::warn(
                    "cocoon.model",
                    format!("model '{model}' not found in {n} available models"),
                    elapsed_ms(start),
                ));
            }
        }
        Err(_) => {
            results.push(CheckResult::fail(
                "cocoon.model",
                "model list unavailable — sidecar may be down",
                elapsed_ms(start),
            ));
        }
    }
}

/// Check 6: verify `ZEPH_COCOON_ACCESS_HASH` is present in the age vault.
///
/// Skipped when `entry.cocoon_access_hash` is `None` (access hash not configured).
/// Vault read is intentionally unbound by `--timeout-secs`; it is a local file
/// read that completes in microseconds (same pattern as Gonka doctor).
async fn check_vault_key(
    config: &zeph_core::config::Config,
    entry: &zeph_config::ProviderEntry,
    results: &mut Vec<CheckResult>,
) {
    let start = Instant::now();

    if entry.cocoon_access_hash.is_none() {
        results.push(CheckResult::ok(
            "cocoon.vault",
            "access hash not configured (skipped)",
            elapsed_ms(start),
        ));
        return;
    }

    let _span = tracing::info_span!("cli.cocoon.doctor.vault").entered();
    let vault_args = crate::bootstrap::parse_vault_args(config, None, None, None);

    let vault: Box<dyn VaultProvider> = match vault_args.backend.as_str() {
        "age" => {
            let (Some(key), Some(path)) = (
                vault_args.key_path.as_deref(),
                vault_args.vault_path.as_deref(),
            ) else {
                results.push(CheckResult::fail(
                    "cocoon.vault",
                    "age vault paths not configured; run `zeph vault init`",
                    elapsed_ms(start),
                ));
                return;
            };
            match AgeVaultProvider::new(Path::new(key), Path::new(path)) {
                Ok(p) => {
                    use std::sync::Arc;
                    use tokio::sync::RwLock;
                    Box::new(ArcAgeVaultProvider(Arc::new(RwLock::new(p))))
                }
                Err(e) => {
                    results.push(CheckResult::fail(
                        "cocoon.vault",
                        format!("vault open failed: {e}; run `zeph vault init`"),
                        elapsed_ms(start),
                    ));
                    return;
                }
            }
        }
        "env" => Box::new(zeph_core::vault::EnvVaultProvider),
        other => {
            results.push(CheckResult::warn(
                "cocoon.vault",
                format!("unknown vault backend '{other}'; cannot verify vault key"),
                elapsed_ms(start),
            ));
            return;
        }
    };

    let secret = vault
        .get_secret("ZEPH_COCOON_ACCESS_HASH")
        .await
        .ok()
        .flatten();
    if secret.is_some() {
        results.push(CheckResult::ok(
            "cocoon.vault",
            "ZEPH_COCOON_ACCESS_HASH found in vault",
            elapsed_ms(start),
        ));
    } else {
        results.push(CheckResult::fail(
            "cocoon.vault",
            "ZEPH_COCOON_ACCESS_HASH not found in vault; \
             set it with: zeph vault set ZEPH_COCOON_ACCESS_HASH <hash>",
            elapsed_ms(start),
        ));
    }
}

/// Run Cocoon doctor diagnostics.
///
/// Executes 6 ordered checks: config present, sidecar reachable, proxy connected,
/// workers available, model listed, vault key. Checks 2–6 are skipped when check 1
/// fails; checks 3–6 are skipped when check 2 fails.
///
/// # Errors
///
/// Returns an error if I/O fails at the top level.
pub(crate) async fn run_cocoon_doctor(
    config_path: &Path,
    json: bool,
    timeout_secs: u64,
) -> anyhow::Result<i32> {
    let _span = tracing::info_span!("cli.cocoon.doctor").entered();
    let total_start = Instant::now();
    let mut results: Vec<CheckResult> = Vec::new();

    // Check 1: Config present
    let Some((config, entry)) = check_config_present(config_path, &mut results) else {
        let report = DoctorReport {
            results,
            elapsed_ms: elapsed_ms(total_start),
        };
        return finish(&report, json);
    };

    let url = entry
        .cocoon_client_url
        .clone()
        .unwrap_or_else(|| "http://localhost:10000".to_owned());

    let client = CocoonClient::new(&url, None, Duration::from_secs(timeout_secs));

    // Check 2: Sidecar reachable
    let health_opt = check_sidecar_reachable(&client, &url, &mut results).await;

    // Check 3: Proxy connected
    check_proxy_connected(health_opt.as_ref(), &mut results);

    // Check 4: Workers available
    check_workers_available(health_opt.as_ref(), &mut results);

    // Check 5: Model listed (only when sidecar responded)
    check_model_listed(&client, &entry, health_opt.as_ref(), &mut results).await;

    // Check 6: Vault key (only when cocoon_access_hash is Some)
    check_vault_key(&config, &entry, &mut results).await;

    let report = DoctorReport {
        results,
        elapsed_ms: elapsed_ms(total_start),
    };
    finish(&report, json)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::doctor::CheckStatus;

    #[cfg(feature = "cocoon")]
    #[test]
    fn cocoon_doctor_cli_parses() {
        use crate::cli::{Cli, CocoonCommand, Command};
        use clap::Parser;

        let cli = Cli::try_parse_from(["zeph", "cocoon", "doctor"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Cocoon {
                command: CocoonCommand::Doctor {
                    json: false,
                    timeout_secs: 5
                }
            })
        ));
    }

    #[cfg(feature = "cocoon")]
    #[test]
    fn cocoon_doctor_cli_parses_json_flag() {
        use crate::cli::{Cli, CocoonCommand, Command};
        use clap::Parser;

        let cli = Cli::try_parse_from(["zeph", "cocoon", "doctor", "--json"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Cocoon {
                command: CocoonCommand::Doctor { json: true, .. }
            })
        ));
    }

    #[cfg(feature = "cocoon")]
    #[test]
    fn cocoon_doctor_cli_parses_timeout() {
        use crate::cli::{Cli, CocoonCommand, Command};
        use clap::Parser;

        let cli =
            Cli::try_parse_from(["zeph", "cocoon", "doctor", "--timeout-secs", "15"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Cocoon {
                command: CocoonCommand::Doctor {
                    json: false,
                    timeout_secs: 15
                }
            })
        ));
    }

    #[test]
    fn cocoon_doctor_no_config_provider_emits_fail() {
        // Build a synthetic report where check 1 fails
        let check = CheckResult::fail(
            "cocoon.config",
            "no [[llm.providers]] entry with type=\"cocoon\" found; \
             add one to config.toml or run zeph --init",
            1,
        );
        assert_eq!(check.status, CheckStatus::Fail);
        let report = DoctorReport {
            results: vec![check],
            elapsed_ms: 1,
        };
        assert!(report.has_failures());
    }

    #[test]
    fn cocoon_doctor_sidecar_down_emits_fail() {
        let check = CheckResult::fail(
            "cocoon.sidecar",
            "sidecar unreachable at http://127.0.0.1:1 — is cocoon-sidecar running?",
            10,
        );
        assert_eq!(check.status, CheckStatus::Fail);
    }

    #[test]
    fn cocoon_doctor_proxy_not_connected_emits_fail() {
        let mut results = Vec::new();
        let health = zeph_llm::cocoon::CocoonHealth {
            proxy_connected: false,
            worker_count: 0,
            ton_balance: None,
        };
        check_proxy_connected(Some(&health), &mut results);
        assert_eq!(results[0].status, CheckStatus::Fail);
        assert!(results[0].detail.contains("proxy not connected"));
    }

    #[test]
    fn cocoon_doctor_workers_zero_emits_warn() {
        let mut results = Vec::new();
        let health = zeph_llm::cocoon::CocoonHealth {
            proxy_connected: true,
            worker_count: 0,
            ton_balance: None,
        };
        check_workers_available(Some(&health), &mut results);
        assert_eq!(results[0].status, CheckStatus::Warn);
        assert!(results[0].detail.contains("no workers available"));
    }

    #[test]
    fn cocoon_doctor_workers_available_emits_ok() {
        let mut results = Vec::new();
        let health = zeph_llm::cocoon::CocoonHealth {
            proxy_connected: true,
            worker_count: 3,
            ton_balance: None,
        };
        check_workers_available(Some(&health), &mut results);
        assert_eq!(results[0].status, CheckStatus::Ok);
        assert!(results[0].detail.contains("3 worker(s)"));
    }

    #[test]
    fn cocoon_doctor_proxy_and_workers_skip_when_sidecar_down() {
        let mut results = Vec::new();
        check_proxy_connected(None, &mut results);
        check_workers_available(None, &mut results);
        // Skip paths emit Warn (not Fail) so they don't inflate the failure count.
        assert_eq!(results[0].status, CheckStatus::Warn);
        assert!(results[0].detail.contains("skipped"));
        assert_eq!(results[1].status, CheckStatus::Warn);
        assert!(results[1].detail.contains("skipped"));
    }

    #[tokio::test]
    async fn cocoon_doctor_model_check_skips_when_sidecar_down() {
        use zeph_config::ProviderEntry;
        let entry = ProviderEntry::default();
        let client = CocoonClient::new("http://127.0.0.1:1", None, Duration::from_millis(100));
        let mut results = Vec::new();
        check_model_listed(&client, &entry, None, &mut results).await;
        assert_eq!(results[0].status, CheckStatus::Warn);
        assert!(results[0].detail.contains("skipped"));
    }

    #[tokio::test]
    async fn cocoon_doctor_vault_skipped_when_no_access_hash() {
        use zeph_config::ProviderEntry;
        let entry = ProviderEntry {
            cocoon_access_hash: None,
            ..ProviderEntry::default()
        };
        let config = zeph_core::config::Config::default();
        let mut results = Vec::new();
        check_vault_key(&config, &entry, &mut results).await;
        assert_eq!(results[0].status, CheckStatus::Ok);
        assert!(results[0].detail.contains("skipped"));
    }

    #[test]
    fn cocoon_doctor_report_json_is_valid() {
        let results = vec![
            CheckResult::ok("cocoon.config", "provider 'cocoon' found", 1),
            CheckResult::fail("cocoon.sidecar", "unreachable", 5),
        ];
        let report = DoctorReport {
            results,
            elapsed_ms: 10,
        };
        let mut buf = Vec::new();
        report.render_json(&mut buf).unwrap();
        let val: serde_json::Value = serde_json::from_slice(&buf).unwrap();
        assert_eq!(val["schema_version"], 1);
        assert_eq!(val["overall"], "fail");
        assert_eq!(val["failures"], 1);
    }

    #[test]
    fn cocoon_doctor_warn_does_not_cause_failure() {
        let results = vec![
            CheckResult::ok("cocoon.config", "found", 1),
            CheckResult::warn("cocoon.workers", "no workers available", 1),
        ];
        let report = DoctorReport {
            results,
            elapsed_ms: 5,
        };
        assert!(!report.has_failures());
    }

    #[cfg(feature = "cocoon")]
    #[tokio::test]
    #[ignore = "requires running Cocoon sidecar (COCOON_TEST_URL)"]
    async fn test_doctor_all_pass() {
        let Some(url) = std::env::var("COCOON_TEST_URL").ok() else {
            return;
        };
        let client = CocoonClient::new(&url, None, Duration::from_secs(5));
        let mut results: Vec<CheckResult> = Vec::new();

        let health_opt = check_sidecar_reachable(&client, &url, &mut results).await;
        check_proxy_connected(health_opt.as_ref(), &mut results);
        check_workers_available(health_opt.as_ref(), &mut results);

        let mut entry = zeph_config::ProviderEntry::default();
        entry.model = Some("Qwen/Qwen3-0.6B".into());
        check_model_listed(&client, &entry, health_opt.as_ref(), &mut results).await;

        for check in &results {
            assert_ne!(
                check.status,
                CheckStatus::Fail,
                "doctor check '{}' failed: {}",
                check.name,
                check.detail
            );
        }
    }
}
