---
name: scheduler
category: system
description: >
  Create, cancel, and manage periodic (cron) and one-shot (deferred) background tasks.
  Use when the user wants to schedule recurring work (daily summaries, cleanups),
  run something at a specific time, set reminders, or manage existing scheduled tasks.
  Keywords: schedule, cron, timer, remind, recurring, periodic, deferred, at, every, cancel.
metadata:
  author: zeph
  version: "1.0"
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

```schedule_deferred
{"name": "reminder-call-home", "run_at": "+2h", "kind": "custom", "task": "Remind the user to call home"}
```

```schedule_deferred
{"name": "standup-reminder", "run_at": "in 30 minutes", "kind": "custom", "task": "Remind the user: standup in 5 minutes"}
```

```schedule_deferred
{"name": "eod-summary", "run_at": "today 18:00", "kind": "custom", "task": "generate end-of-day summary"}
```

## Cancel a scheduled task

```cancel_task
{"name": "daily-cleanup"}
```

## Cron Expression Reference

Both standard 5-field (`min hour day month weekday`) and 6-field (`sec min hour day month weekday`)
expressions are accepted. When 5 fields are given, seconds default to 0.

### Field ranges

| Field | Position (6-field) | Allowed Values | Special Characters |
|-------|-------------------|----------------|-------------------|
| Seconds | 1 | 0-59 | `*` `,` `-` `/` |
| Minutes | 2 | 0-59 | `*` `,` `-` `/` |
| Hours | 3 | 0-23 | `*` `,` `-` `/` |
| Day of Month | 4 | 1-31 | `*` `,` `-` `/` |
| Month | 5 | 1-12 or JAN-DEC | `*` `,` `-` `/` |
| Day of Week | 6 | 0-7 or SUN-SAT | `*` `,` `-` `/` |

### Common cron examples

| Expression (6-field) | Meaning |
|---------------------|---------|
| `0 0 * * * *` | Every hour at :00 |
| `0 */15 * * * *` | Every 15 minutes |
| `0 0 3 * * *` | Daily at 03:00 |
| `0 30 9 * * MON-FRI` | Weekdays at 09:30 |
| `0 0 2 * * SUN` | Every Sunday at 02:00 |
| `0 0 0 1 * *` | First day of every month at midnight |
| `0 0 8,12,18 * * *` | At 08:00, 12:00, and 18:00 daily |

### Special characters

- `*` — all values in the field
- `,` — list separator (e.g. `MON,WED,FRI`)
- `-` — range (e.g. `9-17` for hours 9 through 17)
- `/` — step (e.g. `*/15` for every 15 units, `0/30` for 0 and 30)

## Built-in Kinds

`memory_cleanup`, `skill_refresh`, `health_check`, `update_check`, `custom`.

For `custom` kind, the `task` field controls what happens at execution time.
At the scheduled moment it is injected as `Execute the following scheduled task now: <task>` into the agent turn.

**Two patterns for `task`:**

1. **Reminder for the user** — write what the user should be notified about.
   The agent will relay the message to the user without acting on it.
   ```
   "task": "Remind the user to call home"
   "task": "Remind the user: standup in 5 minutes"
   ```

2. **Action for the agent** — write an instruction for the agent to execute.
   The agent will perform the action autonomously at the scheduled time.
   ```
   "task": "Check if PR #42 was merged and notify the user"
   "task": "Generate an end-of-day summary and send it"
   "task": "Run memory cleanup and report results"
   ```

**Rule:** if the user says "remind me to X", use pattern 1 (`Remind the user to X`).
If the user says "do X at time", use pattern 2 (`X`).

## run_at Formats

`run_at` accepts any of the following (must resolve to a future time):

| Format | Example |
|--------|---------|
| ISO 8601 UTC | `2026-03-03T18:00:00Z` |
| ISO 8601 naive (treated as UTC) | `2026-03-03T18:00:00` |
| Relative shorthand | `+2m`, `+1h`, `+30s`, `+1d`, `+1h30m` |
| Natural language | `in 5 minutes`, `in 2 hours`, `today 14:00`, `tomorrow 09:30` |

## Validation Rules

- `run_at` must resolve to a future time.
- `cron` must be a valid 5 or 6-field cron expression.
- Task names must be unique. Scheduling with an existing name updates the task.
