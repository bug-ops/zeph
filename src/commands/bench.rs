// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::bootstrap::resolve_config_path;
use zeph_bench::{BenchCommand, DatasetRegistry};
use zeph_core::config::Config;

pub(crate) fn handle_bench_command(
    cmd: &BenchCommand,
    config_path: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    match cmd {
        BenchCommand::List => {
            let reg = DatasetRegistry::new();
            println!("{:<16} DESCRIPTION", "NAME");
            for ds in reg.list() {
                println!("{:<16} {}", ds.name, ds.description);
            }
        }

        BenchCommand::Download { dataset } => {
            let reg = DatasetRegistry::new();
            if reg.get(dataset).is_none() {
                eprintln!(
                    "error: unknown dataset '{dataset}'. Run `zeph bench list` to see available datasets."
                );
                std::process::exit(1);
            }
            eprintln!("Dataset download is not yet implemented for '{dataset}'.");
            eprintln!(
                "See the dataset URL in `zeph bench list` output for manual download instructions."
            );
            std::process::exit(1);
        }

        BenchCommand::Run {
            dataset,
            output: _,
            scenario: _,
            provider: _,
            baseline: _,
            resume: _,
            no_deterministic: _,
        } => {
            let reg = DatasetRegistry::new();
            if reg.get(dataset).is_none() {
                eprintln!(
                    "error: unknown dataset '{dataset}'. Run `zeph bench list` to see available datasets."
                );
                std::process::exit(1);
            }

            let path = resolve_config_path(config_path);
            let _config = Config::load(&path).unwrap_or_default();

            eprintln!(
                "error: dataset '{dataset}' is not downloaded. Run `zeph bench download --dataset {dataset}` first."
            );
            std::process::exit(1);
        }

        BenchCommand::Show { results } => {
            if !results.exists() {
                eprintln!(
                    "error: results file '{}' does not exist.",
                    results.display()
                );
                std::process::exit(1);
            }
            let data = std::fs::read_to_string(results)?;
            println!("{data}");
        }
    }

    Ok(())
}
