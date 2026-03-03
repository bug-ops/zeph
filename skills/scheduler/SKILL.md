---
name: scheduler
description: >
  Create, cancel, and manage periodic (cron) and one-shot (deferred) background tasks.
  Use when the user wants to schedule recurring work (daily summaries, cleanups)
  or run something at a specific time in the future.
triggers:
  - "every day at"
  - "schedule"
  - "remind me"
  - "at 3am"
  - "in 2 hours"
  - "recurring"
  - "cancel task"
---

# Scheduler

Use `schedule_periodic` for recurring cron tasks, `schedule_deferred` for one-shot
tasks at a specific future time, and `cancel_task` to remove a scheduled task.

## Create a daily memory cleanup (cron)

```schedule_periodic
{"name": "daily-cleanup", "cron": "0 0 3 * * *", "kind": "memory_cleanup"}
```

## Create a weekly skill refresh (cron)

```schedule_periodic
{"name": "weekly-skill-refresh", "cron": "0 0 2 * * SUN", "kind": "skill_refresh"}
```

## Run a custom task at a specific time (deferred)

```schedule_deferred
{"name": "follow-up", "run_at": "2026-03-03T18:00:00Z", "kind": "custom", "task": "check if PR was merged and notify"}
```

## Cancel a scheduled task

```cancel_task
{"name": "daily-cleanup"}
```

## Cron format

`sec min hour day month weekday` (6 fields, uses the `cron` crate).

Built-in kinds: `memory_cleanup`, `skill_refresh`, `health_check`, `update_check`, `custom`.

For `custom` kind, put a human-readable description in the `task` field —
the scheduler will inject it as a new agent turn at the scheduled time.

## Validation rules

- `run_at` must be ISO 8601 UTC and in the future.
- `cron` must be a valid 6-field cron expression.
- Task names must be unique. Scheduling with an existing name updates the task.
