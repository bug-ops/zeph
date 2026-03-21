---
name: cron
description: >
  Schedule recurring and one-time tasks using cron, crontab, at, and launchd.
  Use when the user asks to set up scheduled jobs, configure crontab entries,
  write cron expressions, schedule one-time tasks with at, create launchd
  plist files on macOS, debug why a cron job is not running, or manage
  periodic automated tasks on Unix/Linux/macOS systems.
license: MIT
compatibility: Requires cron/crontab (Linux/macOS) or launchd (macOS)
metadata:
  author: zeph
  version: "1.0"
---

# Task Scheduling with Cron, At, and Launchd

## Cron Expression Format

```
┌─────────── minute (0-59)
│ ┌───────── hour (0-23)
│ │ ┌─────── day of month (1-31)
│ │ │ ┌───── month (1-12 or JAN-DEC)
│ │ │ │ ┌─── day of week (0-6, SUN-SAT; 7 = SUN on some systems)
│ │ │ │ │
* * * * * command
```

### Special Characters

| Character | Meaning | Example |
|-----------|---------|---------|
| `*` | Any value | `* * * * *` (every minute) |
| `,` | List | `1,15,30` (at 1, 15, 30) |
| `-` | Range | `1-5` (1 through 5) |
| `/` | Step | `*/5` (every 5 units) |

### Common Expressions

| Expression | Schedule |
|------------|----------|
| `* * * * *` | Every minute |
| `*/5 * * * *` | Every 5 minutes |
| `*/15 * * * *` | Every 15 minutes |
| `0 * * * *` | Every hour (at :00) |
| `0 */2 * * *` | Every 2 hours |
| `30 * * * *` | Every hour at :30 |
| `0 0 * * *` | Daily at midnight |
| `0 6 * * *` | Daily at 6:00 AM |
| `0 8,12,18 * * *` | At 8 AM, noon, 6 PM |
| `0 9-17 * * *` | Every hour from 9 AM to 5 PM |
| `0 0 * * 0` | Weekly on Sunday at midnight |
| `0 0 * * 1-5` | Weekdays at midnight |
| `0 0 * * 6,0` | Weekends at midnight |
| `0 0 1 * *` | First day of every month |
| `0 0 1 1 *` | Yearly on January 1 |
| `0 0 1 */3 *` | Every 3 months (quarterly) |
| `0 0 15 * *` | 15th of every month |
| `5 4 * * 0` | Sunday at 4:05 AM |
| `0 22 * * 1-5` | Weekdays at 10 PM |

### Predefined Shortcuts

| Shortcut | Equivalent | Description |
|----------|-----------|-------------|
| `@reboot` | — | Run once at startup |
| `@yearly` / `@annually` | `0 0 1 1 *` | Once a year (Jan 1) |
| `@monthly` | `0 0 1 * *` | Once a month (1st) |
| `@weekly` | `0 0 * * 0` | Once a week (Sunday) |
| `@daily` / `@midnight` | `0 0 * * *` | Once a day |
| `@hourly` | `0 * * * *` | Once an hour |

## Crontab Management

```bash
# Edit current user's crontab
crontab -e

# List current user's crontab
crontab -l

# Remove current user's crontab (careful!)
crontab -r

# Edit crontab for another user (requires root)
sudo crontab -u username -e

# List another user's crontab
sudo crontab -u username -l

# Create crontab from file
crontab mycrontab.txt

# Backup crontab
crontab -l > crontab-backup.txt

# Restore crontab from backup
crontab crontab-backup.txt
```

## Crontab File Format

```bash
# Environment variables (set before jobs)
SHELL=/bin/bash
PATH=/usr/local/bin:/usr/bin:/bin
MAILTO=admin@example.com
HOME=/home/user

# Jobs
# min hour dom month dow command

# Daily backup at 2:30 AM
30 2 * * * /usr/local/bin/backup.sh

# Every 15 minutes: check service health
*/15 * * * * /usr/local/bin/healthcheck.sh

# Weekdays at 9 AM: send report
0 9 * * 1-5 /usr/local/bin/send-report.sh

# Monthly cleanup on the 1st at 3 AM
0 3 1 * * /usr/local/bin/cleanup.sh

# At reboot: start services
@reboot /usr/local/bin/start-services.sh
```

## Output Handling

```bash
# Redirect stdout to file
* * * * * /path/to/script.sh > /var/log/cron-output.log 2>&1

# Append to file
* * * * * /path/to/script.sh >> /var/log/cron-output.log 2>&1

# Discard all output (silent)
* * * * * /path/to/script.sh > /dev/null 2>&1

# Redirect stdout and stderr separately
* * * * * /path/to/script.sh > /var/log/cron.log 2> /var/log/cron-error.log

# Email output (if MAILTO is set)
MAILTO=user@example.com
0 * * * * /path/to/script.sh

# Disable email
MAILTO=""
0 * * * * /path/to/script.sh

# Email only on error
0 * * * * /path/to/script.sh > /dev/null
```

## Environment in Cron

Cron jobs run with a minimal environment. The PATH is usually just `/usr/bin:/bin`.

```bash
# Set PATH in crontab
PATH=/usr/local/bin:/usr/bin:/bin:/home/user/.local/bin

# Or use full paths in commands
0 * * * * /usr/local/bin/python3 /home/user/scripts/task.py

# Or source profile in command
0 * * * * . /home/user/.profile && /home/user/scripts/task.sh

# Or wrap in bash with login shell
0 * * * * /bin/bash -lc '/home/user/scripts/task.sh'

# Set environment variables inline
0 * * * * LANG=en_US.UTF-8 /home/user/scripts/task.sh

# Set in crontab file
LANG=en_US.UTF-8
NODE_ENV=production
```

## Locking (Preventing Overlap)

```bash
# Using flock (recommended)
*/5 * * * * flock -n /tmp/myjob.lock /path/to/script.sh

# flock with timeout (wait up to 60 seconds for lock)
*/5 * * * * flock -w 60 /tmp/myjob.lock /path/to/script.sh

# Using PID file (manual)
*/5 * * * * /path/to/script-with-pidcheck.sh
# In script:
# PIDFILE=/tmp/myjob.pid
# if [ -f "$PIDFILE" ] && kill -0 $(cat "$PIDFILE") 2>/dev/null; then exit 0; fi
# echo $$ > "$PIDFILE"
# trap "rm -f $PIDFILE" EXIT
```

## System Cron Directories

```bash
# System-wide crontab
/etc/crontab

# Drop-in directories (scripts placed here run automatically)
/etc/cron.d/           # custom cron files (crontab format with user field)
/etc/cron.hourly/      # scripts run hourly
/etc/cron.daily/       # scripts run daily
/etc/cron.weekly/      # scripts run weekly
/etc/cron.monthly/     # scripts run monthly

# Format in /etc/crontab and /etc/cron.d/* includes a user field:
# min hour dom month dow user command
*/5 * * * * root /usr/local/bin/check.sh
```

## at — One-Time Scheduled Tasks

```bash
# Schedule a command for a specific time
echo "/path/to/script.sh" | at 14:30

# Schedule for a specific date and time
echo "/path/to/script.sh" | at 14:30 2024-12-25

# Schedule relative to now
echo "/path/to/script.sh" | at now + 5 minutes
echo "/path/to/script.sh" | at now + 2 hours
echo "/path/to/script.sh" | at now + 1 day
echo "/path/to/script.sh" | at now + 1 week

# Interactive mode (type commands, press Ctrl+D to finish)
at 15:00
# > /path/to/script.sh
# > echo "Done" | mail user@example.com
# > <Ctrl+D>

# Other time expressions
echo "cmd" | at midnight
echo "cmd" | at noon
echo "cmd" | at teatime        # 4 PM
echo "cmd" | at tomorrow
echo "cmd" | at "10:00 AM Jul 4"

# List pending jobs
atq
at -l

# View job contents
at -c JOB_NUMBER

# Remove a job
atrm JOB_NUMBER
at -d JOB_NUMBER

# batch — run when system load is low
echo "/path/to/heavy-task.sh" | batch
```

## Launchd (macOS)

Launchd is the macOS replacement for cron. Jobs are configured via XML property list (plist) files.

### Plist File Template

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.example.myjob</string>

    <key>ProgramArguments</key>
    <array>
        <string>/usr/local/bin/script.sh</string>
    </array>

    <!-- Run every 300 seconds (5 minutes) -->
    <key>StartInterval</key>
    <integer>300</integer>

    <!-- OR: calendar-based schedule (like cron) -->
    <!--
    <key>StartCalendarInterval</key>
    <dict>
        <key>Hour</key>
        <integer>6</integer>
        <key>Minute</key>
        <integer>30</integer>
    </dict>
    -->

    <!-- Standard output and error logs -->
    <key>StandardOutPath</key>
    <string>/tmp/myjob.stdout.log</string>
    <key>StandardErrorPath</key>
    <string>/tmp/myjob.stderr.log</string>

    <!-- Run at load (startup) -->
    <key>RunAtLoad</key>
    <true/>

    <!-- Environment variables -->
    <key>EnvironmentVariables</key>
    <dict>
        <key>PATH</key>
        <string>/usr/local/bin:/usr/bin:/bin</string>
    </dict>
</dict>
</plist>
```

### Calendar Schedule Keys

```xml
<!-- Daily at 2:30 AM -->
<key>StartCalendarInterval</key>
<dict>
    <key>Hour</key>
    <integer>2</integer>
    <key>Minute</key>
    <integer>30</integer>
</dict>

<!-- Weekdays at 9 AM (Weekday: 1=Mon, 7=Sun) -->
<key>StartCalendarInterval</key>
<array>
    <dict>
        <key>Weekday</key><integer>1</integer>
        <key>Hour</key><integer>9</integer>
        <key>Minute</key><integer>0</integer>
    </dict>
    <dict>
        <key>Weekday</key><integer>2</integer>
        <key>Hour</key><integer>9</integer>
        <key>Minute</key><integer>0</integer>
    </dict>
    <dict>
        <key>Weekday</key><integer>3</integer>
        <key>Hour</key><integer>9</integer>
        <key>Minute</key><integer>0</integer>
    </dict>
    <dict>
        <key>Weekday</key><integer>4</integer>
        <key>Hour</key><integer>9</integer>
        <key>Minute</key><integer>0</integer>
    </dict>
    <dict>
        <key>Weekday</key><integer>5</integer>
        <key>Hour</key><integer>9</integer>
        <key>Minute</key><integer>0</integer>
    </dict>
</array>

<!-- Monthly on the 1st -->
<key>StartCalendarInterval</key>
<dict>
    <key>Day</key>
    <integer>1</integer>
    <key>Hour</key>
    <integer>0</integer>
    <key>Minute</key>
    <integer>0</integer>
</dict>
```

### Launchd Management

```bash
# Load (start) a user agent
launchctl load ~/Library/LaunchAgents/com.example.myjob.plist

# Unload (stop) a user agent
launchctl unload ~/Library/LaunchAgents/com.example.myjob.plist

# Load a system daemon (requires sudo)
sudo launchctl load /Library/LaunchDaemons/com.example.myjob.plist

# Check if loaded
launchctl list | grep com.example.myjob

# Modern syntax (macOS 10.10+)
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.example.myjob.plist
launchctl bootout gui/$(id -u)/com.example.myjob

# Force start (run now regardless of schedule)
launchctl start com.example.myjob

# Validate plist syntax
plutil -lint ~/Library/LaunchAgents/com.example.myjob.plist
```

### Launchd Directories

| Directory | Scope | Runs as |
|-----------|-------|---------|
| `~/Library/LaunchAgents/` | Current user only | Current user |
| `/Library/LaunchAgents/` | All users | Logged-in user |
| `/Library/LaunchDaemons/` | System-wide | root |

## Debugging Cron Jobs

### Common Issues

1. **Wrong PATH**: cron uses a minimal PATH; use full paths or set PATH in crontab
2. **No output captured**: redirect both stdout and stderr (`> log 2>&1`)
3. **Permission denied**: check script has execute permission (`chmod +x script.sh`)
4. **Wrong user**: verify which user's crontab the job is in
5. **Environment missing**: cron does not source `.bashrc` or `.profile`

### Diagnostic Steps

```bash
# Check cron daemon is running
# Linux:
systemctl status cron
systemctl status crond

# macOS:
launchctl list | grep cron

# Check syslog for cron execution
# Linux:
grep CRON /var/log/syslog
journalctl -u cron
journalctl -u cron --since "1 hour ago"

# macOS:
log show --predicate 'process == "cron"' --last 1h

# Verify crontab is saved
crontab -l

# Test command manually with cron's environment
env -i SHELL=/bin/bash PATH=/usr/bin:/bin HOME=$HOME /path/to/script.sh

# Check if cron email is queued
ls /var/mail/
```

### Test Script for Cron Environment

```bash
#!/bin/bash
# Save as: /tmp/cron-debug.sh
# Add to crontab: * * * * * /tmp/cron-debug.sh > /tmp/cron-debug.log 2>&1
echo "Date: $(date)"
echo "User: $(whoami)"
echo "Shell: $SHELL"
echo "PATH: $PATH"
echo "HOME: $HOME"
echo "PWD: $(pwd)"
env | sort
```

## Important Notes

- Cron uses the system timezone; be explicit about timezone if it matters
- The `%` character in crontab has special meaning (newline); escape it as `\%`
- Both day-of-month and day-of-week fields are OR-ed: `0 0 1 * 5` runs on the 1st AND every Friday
- Crontab files must end with a newline; the last line may be silently ignored without one
- On macOS, launchd is preferred over cron; some macOS versions restrict cron
- `at` may not be installed by default on minimal Linux installations; install via `at` package
- Use `flock` to prevent overlapping job execution for long-running tasks
- Always test cron commands by running them manually first with a reduced environment
- Log rotation: if cron jobs produce logs, set up logrotate to prevent disk filling
- For complex scheduling (business days, holidays), consider dedicated schedulers over cron
