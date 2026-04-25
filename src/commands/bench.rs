// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::bootstrap::{
    create_named_provider, create_provider, parse_vault_args, resolve_config_path,
};
use zeph_bench::{
    BenchCommand, BenchRunner, DatasetRegistry, ResultWriter, RunOptions, RunStatus,
    apply_deterministic_overrides,
    loaders::{
        FramesEvaluator, FramesLoader, GaiaEvaluator, GaiaLoader, LocomoEvaluator, LocomoLoader,
    },
};
use zeph_core::config::{Config, SecretResolver as _};

pub(crate) async fn handle_bench_command(
    cmd: &BenchCommand,
    config_path: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    match cmd {
        BenchCommand::List => {
            handle_list();
            Ok(())
        }
        BenchCommand::Download { dataset } => handle_download(dataset),
        BenchCommand::Run {
            dataset,
            output,
            data_file,
            scenario,
            provider: provider_name,
            baseline: _,
            resume,
            no_deterministic,
        } => {
            handle_run(
                dataset,
                output,
                data_file.as_deref(),
                scenario.as_deref(),
                provider_name.as_deref(),
                *resume,
                *no_deterministic,
                config_path,
            )
            .await
        }
        BenchCommand::Show { results } => handle_show(results),
    }
}

fn handle_list() {
    let reg = DatasetRegistry::new();
    println!("{:<16} DESCRIPTION", "NAME");
    for ds in reg.list() {
        println!("{:<16} {}", ds.name, ds.description);
    }
}

fn handle_download(dataset: &str) -> anyhow::Result<()> {
    let reg = DatasetRegistry::new();
    if reg.get(dataset).is_none() {
        eprintln!(
            "error: unknown dataset '{dataset}'. Run `zeph bench list` to see available datasets."
        );
        std::process::exit(1);
    }
    eprintln!("Dataset download is not yet implemented for '{dataset}'.");
    eprintln!("See the dataset URL in `zeph bench list` output for manual download instructions.");
    std::process::exit(1);
}

fn handle_show(results: &std::path::Path) -> anyhow::Result<()> {
    if !results.exists() {
        eprintln!(
            "error: results file '{}' does not exist.",
            results.display()
        );
        std::process::exit(1);
    }
    let data = std::fs::read_to_string(results)?;
    println!("{data}");
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn handle_run(
    dataset: &str,
    output: &std::path::Path,
    data_file: Option<&std::path::Path>,
    scenario: Option<&str>,
    provider_name: Option<&str>,
    resume: bool,
    no_deterministic: bool,
    config_path: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    let reg = DatasetRegistry::new();
    if reg.get(dataset).is_none() {
        eprintln!(
            "error: unknown dataset '{dataset}'. Run `zeph bench list` to see available datasets."
        );
        std::process::exit(1);
    }

    let data_path = resolve_data_path(dataset, data_file);

    let path = resolve_config_path(config_path);
    let mut config = Config::load(&path).unwrap_or_default();

    // Resolve vault secrets before building the provider.
    // The bench command is dispatched before AppBuilder runs, so we must
    // initialize the vault here to populate config.secrets.
    let vault_args = parse_vault_args(&config, None, None, None);
    if let Some(vault) = crate::bootstrap::build_vault_provider(&vault_args)
        && let Err(e) = config.resolve_secrets(vault.as_ref()).await
    {
        tracing::warn!("vault secret resolution failed: {e}");
    }

    let raw_provider = if let Some(name) = provider_name {
        create_named_provider(name, &config)
            .map_err(|e| anyhow::anyhow!("failed to resolve provider '{name}': {e}"))?
    } else {
        create_provider(&config)
            .map_err(|e| anyhow::anyhow!("failed to create default provider: {e}"))?
    };
    let provider = apply_deterministic_overrides(raw_provider, no_deterministic);

    let writer = ResultWriter::new(output)?;
    let completed_ids = if resume {
        writer
            .load_existing()?
            .map(|r| r.completed_ids())
            .unwrap_or_default()
    } else {
        std::collections::HashSet::new()
    };

    let opts = RunOptions {
        scenario_filter: scenario.map(ToOwned::to_owned),
        completed_ids,
    };

    let runner = BenchRunner::new(provider, no_deterministic);
    let mut run = dispatch_run(&runner, dataset, &data_path, opts).await?;

    run.status = RunStatus::Completed;
    run.finished_at = finished_at_now();
    run.recompute_aggregate();
    writer.write(&run)?;

    println!(
        "Benchmark complete: {}/{} exact, mean score {:.4}",
        run.aggregate.exact_match, run.aggregate.total, run.aggregate.mean_score
    );
    println!("Results written to {}", writer.results_path().display());
    println!("Summary written to {}", writer.summary_path().display());
    Ok(())
}

/// Resolve the dataset file path from `--data-file` or exit with a clear error.
fn resolve_data_path(dataset: &str, data_file: Option<&std::path::Path>) -> std::path::PathBuf {
    if let Some(p) = data_file {
        if !p.exists() {
            eprintln!("error: data file '{}' does not exist.", p.display());
            std::process::exit(1);
        }
        return p.to_path_buf();
    }
    eprintln!("error: --data-file <path> is required until automatic download is implemented.");
    eprintln!(
        "Obtain the dataset file from the URL shown by `zeph bench list --dataset {dataset}`."
    );
    std::process::exit(1);
}

/// Dispatch to the correct loader/evaluator pair based on dataset name.
async fn dispatch_run(
    runner: &BenchRunner,
    dataset: &str,
    data_path: &std::path::Path,
    opts: RunOptions,
) -> anyhow::Result<zeph_bench::BenchRun> {
    match dataset {
        "locomo" => Ok(runner
            .run_dataset(&LocomoLoader, &LocomoEvaluator, data_path, opts)
            .await?),
        "gaia" => Ok(runner
            .run_dataset(&GaiaLoader::all_levels(), &GaiaEvaluator, data_path, opts)
            .await?),
        "frames" => Ok(runner
            .run_dataset(&FramesLoader, &FramesEvaluator, data_path, opts)
            .await?),
        other => {
            eprintln!(
                "error: no built-in runner for dataset '{other}'. Supported: locomo, gaia, frames."
            );
            std::process::exit(1);
        }
    }
}

/// Build a simple RFC 3339 timestamp for the `finished_at` field.
fn finished_at_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let (y, mo, d, h, mi, s) = secs_to_ymdhms(secs);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

fn secs_to_ymdhms(secs: u64) -> (u64, u64, u64, u64, u64, u64) {
    const SECS_PER_MIN: u64 = 60;
    const DAYS_PER_400Y: u64 = 146_097;

    let s = secs % SECS_PER_MIN;
    let total_mins = secs / SECS_PER_MIN;
    let mi = total_mins % 60;
    let total_hours = total_mins / 60;
    let h = total_hours % 24;
    let mut days = total_hours / 24;

    days += 719_468;
    let era = days / DAYS_PER_400Y;
    let doe = days % DAYS_PER_400Y;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    (y, mo, d, h, mi, s)
}
