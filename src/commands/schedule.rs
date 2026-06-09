// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

#[cfg(feature = "scheduler")]
use crate::cli::ScheduleCommand;

#[cfg(feature = "scheduler")]
fn cron_display(expr: Option<&zeph_scheduler::CronExpr>) -> &str {
    expr.map_or("-", zeph_scheduler::CronExpr::as_str)
}

#[cfg(feature = "scheduler")]
pub(crate) async fn handle_schedule_command(
    cmd: ScheduleCommand,
    config_path: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    use std::str::FromStr as _;

    use crate::bootstrap::{load_config_or_default, resolve_config_path};
    use zeph_scheduler::{JobStore, SchedulerError, normalize_cron_expr, sanitize_task_prompt};

    let config_file = resolve_config_path(config_path);
    let config = load_config_or_default(&config_file);
    let db_url = crate::db_url::resolve_db_url(&config);
    let store = JobStore::open(db_url)
        .await
        .map_err(|e| anyhow::anyhow!("failed to open scheduler store: {e}"))?;
    store
        .init()
        .await
        .map_err(|e| anyhow::anyhow!("failed to init scheduler store: {e}"))?;

    match cmd {
        ScheduleCommand::List => {
            let jobs = store
                .list_jobs_full()
                .await
                .map_err(|e| anyhow::anyhow!("failed to list jobs: {e}"))?;

            if jobs.is_empty() {
                println!("No scheduled jobs.");
                return Ok(());
            }

            println!(
                "{:<32} {:<16} {:<10} {:<22} CRON",
                "NAME", "KIND", "MODE", "NEXT RUN"
            );
            println!("{}", "-".repeat(100));
            for job in &jobs {
                println!(
                    "{:<32} {:<16} {:<10} {:<22} {}",
                    job.name,
                    job.kind,
                    job.task_mode,
                    job.next_run,
                    cron_display(job.cron_expr.as_ref())
                );
            }
        }

        ScheduleCommand::Add {
            cron,
            prompt,
            name,
            kind,
        } => {
            let normalized = normalize_cron_expr(&cron);
            cron::Schedule::from_str(&normalized)
                .map_err(|e| anyhow::anyhow!("invalid cron expression '{cron}': {e}"))?;

            let sanitized = sanitize_task_prompt(&prompt);
            let job_name = name.unwrap_or_else(|| {
                let hash = blake3::hash(sanitized.as_bytes());
                format!("cli-{}", &hash.to_hex()[..8])
            });

            match store
                .insert_job(&job_name, &normalized, &kind, "periodic", None, &sanitized)
                .await
            {
                Ok(()) => {
                    println!("Added scheduled job '{job_name}' with cron '{normalized}'.");
                }
                Err(SchedulerError::DuplicateJob(n)) => {
                    anyhow::bail!(
                        "job '{n}' already exists. Remove it first with: zeph schedule remove {n}"
                    );
                }
                Err(e) => return Err(anyhow::anyhow!("failed to add job: {e}")),
            }
        }

        ScheduleCommand::Remove { name } => {
            let removed = store
                .delete_job(&name)
                .await
                .map_err(|e| anyhow::anyhow!("failed to remove job: {e}"))?;

            if removed {
                println!("Removed job '{name}'.");
            } else {
                anyhow::bail!("no scheduled job named '{name}'");
            }
        }

        ScheduleCommand::Show { name } => {
            let job = store
                .list_jobs_full()
                .await
                .map_err(|e| anyhow::anyhow!("failed to list jobs: {e}"))?
                .into_iter()
                .find(|j| j.name == name)
                .ok_or_else(|| anyhow::anyhow!("no scheduled job named '{name}'"))?;

            println!("Name:     {}", job.name);
            println!("Kind:     {}", job.kind);
            println!("Mode:     {}", job.task_mode);
            println!("Cron:     {}", cron_display(job.cron_expr.as_ref()));
            println!("Next run: {}", job.next_run);
            if !job.task_data.is_empty() {
                println!("Prompt:   {}", job.task_data);
            }
        }
    }

    Ok(())
}
