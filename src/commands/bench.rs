// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::bootstrap::{
    create_named_provider, create_provider, parse_vault_args, resolve_config_path,
};
use zeph_bench::{
    BenchCommand, BenchMemoryParams, BenchRun, BenchRunner, DatasetRegistry, MemoryMode,
    ResultWriter, RunOptions, RunStatus, apply_deterministic_overrides,
    baseline::BaselineComparison,
    loaders::{
        AirlineEnv, FramesEvaluator, FramesLoader, GaiaEvaluator, GaiaLoader, LocomoEvaluator,
        LocomoLoader, LongMemEvalEvaluator, LongMemEvalLoader, RetailEnv, Tau2BenchLoader,
        tau2_bench::loader::db_json_path,
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
            baseline,
            resume,
            no_deterministic,
        } => {
            if *baseline {
                handle_run_baseline(
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
            } else {
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
        }
        BenchCommand::Show { results } => handle_show(results),
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_run_baseline(
    dataset: &str,
    output: &std::path::Path,
    data_file: Option<&std::path::Path>,
    scenario: Option<&str>,
    provider_name: Option<&str>,
    resume: bool,
    no_deterministic: bool,
    config_path: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    match dataset {
        "longmemeval" | "locomo" => {}
        "tau2-bench-retail" | "tau2-bench-airline" => {
            anyhow::bail!(
                "--baseline is not supported for tool-driven datasets ({dataset}); \
                 baselines apply only to memory-evaluation datasets (longmemeval, locomo)"
            );
        }
        other => {
            anyhow::bail!(
                "--baseline is supported only for memory-relevant datasets (longmemeval, locomo). \
                 Dataset '{other}' requires tool execution which is not wired in bench mode. \
                 See the issue tracker for tool-executor support."
            );
        }
    }

    let data_path = resolve_data_path(dataset, data_file);
    let path = resolve_config_path(config_path);
    let mut config = Config::load(&path).unwrap_or_default();

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

    let base_completed_ids = if resume {
        let off_writer = ResultWriter::new(output.join("baseline").join("memory-off"))?;
        off_writer
            .load_existing()?
            .map(|r| r.completed_ids())
            .unwrap_or_default()
    } else {
        std::collections::HashSet::new()
    };

    let data_dir = output.join("bench-data");
    std::fs::create_dir_all(&data_dir)?;

    let (off_run, off_writer) = run_memory_off_pass(
        dataset,
        &data_path,
        output,
        scenario,
        base_completed_ids,
        provider.clone(),
        no_deterministic,
    )
    .await?;

    let (on_run, on_writer) = run_memory_on_pass(
        dataset,
        &data_path,
        output,
        scenario,
        &data_dir,
        &config.llm.embedding_model.clone(),
        provider,
        no_deterministic,
    )
    .await?;

    let baseline_dir = output.join("baseline");
    std::fs::create_dir_all(&baseline_dir)?;
    let cmp = BaselineComparison::compute(&on_run, &off_run);
    cmp.write_comparison_json(&baseline_dir)?;
    cmp.write_delta_table(&baseline_dir.join("summary.md"))?;

    println!(
        "Baseline complete: aggregate delta = {:+.4}",
        cmp.aggregate_delta
    );
    println!("Off run: {}", off_writer.results_path().display());
    println!("On  run: {}", on_writer.results_path().display());
    println!(
        "Comparison: {}",
        baseline_dir.join("comparison.json").display()
    );
    Ok(())
}

async fn run_memory_off_pass(
    dataset: &str,
    data_path: &std::path::Path,
    output: &std::path::Path,
    scenario: Option<&str>,
    completed_ids: std::collections::HashSet<String>,
    provider: zeph_llm::any::AnyProvider,
    no_deterministic: bool,
) -> anyhow::Result<(BenchRun, ResultWriter)> {
    let dir = output.join("baseline").join("memory-off");
    let writer = ResultWriter::new(&dir)?;
    let opts = RunOptions {
        scenario_filter: scenario.map(ToOwned::to_owned),
        completed_ids,
        memory_mode: MemoryMode::Off,
    };
    let runner = BenchRunner::new(provider, no_deterministic);
    let mut run = dispatch_run(&runner, dataset, data_path, opts).await?;
    run.status = RunStatus::Completed;
    run.finished_at = finished_at_now();
    run.run_id = format!("bench-off-{}", baseline_run_id_suffix());
    run.recompute_aggregate();
    writer.write(&run)?;
    println!(
        "Memory-off pass complete: mean score {:.4} ({}/{} exact)",
        run.aggregate.mean_score, run.aggregate.exact_match, run.aggregate.total
    );
    Ok((run, writer))
}

#[allow(clippy::too_many_arguments)]
async fn run_memory_on_pass(
    dataset: &str,
    data_path: &std::path::Path,
    output: &std::path::Path,
    scenario: Option<&str>,
    data_dir: &std::path::Path,
    embedding_model: &str,
    provider: zeph_llm::any::AnyProvider,
    no_deterministic: bool,
) -> anyhow::Result<(BenchRun, ResultWriter)> {
    let run_id = format!("bench-on-{}", baseline_run_id_suffix());
    let memory_params = BenchMemoryParams {
        data_dir: data_dir.to_path_buf(),
        embedding_model: embedding_model.to_owned(),
        run_id: run_id.clone(),
        dataset: dataset.to_owned(),
    };
    let dir = output.join("baseline").join("memory-on");
    let writer = ResultWriter::new(&dir)?;
    let opts = RunOptions {
        scenario_filter: scenario.map(ToOwned::to_owned),
        completed_ids: std::collections::HashSet::new(),
        memory_mode: MemoryMode::On,
    };
    let runner = BenchRunner::new(provider, no_deterministic).with_memory_params(memory_params);
    let mut run = dispatch_run(&runner, dataset, data_path, opts).await?;
    run.status = RunStatus::Completed;
    run.finished_at = finished_at_now();
    run.run_id = run_id;
    run.recompute_aggregate();
    writer.write(&run)?;
    println!(
        "Memory-on pass complete: mean score {:.4} ({}/{} exact)",
        run.aggregate.mean_score, run.aggregate.exact_match, run.aggregate.total
    );
    Ok((run, writer))
}

/// Generate a short unique suffix for baseline run IDs.
fn baseline_run_id_suffix() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    format!("{secs:x}-{ns:x}")
}

fn handle_list() {
    let reg = DatasetRegistry::new();
    println!("{:<16} DESCRIPTION", "NAME");
    for ds in reg.list() {
        println!("{:<16} {}", ds.name, ds.description);
    }
}

fn handle_download(dataset: &str) -> anyhow::Result<()> {
    match dataset {
        "tau2-bench" | "tau2-bench-retail" | "tau2-bench-airline" => {
            return download_tau2_bench();
        }
        _ => {}
    }
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

/// Clone `sierra-research/tau2-bench` and copy the JSON data files for retail and airline.
///
/// Produces:
/// ```text
/// <cache>/zeph-bench/tau2-bench/
/// ├── retail/{tasks.json,db.json,split_tasks.json}
/// └── airline/{tasks.json,db.json,split_tasks.json}
/// ```
///
/// Requires `git` on PATH. No Python execution is needed — tau2-bench stores all
/// task data as pure JSON.
fn download_tau2_bench() -> anyhow::Result<()> {
    let cache = dirs::cache_dir()
        .ok_or_else(|| anyhow::anyhow!("no cache directory found (dirs::cache_dir returned None)"))?
        .join("zeph-bench")
        .join("tau2-bench");

    let repo = cache.join("repo");

    if !repo.exists() {
        // TODO(critic): clone to `repo.tmp` and rename atomically; current logic skips re-clone
        // on partial state. See critic-tau-bench-v2.md S4.
        let repo_str = repo
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("cache path contains non-UTF8 characters"))?;

        let status = std::process::Command::new("git")
            .args([
                "clone",
                "--depth=1",
                "https://github.com/sierra-research/tau2-bench",
                repo_str,
            ])
            .status()
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    anyhow::anyhow!(
                        "git is required for `bench download` but was not found on PATH"
                    )
                } else {
                    anyhow::anyhow!("git clone failed: {e}")
                }
            })?;

        if !status.success() {
            anyhow::bail!("git clone failed with exit code: {:?}", status.code());
        }
    }

    for domain in ["retail", "airline"] {
        let src = repo.join("data/tau2/domains").join(domain);
        let dst = cache.join(domain);
        std::fs::create_dir_all(&dst)?;
        for fname in ["tasks.json", "db.json", "split_tasks.json"] {
            std::fs::copy(src.join(fname), dst.join(fname))
                .map_err(|e| anyhow::anyhow!("copy {domain}/{fname}: {e}"))?;
        }
    }

    println!("tau2-bench data ready at {}", cache.display());
    println!(
        "Run: zeph bench run --dataset tau2-bench-retail --data-file {}/retail/tasks.json --output <dir>",
        cache.display()
    );
    Ok(())
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
        memory_mode: MemoryMode::Off,
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
        "longmemeval" => Ok(runner
            .run_dataset(&LongMemEvalLoader, &LongMemEvalEvaluator, data_path, opts)
            .await?),
        "tau2-bench-retail" => {
            let db = db_json_path(data_path)?;
            let env_factory = |_s: &zeph_bench::Scenario| RetailEnv::new_from_seed(&db);
            Ok(runner
                .run_dataset_with_env_factory(
                    &Tau2BenchLoader::retail(),
                    env_factory,
                    data_path,
                    opts,
                )
                .await?)
        }
        "tau2-bench-airline" => {
            let db = db_json_path(data_path)?;
            let env_factory = |_s: &zeph_bench::Scenario| AirlineEnv::new_from_seed(&db);
            Ok(runner
                .run_dataset_with_env_factory(
                    &Tau2BenchLoader::airline(),
                    env_factory,
                    data_path,
                    opts,
                )
                .await?)
        }
        other => {
            eprintln!(
                "error: no built-in runner for dataset '{other}'. \
                 Supported: locomo, gaia, frames, longmemeval, tau2-bench-retail, tau2-bench-airline."
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
