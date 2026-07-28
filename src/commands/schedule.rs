// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

#[cfg(feature = "scheduler")]
use crate::cli::ScheduleCommand;

#[cfg(feature = "scheduler")]
fn cron_display(expr: Option<&zeph_scheduler::CronExpr>) -> &str {
    expr.map_or("-", zeph_scheduler::CronExpr::as_str)
}

/// Handles `zeph schedule add`, both the existing periodic-cron path and the `--run-at`
/// one-shot path (#6361). Split out of `handle_schedule_command` to stay under the
/// `too_many_lines` clippy threshold.
#[cfg(feature = "scheduler")]
async fn handle_schedule_add(
    store: &zeph_scheduler::JobStore,
    cron: Option<String>,
    prompt: String,
    name: Option<String>,
    kind: String,
    run_at: Option<String>,
) -> anyhow::Result<()> {
    use std::str::FromStr as _;

    use zeph_scheduler::{SchedulerError, normalize_cron_expr, sanitize_task_prompt};

    let sanitized = sanitize_task_prompt(&prompt);
    let job_name = name.unwrap_or_else(|| {
        let hash = blake3::hash(sanitized.as_bytes());
        format!("cli-{}", &hash.to_hex()[..8])
    });

    if let Some(run_at) = run_at {
        // FR-007: reject a past run_at. `chrono::Utc::now()` is used directly here — spec 070
        // §8's mockable-clock MUST scopes only the tool executor and the reminder-injection
        // path (crates/zeph-tools, crates/zeph-core), not this one-off CLI arg validation in
        // the binary crate.
        let parsed = chrono::DateTime::parse_from_rfc3339(&run_at)
            .map_err(|e| {
                anyhow::anyhow!(
                    "invalid --run-at timestamp '{run_at}': {e}. \
                     Expected RFC3339 with an explicit UTC offset, e.g. 2026-07-19T14:30:00Z"
                )
            })?
            .with_timezone(&chrono::Utc);
        if parsed <= chrono::Utc::now() {
            anyhow::bail!(
                "--run-at '{run_at}' is in the past; one-shot jobs must be scheduled for a \
                 future time"
            );
        }

        return match store
            .insert_job(
                &job_name,
                "",
                &kind,
                "oneshot",
                Some(&parsed.to_rfc3339()),
                &sanitized,
            )
            .await
        {
            Ok(()) => {
                println!(
                    "Added one-shot job '{job_name}' to run at '{}'. Fires on the next agent \
                     start if the agent is not already running (Zeph's scheduler runs \
                     in-process, not as an always-on daemon).",
                    parsed.to_rfc3339()
                );
                Ok(())
            }
            Err(SchedulerError::DuplicateJob(n)) => Err(anyhow::anyhow!(
                "job '{n}' already exists. Remove it first with: zeph schedule remove {n}"
            )),
            Err(e) => Err(anyhow::anyhow!("failed to add job: {e}")),
        };
    }

    let Some(cron) = cron else {
        unreachable!("clap enforces cron is present when run_at is absent");
    };
    let normalized = normalize_cron_expr(&cron);
    cron::Schedule::from_str(&normalized)
        .map_err(|e| anyhow::anyhow!("invalid cron expression '{cron}': {e}"))?;

    match store
        .insert_job(&job_name, &normalized, &kind, "periodic", None, &sanitized)
        .await
    {
        Ok(()) => {
            println!("Added scheduled job '{job_name}' with cron '{normalized}'.");
            Ok(())
        }
        Err(SchedulerError::DuplicateJob(n)) => Err(anyhow::anyhow!(
            "job '{n}' already exists. Remove it first with: zeph schedule remove {n}"
        )),
        Err(e) => Err(anyhow::anyhow!("failed to add job: {e}")),
    }
}

#[cfg(feature = "scheduler")]
pub(crate) async fn handle_schedule_command(
    cmd: ScheduleCommand,
    config_path: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    use crate::bootstrap::{load_config_or_default, resolve_config_path};
    use zeph_scheduler::JobStore;

    let config_file = resolve_config_path(config_path);
    let config = load_config_or_default(&config_file)?;
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
            run_at,
        } => {
            handle_schedule_add(&store, cron, prompt, name, kind, run_at).await?;
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

#[cfg(all(test, feature = "scheduler"))]
mod tests {
    use super::handle_schedule_add;

    async fn test_pool() -> zeph_db::DbPool {
        zeph_db::DbConfig {
            url: ":memory:".to_owned(),
            ..Default::default()
        }
        .connect()
        .await
        .unwrap()
    }

    async fn test_store() -> zeph_scheduler::JobStore {
        let pool = test_pool().await;
        let store = zeph_scheduler::JobStore::new(pool);
        store.init().await.unwrap();
        store
    }

    #[tokio::test]
    async fn handle_schedule_add_rejects_past_run_at() {
        let store = test_store().await;
        let result = handle_schedule_add(
            &store,
            None,
            "should be rejected".to_owned(),
            None,
            "custom".to_owned(),
            Some("2020-01-01T00:00:00Z".to_owned()),
        )
        .await;

        let err = result.expect_err("a past --run-at must be rejected (FR-007)");
        assert!(
            err.to_string().contains("is in the past"),
            "error must mention the past-timestamp rejection, got: {err}"
        );
        let jobs = store.list_jobs_full().await.unwrap();
        assert!(
            jobs.is_empty(),
            "no row must be written for a rejected past run_at"
        );
    }

    #[tokio::test]
    async fn handle_schedule_add_rejects_invalid_rfc3339() {
        let store = test_store().await;
        let result = handle_schedule_add(
            &store,
            None,
            "should be rejected".to_owned(),
            None,
            "custom".to_owned(),
            Some("not-a-timestamp".to_owned()),
        )
        .await;

        let err = result.expect_err("an invalid RFC3339 timestamp must be rejected");
        assert!(
            err.to_string().contains("invalid --run-at timestamp"),
            "error must mention the invalid timestamp, got: {err}"
        );
    }

    #[tokio::test]
    async fn handle_schedule_add_rejects_bare_offsetless_timestamp() {
        let store = test_store().await;
        let result = handle_schedule_add(
            &store,
            None,
            "should be rejected".to_owned(),
            None,
            "custom".to_owned(),
            // No trailing Z / explicit offset.
            Some("2026-07-19T14:30:00".to_owned()),
        )
        .await;

        assert!(
            result.is_err(),
            "a bare local-time string without an explicit UTC offset must be rejected"
        );
    }

    #[tokio::test]
    async fn handle_schedule_add_run_at_accepts_future_timestamp_and_stores_prompt() {
        let store = test_store().await;
        let future = (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
        handle_schedule_add(
            &store,
            None,
            "check the deploy status".to_owned(),
            Some("my-oneshot-job".to_owned()),
            "custom".to_owned(),
            Some(future),
        )
        .await
        .expect("a future --run-at must be accepted");

        let jobs = store.list_jobs_full().await.unwrap();
        let job = jobs
            .iter()
            .find(|j| j.name == "my-oneshot-job")
            .expect("job must be persisted under the given --name");
        assert_eq!(job.task_mode, "oneshot");
        assert_eq!(job.task_data, "check the deploy status");
    }

    #[tokio::test]
    async fn handle_schedule_add_run_at_duplicate_name_errors() {
        let store = test_store().await;
        let future = (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
        handle_schedule_add(
            &store,
            None,
            "first".to_owned(),
            Some("dup-job".to_owned()),
            "custom".to_owned(),
            Some(future.clone()),
        )
        .await
        .expect("first insert must succeed");

        let result = handle_schedule_add(
            &store,
            None,
            "second".to_owned(),
            Some("dup-job".to_owned()),
            "custom".to_owned(),
            Some(future),
        )
        .await;

        let err = result.expect_err("a duplicate job name must be rejected");
        assert!(
            err.to_string().contains("already exists"),
            "error must mention the duplicate job, got: {err}"
        );
    }

    #[tokio::test]
    async fn handle_schedule_add_cron_path_still_works() {
        let store = test_store().await;
        handle_schedule_add(
            &store,
            Some("0 * * * *".to_owned()),
            "hourly check".to_owned(),
            Some("cron-job".to_owned()),
            "custom".to_owned(),
            None,
        )
        .await
        .expect("a valid cron expression must be accepted");

        let jobs = store.list_jobs_full().await.unwrap();
        let job = jobs
            .iter()
            .find(|j| j.name == "cron-job")
            .expect("job must be persisted");
        assert_eq!(job.task_mode, "periodic");
    }
}
