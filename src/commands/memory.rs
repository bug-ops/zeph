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
    let sqlite = SqliteStore::new(&config.memory.sqlite_path)
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
    }

    Ok(())
}
