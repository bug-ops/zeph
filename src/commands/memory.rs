// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::cli::MemoryCommand;

pub(crate) async fn handle_memory_command(
    cmd: MemoryCommand,
    config_path: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    use zeph_core::bootstrap::resolve_config_path;
    use zeph_memory::store::SqliteStore;

    let config_file = resolve_config_path(config_path);
    let config = zeph_core::config::Config::load(&config_file).unwrap_or_default();
    let sqlite = SqliteStore::new(crate::db_url::resolve_db_url(&config))
        .await
        .map_err(|e| anyhow::anyhow!("failed to open SQLite: {e}"))?;

    match cmd {
        MemoryCommand::Export { path } => {
            let snapshot = zeph_memory::export_snapshot(&sqlite)
                .await
                .map_err(|e| anyhow::anyhow!("export failed: {e}"))?;
            let json = serde_json::to_string_pretty(&snapshot)
                .map_err(|e| anyhow::anyhow!("serialization failed: {e}"))?;
            std::fs::write(&path, json)
                .map_err(|e| anyhow::anyhow!("failed to write {}: {e}", path.display()))?;
            let convs = snapshot.conversations.len();
            let msgs: usize = snapshot
                .conversations
                .iter()
                .map(|c| c.messages.len())
                .sum();
            println!(
                "Exported {convs} conversation(s) with {msgs} message(s) to {}",
                path.display()
            );
            if config.memory.redact_credentials {
                eprintln!(
                    "Warning: snapshot may contain sensitive conversation data predating \
                     redaction. Store the file securely and restrict access."
                );
            }
        }
        MemoryCommand::Import { path } => {
            let json = std::fs::read_to_string(&path)
                .map_err(|e| anyhow::anyhow!("failed to read {}: {e}", path.display()))?;
            let snapshot: zeph_memory::MemorySnapshot = serde_json::from_str(&json)
                .map_err(|e| anyhow::anyhow!("invalid snapshot format: {e}"))?;
            let stats = zeph_memory::import_snapshot(&sqlite, snapshot)
                .await
                .map_err(|e| anyhow::anyhow!("import failed: {e}"))?;
            println!(
                "Imported: {} conversation(s), {} message(s), {} summary(ies), {} skipped",
                stats.conversations_imported,
                stats.messages_imported,
                stats.summaries_imported,
                stats.skipped,
            );
        }
        MemoryCommand::ForgettingSweep => {
            let forgetting_cfg = zeph_memory::ForgettingConfig {
                enabled: true, // always run when triggered manually
                decay_rate: config.memory.forgetting.decay_rate,
                forgetting_floor: config.memory.forgetting.forgetting_floor,
                sweep_interval_secs: config.memory.forgetting.sweep_interval_secs,
                sweep_batch_size: config.memory.forgetting.sweep_batch_size,
                replay_window_hours: config.memory.forgetting.replay_window_hours,
                replay_min_access_count: config.memory.forgetting.replay_min_access_count,
                protect_recent_hours: config.memory.forgetting.protect_recent_hours,
                protect_min_access_count: config.memory.forgetting.protect_min_access_count,
            };
            let result = zeph_memory::forgetting::run_forgetting_sweep(&sqlite, &forgetting_cfg)
                .await
                .map_err(|e| anyhow::anyhow!("forgetting sweep failed: {e}"))?;
            println!(
                "Forgetting sweep complete: downscaled={} replayed={} pruned={}",
                result.downscaled, result.replayed, result.pruned
            );
        }
        MemoryCommand::PredictorStatus => {
            let sample_count = sqlite
                .count_compression_training_records()
                .await
                .map_err(|e| anyhow::anyhow!("failed to query training records: {e}"))?;
            let weights_json = sqlite
                .load_compression_predictor_weights()
                .await
                .map_err(|e| anyhow::anyhow!("failed to load weights: {e}"))?;
            let min_samples = config.memory.compression.predictor.min_samples;
            println!("Compression predictor status:");
            println!("  Training samples: {sample_count}");
            println!("  Min samples for activation: {min_samples}");
            println!(
                "  Active: {}",
                if sample_count >= 0 && sample_count.unsigned_abs() >= min_samples {
                    "yes"
                } else {
                    "no (cold start)"
                }
            );
            if weights_json.is_some() {
                println!("  Weights: saved");
            } else {
                println!("  Weights: not yet trained");
            }
        }
    }

    Ok(())
}
