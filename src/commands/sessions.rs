// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

#[cfg(feature = "acp")]
use crate::cli::SessionsCommand;

#[cfg(feature = "acp")]
pub(crate) async fn handle_sessions_command(
    cmd: SessionsCommand,
    config_path: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    use zeph_core::bootstrap::resolve_config_path;
    use zeph_core::text::truncate_to_chars;
    use zeph_memory::store::SqliteStore;

    let config_file = resolve_config_path(config_path);
    let config = zeph_core::config::Config::load(&config_file).unwrap_or_default();
    let store = SqliteStore::new(&config.memory.sqlite_path)
        .await
        .map_err(|e| anyhow::anyhow!("failed to open SQLite: {e}"))?;

    match cmd {
        SessionsCommand::List => {
            let limit = config.memory.sessions.max_history;
            let sessions = store
                .list_acp_sessions(limit)
                .await
                .map_err(|e| anyhow::anyhow!("failed to list sessions: {e}"))?;

            if sessions.is_empty() {
                println!("No sessions found.");
                return Ok(());
            }

            println!(
                "{:<38} {:<62} {:<24} {:>5}",
                "ID", "TITLE", "UPDATED", "MSGS"
            );
            println!("{}", "-".repeat(135));
            for s in &sessions {
                let title = s.title.as_deref().unwrap_or("(untitled)");
                let title_display = truncate_to_chars(title, 60);
                println!(
                    "{:<38} {:<62} {:<24} {:>5}",
                    s.id, title_display, s.updated_at, s.message_count
                );
            }
        }

        SessionsCommand::Resume { id } => {
            let exists = store
                .acp_session_exists(&id)
                .await
                .map_err(|e| anyhow::anyhow!("failed to check session: {e}"))?;

            if !exists {
                anyhow::bail!("session not found: {id}");
            }

            let events = store
                .load_acp_events(&id)
                .await
                .map_err(|e| anyhow::anyhow!("failed to load events: {e}"))?;

            println!("Session: {id}");
            println!("{} event(s):", events.len());
            for event in &events {
                println!("[{}] {}", event.event_type, event.payload);
            }
        }

        SessionsCommand::Delete { id } => {
            let exists = store
                .acp_session_exists(&id)
                .await
                .map_err(|e| anyhow::anyhow!("failed to check session: {e}"))?;

            if !exists {
                anyhow::bail!("session not found: {id}");
            }

            store
                .delete_acp_session(&id)
                .await
                .map_err(|e| anyhow::anyhow!("failed to delete session: {e}"))?;

            println!("Deleted session {id}.");
        }
    }

    Ok(())
}
