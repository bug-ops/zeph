// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

#[cfg(any(feature = "acp", feature = "session"))]
use crate::cli::SessionsCommand;

#[cfg(any(feature = "acp", feature = "session"))]
pub(crate) async fn handle_sessions_command(
    cmd: SessionsCommand,
    config_path: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    use crate::bootstrap::{load_config_or_default, resolve_config_path};
    use zeph_memory::store::SqliteStore;

    let config_file = resolve_config_path(config_path);
    let config = load_config_or_default(&config_file)?;
    let store = SqliteStore::new(crate::db_url::resolve_db_url(&config))
        .await
        .map_err(|e| anyhow::anyhow!("failed to open SQLite: {e}"))?;
    let session_store = zeph_session::SessionStore::new(store.pool().clone());
    let data_dir = std::path::PathBuf::from(&config.session.data_dir);

    match cmd {
        SessionsCommand::List => {
            list_sessions(&session_store, config.memory.sessions.max_history).await
        }
        // `print: false` is intercepted earlier, in `runner::run`, and dispatched to a live
        // interactive agent instead (spec-068 D-6, #5343) — this handler only ever sees
        // `print: true` in practice, but the field stays on the CLI schema either way.
        // `allow_unverified` (issue #6360) applies to this `--print` dump path; the live
        // interactive resume path (intercepted in `runner::run` before reaching this handler)
        // does not yet thread the flag through its bootstrap — tracked as follow-up work (see
        // the implementation handoff for this feature).
        SessionsCommand::Resume {
            id,
            print: _,
            allow_unverified,
        } => print_session_events(&data_dir, &id, None, None, allow_unverified).await,
        SessionsCommand::Show {
            id,
            from,
            to,
            events,
        } => show_session(&session_store, &data_dir, &id, from, to, events).await,
        SessionsCommand::Delete { id } => delete_session(&store, &session_store, &id).await,
        SessionsCommand::Verify { id } => verify_sessions(&session_store, &data_dir, id).await,
        SessionsCommand::Fork { id, at } => {
            fork_session_cli(&session_store, &data_dir, &id, at).await
        }
        SessionsCommand::Export { id, path } => export_session(&data_dir, &id, &path).await,
        SessionsCommand::Import { path } => import_session(&session_store, &data_dir, &path).await,
    }
}

#[cfg(any(feature = "acp", feature = "session"))]
async fn list_sessions(
    session_store: &zeph_session::SessionStore,
    limit: usize,
) -> anyhow::Result<()> {
    use zeph_core::text::truncate_to_chars;

    let sessions = session_store
        .list(&zeph_session::SessionFilter {
            status: None,
            limit,
        })
        .await
        .map_err(|e| anyhow::anyhow!("failed to list sessions: {e}"))?;

    if sessions.is_empty() {
        println!("No sessions found.");
        return Ok(());
    }

    println!(
        "{:<38} {:<40} {:<9} {:>6} {:<38} {:<24}",
        "ID", "TITLE", "STATUS", "EVENTS", "FORKED_FROM", "UPDATED"
    );
    println!("{}", "-".repeat(160));
    for s in &sessions {
        let title = s.title.as_deref().unwrap_or("(untitled)");
        let title_display = truncate_to_chars(title, 38);
        let forked_from = s.forked_from.as_deref().unwrap_or("-");
        println!(
            "{:<38} {:<40} {:<9} {:>6} {:<38} {:<24}",
            s.session_id,
            title_display,
            s.status.as_str(),
            s.event_count,
            forked_from,
            s.updated_at
        );
    }
    Ok(())
}

#[cfg(any(feature = "acp", feature = "session"))]
async fn show_session(
    session_store: &zeph_session::SessionStore,
    data_dir: &std::path::Path,
    id: &str,
    from: Option<u64>,
    to: Option<u64>,
    events: bool,
) -> anyhow::Result<()> {
    let meta = session_store
        .get(id)
        .await
        .map_err(|e| anyhow::anyhow!("failed to look up session: {e}"))?
        .ok_or_else(|| anyhow::anyhow!("session not found: {id}"))?;

    println!("Session:            {}", meta.session_id);
    println!(
        "Title:              {}",
        meta.title.as_deref().unwrap_or("(untitled)")
    );
    println!("Status:             {}", meta.status.as_str());
    println!("Created:            {}", meta.created_at);
    println!("Updated:            {}", meta.updated_at);
    println!(
        "Conversation ID:    {}",
        meta.conversation_id
            .map_or_else(|| "-".to_owned(), |c| c.to_string())
    );
    println!("Last seq:           {}", meta.last_seq);
    println!("Event count:        {}", meta.event_count);
    println!("Last condensed seq: {}", meta.last_condensed_seq);
    println!(
        "Forked from:        {}",
        meta.forked_from.as_deref().unwrap_or("-")
    );
    if let Some(at_seq) = meta.forked_at_seq {
        println!("Forked at seq:      {at_seq}");
    }

    if events {
        println!();
        print_session_events(data_dir, id, from, to, false).await?;
    }

    Ok(())
}

#[cfg(any(feature = "acp", feature = "session"))]
async fn print_session_events(
    data_dir: &std::path::Path,
    id: &str,
    from: Option<u64>,
    to: Option<u64>,
    allow_unverified: bool,
) -> anyhow::Result<()> {
    let session_path = zeph_session::session_dir(data_dir, id);
    let log = if allow_unverified {
        zeph_session::SessionEventLog::open_allow_unverified(&session_path).await
    } else {
        zeph_session::SessionEventLog::open(&session_path).await
    }
    .map_err(|e| anyhow::anyhow!("failed to open session event log: {e}"))?;
    let events = log
        .read_all()
        .await
        .map_err(|e| anyhow::anyhow!("failed to read session event log: {e}"))?;

    let filtered: Vec<_> = events
        .into_iter()
        .filter(|e| from.is_none_or(|f| e.seq >= f) && to.is_none_or(|t| e.seq < t))
        .collect();

    println!("{} event(s):", filtered.len());
    for envelope in &filtered {
        let payload = serde_json::to_string(&envelope.kind).unwrap_or_default();
        println!("[{}] {payload}", envelope.seq);
    }
    Ok(())
}

#[cfg(any(feature = "acp", feature = "session"))]
async fn delete_session(
    store: &zeph_memory::store::SqliteStore,
    session_store: &zeph_session::SessionStore,
    id: &str,
) -> anyhow::Result<()> {
    let exists = session_store
        .get(id)
        .await
        .map_err(|e| anyhow::anyhow!("failed to look up session: {e}"))?
        .is_some()
        || store
            .acp_session_exists(id)
            .await
            .map_err(|e| anyhow::anyhow!("failed to check session: {e}"))?;

    if !exists {
        anyhow::bail!("session not found: {id}");
    }

    store
        .delete_acp_session_checked(id)
        .await
        .map_err(|e| anyhow::anyhow!("failed to delete session: {e}"))?;

    println!("Deleted session {id}.");
    println!(
        "Note: the on-disk event log directory is not removed yet (blob/event-log GC lands in a follow-up)."
    );
    Ok(())
}

/// `sessions verify [id]` (issue #6449) — verify one session's (or every session's) hash chain
/// and vault anchor without resuming it. Uses the lockless
/// [`zeph_session::SessionEventLog::open`] and [`zeph_session::SessionEventLog::read_all`] path,
/// which runs full chain and anchor verification as a side effect of reading (see the
/// module-level anchor docs, `zeph_common::anchor`) — never mutates the session (never
/// `open_exclusive`).
///
/// Prints one `OK`/`TAMPER`/`FAIL` line per session and exits non-zero if any failed.
///
/// # Errors
///
/// Returns an error if the session list cannot be queried, or if one or more sessions fail
/// verification (the summary failure, not a lookup failure).
#[cfg(any(feature = "acp", feature = "session"))]
async fn verify_sessions(
    session_store: &zeph_session::SessionStore,
    data_dir: &std::path::Path,
    id: Option<String>,
) -> anyhow::Result<()> {
    let ids: Vec<String> = if let Some(id) = id {
        vec![id]
    } else {
        let sessions = session_store
            .list(&zeph_session::SessionFilter {
                status: None,
                limit: usize::MAX,
            })
            .await
            .map_err(|e| anyhow::anyhow!("failed to list sessions: {e}"))?;
        sessions.into_iter().map(|s| s.session_id).collect()
    };

    if ids.is_empty() {
        println!("No sessions found.");
        return Ok(());
    }

    let mut failures = 0usize;
    for id in &ids {
        let session_path = zeph_session::session_dir(data_dir, id);
        let outcome = match zeph_session::SessionEventLog::open(&session_path).await {
            Ok(log) => log.read_all().await.map(|events| events.len()),
            Err(e) => Err(e),
        };
        match outcome {
            Ok(count) => println!("OK      {id}  ({count} events)"),
            Err(e @ zeph_session::SessionError::Integrity(_)) => {
                failures += 1;
                println!("TAMPER  {id}: {e}");
            }
            Err(e) => {
                failures += 1;
                println!("FAIL    {id}: {e}");
            }
        }
    }

    if failures > 0 {
        anyhow::bail!("{failures} of {} session(s) failed verification", ids.len());
    }
    println!("All {} session(s) verified OK.", ids.len());
    Ok(())
}

/// `sessions fork <id> [--at <seq>]` — spec-068 P2, #5343.
#[cfg(any(feature = "acp", feature = "session"))]
async fn fork_session_cli(
    session_store: &zeph_session::SessionStore,
    data_dir: &std::path::Path,
    id: &str,
    at: Option<u64>,
) -> anyhow::Result<()> {
    let new_id = zeph_common::SessionId::generate();
    let result =
        zeph_session::ForkEngine::fork(data_dir, id, new_id.as_str(), at, session_store, None)
            .await
            .map_err(|e| anyhow::anyhow!("failed to fork session: {e}"))?;

    println!(
        "Forked session {id} -> {} ({} event(s) copied).",
        result.new_session_id, result.events_copied
    );
    Ok(())
}

/// `sessions export <id> <path.jsonl>` — spec-068 P2, #5343.
///
/// Copies the session's validated (INV-SP-2 torn-tail-truncated) `events.jsonl` file to `path`.
#[cfg(any(feature = "acp", feature = "session"))]
async fn export_session(
    data_dir: &std::path::Path,
    id: &str,
    path: &std::path::Path,
) -> anyhow::Result<()> {
    let session_path = zeph_session::session_dir(data_dir, id);
    let log = zeph_session::SessionEventLog::open(&session_path)
        .await
        .map_err(|e| anyhow::anyhow!("failed to open session event log: {e}"))?;

    tokio::fs::copy(log.path(), path)
        .await
        .map_err(|e| anyhow::anyhow!("failed to write export file: {e}"))?;

    println!("Exported session {id} to {}.", path.display());
    Ok(())
}

/// `sessions import <path.jsonl>` — spec-068 P2, #5343.
///
/// Imports a previously-exported JSONL file as a brand-new session (fresh id, no `forked_from`
/// provenance — an import is a restore, not a fork).
#[cfg(any(feature = "acp", feature = "session"))]
async fn import_session(
    session_store: &zeph_session::SessionStore,
    data_dir: &std::path::Path,
    path: &std::path::Path,
) -> anyhow::Result<()> {
    let new_id = zeph_common::SessionId::generate();
    let child_dir = zeph_session::session_dir(data_dir, new_id.as_str());

    let empty_log = zeph_session::SessionEventLog::open(&child_dir)
        .await
        .map_err(|e| anyhow::anyhow!("failed to create session directory: {e}"))?;
    tokio::fs::copy(path, empty_log.path())
        .await
        .map_err(|e| anyhow::anyhow!("failed to read import file: {e}"))?;
    drop(empty_log);

    // Reopen to validate (INV-SP-2 torn-tail truncation) and read the imported content back.
    let log = zeph_session::SessionEventLog::open(&child_dir)
        .await
        .map_err(|e| anyhow::anyhow!("failed to validate imported event log: {e}"))?;
    let events = log
        .read_all()
        .await
        .map_err(|e| anyhow::anyhow!("failed to read imported event log: {e}"))?;

    session_store
        .create(new_id.as_str())
        .await
        .map_err(|e| anyhow::anyhow!("failed to create imported session: {e}"))?;
    #[allow(clippy::cast_possible_truncation)]
    let event_count = events.len() as u64;
    session_store
        .update_seq(new_id.as_str(), log.last_seq().unwrap_or(0), event_count)
        .await
        .map_err(|e| anyhow::anyhow!("failed to record imported session metadata: {e}"))?;

    println!(
        "Imported {} as new session {} ({event_count} event(s)).",
        path.display(),
        new_id.as_str()
    );
    Ok(())
}

#[cfg(all(test, any(feature = "acp", feature = "session")))]
mod tests {
    use crate::cli::{Cli, Command, SessionsCommand};
    use clap::Parser;

    #[test]
    fn sessions_list_parses() {
        let cli = Cli::try_parse_from(["zeph", "sessions", "list"]).expect("parse");
        assert!(matches!(
            cli.command,
            Some(Command::Sessions {
                command: SessionsCommand::List
            })
        ));
    }

    #[test]
    fn sessions_resume_parses_without_print() {
        let cli = Cli::try_parse_from(["zeph", "sessions", "resume", "abc"]).expect("parse");
        let Some(Command::Sessions {
            command:
                SessionsCommand::Resume {
                    id,
                    print,
                    allow_unverified,
                },
        }) = cli.command
        else {
            panic!("expected Sessions(Resume)");
        };
        assert_eq!(id, "abc");
        assert!(!print);
        assert!(!allow_unverified);
    }

    #[test]
    fn sessions_resume_parses_with_print_flag() {
        let cli =
            Cli::try_parse_from(["zeph", "sessions", "resume", "abc", "--print"]).expect("parse");
        let Some(Command::Sessions {
            command:
                SessionsCommand::Resume {
                    id,
                    print,
                    allow_unverified,
                },
        }) = cli.command
        else {
            panic!("expected Sessions(Resume)");
        };
        assert_eq!(id, "abc");
        assert!(print);
        assert!(!allow_unverified);
    }

    #[test]
    fn sessions_resume_parses_with_allow_unverified_flag() {
        // --allow-unverified requires --print (issue #6360 B3): the interactive resume path
        // doesn't honor the override yet, so clap rejects the flag combination outright rather
        // than accepting a silent no-op.
        let cli = Cli::try_parse_from([
            "zeph",
            "sessions",
            "resume",
            "abc",
            "--print",
            "--allow-unverified",
        ])
        .expect("parse");
        let Some(Command::Sessions {
            command:
                SessionsCommand::Resume {
                    id,
                    allow_unverified,
                    ..
                },
        }) = cli.command
        else {
            panic!("expected Sessions(Resume)");
        };
        assert_eq!(id, "abc");
        assert!(allow_unverified);
    }

    /// Issue #6360 B3: `--allow-unverified` without `--print` must be rejected at arg-parsing
    /// time, not silently accepted and then ignored on the interactive resume path.
    #[test]
    fn sessions_resume_rejects_allow_unverified_without_print() {
        let result =
            Cli::try_parse_from(["zeph", "sessions", "resume", "abc", "--allow-unverified"]);
        let Err(err) = result else {
            panic!("--allow-unverified without --print must be rejected");
        };
        let rendered = err.to_string();
        assert!(
            rendered.contains("--print"),
            "error message should mention the missing required --print flag, got: {rendered}"
        );
    }

    #[test]
    fn sessions_show_parses_with_all_flags() {
        let cli = Cli::try_parse_from([
            "zeph", "sessions", "show", "abc", "--from", "1", "--to", "10", "--events",
        ])
        .expect("parse");
        let Some(Command::Sessions {
            command:
                SessionsCommand::Show {
                    id,
                    from,
                    to,
                    events,
                },
        }) = cli.command
        else {
            panic!("expected Sessions(Show)");
        };
        assert_eq!(id, "abc");
        assert_eq!(from, Some(1));
        assert_eq!(to, Some(10));
        assert!(events);
    }

    #[test]
    fn sessions_show_parses_with_no_optional_flags() {
        let cli = Cli::try_parse_from(["zeph", "sessions", "show", "abc"]).expect("parse");
        let Some(Command::Sessions {
            command:
                SessionsCommand::Show {
                    id,
                    from,
                    to,
                    events,
                },
        }) = cli.command
        else {
            panic!("expected Sessions(Show)");
        };
        assert_eq!(id, "abc");
        assert_eq!(from, None);
        assert_eq!(to, None);
        assert!(!events);
    }

    #[test]
    fn sessions_delete_parses() {
        let cli = Cli::try_parse_from(["zeph", "sessions", "delete", "abc"]).expect("parse");
        let Some(Command::Sessions {
            command: SessionsCommand::Delete { id },
        }) = cli.command
        else {
            panic!("expected Sessions(Delete)");
        };
        assert_eq!(id, "abc");
    }

    #[test]
    fn sessions_fork_parses_without_at() {
        let cli = Cli::try_parse_from(["zeph", "sessions", "fork", "abc"]).expect("parse");
        let Some(Command::Sessions {
            command: SessionsCommand::Fork { id, at },
        }) = cli.command
        else {
            panic!("expected Sessions(Fork)");
        };
        assert_eq!(id, "abc");
        assert_eq!(at, None);
    }

    #[test]
    fn sessions_fork_parses_with_at() {
        let cli =
            Cli::try_parse_from(["zeph", "sessions", "fork", "abc", "--at", "5"]).expect("parse");
        let Some(Command::Sessions {
            command: SessionsCommand::Fork { id, at },
        }) = cli.command
        else {
            panic!("expected Sessions(Fork)");
        };
        assert_eq!(id, "abc");
        assert_eq!(at, Some(5));
    }

    #[test]
    fn sessions_export_parses() {
        let cli =
            Cli::try_parse_from(["zeph", "sessions", "export", "abc", "out.jsonl"]).expect("parse");
        let Some(Command::Sessions {
            command: SessionsCommand::Export { id, path },
        }) = cli.command
        else {
            panic!("expected Sessions(Export)");
        };
        assert_eq!(id, "abc");
        assert_eq!(path, std::path::PathBuf::from("out.jsonl"));
    }

    #[test]
    fn sessions_import_parses() {
        let cli = Cli::try_parse_from(["zeph", "sessions", "import", "in.jsonl"]).expect("parse");
        let Some(Command::Sessions {
            command: SessionsCommand::Import { path },
        }) = cli.command
        else {
            panic!("expected Sessions(Import)");
        };
        assert_eq!(path, std::path::PathBuf::from("in.jsonl"));
    }
}
