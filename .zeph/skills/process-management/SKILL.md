---
name: process-management
category: system
description: >
  Monitor, manage, and control system processes. Use when the user asks to list
  running processes, find a process by name or PID, kill or terminate processes,
  manage background jobs, check resource usage, set resource limits, manage
  system services with systemctl or launchctl, or troubleshoot CPU and RAM usage
  with ps, top, htop, kill, pgrep, and related tools.
license: MIT
metadata:
  author: zeph
  version: "1.0"
---

# Process Management

## Quick Reference

| Task | Command |
|------|---------|
| List all processes | `ps aux` |
| Find process by name | `pgrep -a name` |
| Find process on port | `lsof -i :8080` |
| Kill by PID | `kill PID` |
| Kill by name | `pkill name` |
| Force kill | `kill -9 PID` |
| Process tree | `pstree` |
| Interactive monitor | `top` or `htop` |
| Background a command | `command &` |
| List background jobs | `jobs` |

## Viewing Processes

### ps — Process Status

```bash
# All processes (BSD-style, most common)
ps aux

# All processes (POSIX-style)
ps -ef

# Process tree (Linux)
ps -ef --forest

# Specific user's processes
ps -u username

# Specific process by PID
ps -p 1234

# Specific process by name
ps aux | grep nginx

# Custom output columns
ps -eo pid,ppid,user,%cpu,%mem,vsz,rss,stat,start,time,comm

# Sort by CPU usage
ps aux --sort=-%cpu | head -20

# Sort by memory usage
ps aux --sort=-%mem | head -20

# Show threads
ps -eLf                  # Linux
ps -M PID                # macOS

# Show full command line
ps auxww

# Count processes
ps aux | wc -l
```

### Process Status Codes (STAT column)

| Code | Meaning |
|------|---------|
| `R` | Running or runnable |
| `S` | Sleeping (interruptible) |
| `D` | Uninterruptible sleep (I/O wait) |
| `T` | Stopped (by signal or debugger) |
| `Z` | Zombie (terminated but not reaped) |
| `I` | Idle kernel thread |
| `<` | High priority |
| `N` | Low priority (nice) |
| `s` | Session leader |
| `l` | Multi-threaded |
| `+` | Foreground process group |

### top — Interactive Process Monitor

```bash
# Start top
top

# Sort by CPU (default)
# Press: P (CPU), M (memory), T (time), N (PID)

# Show specific user
top -u username

# Non-interactive mode (batch, for scripting)
top -b -n 1

# Refresh interval (seconds)
top -d 2

# Show specific number of processes
top -n 1 -b | head -20

# macOS top: sort by CPU
top -o cpu

# macOS top: sort by memory
top -o mem
```

### htop — Enhanced Interactive Monitor

```bash
htop                             # start htop
htop -u username                 # filter by user
htop -t                          # tree view
htop --sort-key=PERCENT_CPU      # sort by CPU
```

Key bindings: `F5`=tree, `F6`=sort, `F9`=kill, `F4`=filter, `/`=search, `u`=user filter.

## Finding Processes

### pgrep — Find Process by Name

```bash
# Find PID by name
pgrep nginx

# Show full command line
pgrep -a nginx

# Show PID and name
pgrep -l nginx

# Match exact name (not substring)
pgrep -x nginx

# Find by user
pgrep -u www-data

# Find newest matching process
pgrep -n nginx

# Find oldest matching process
pgrep -o nginx

# Count matching processes
pgrep -c nginx

# Match parent PID
pgrep -P 1234
```

### Finding Processes by Resource Usage

```bash
# Process using most CPU
ps aux --sort=-%cpu | head -5

# Process using most memory
ps aux --sort=-%mem | head -5

# Process using specific port
lsof -i :8080
lsof -i :8080 -sTCP:LISTEN

# Process using specific file
lsof /path/to/file

# All files opened by process
lsof -p PID

# Process with specific working directory
lsof +D /path/to/dir
```

### pstree — Process Tree

```bash
# Full process tree
pstree

# Process tree with PIDs
pstree -p

# Tree for specific PID
pstree -p 1234

# Tree for specific user
pstree username

# Show command-line arguments
pstree -a

# Compact view (merge identical branches)
pstree -c
```

## Killing Processes

### Signals

| Signal | Number | Action | Use Case |
|--------|--------|--------|----------|
| `SIGHUP` | 1 | Hangup | Reload configuration |
| `SIGINT` | 2 | Interrupt | Same as Ctrl+C |
| `SIGQUIT` | 3 | Quit | Quit with core dump |
| `SIGKILL` | 9 | Kill | Force kill (cannot be caught) |
| `SIGTERM` | 15 | Terminate | Graceful shutdown (default) |
| `SIGSTOP` | 19 | Stop | Pause process (cannot be caught) |
| `SIGCONT` | 18 | Continue | Resume stopped process |
| `SIGUSR1` | 10 | User-defined | Application-specific |
| `SIGUSR2` | 12 | User-defined | Application-specific |

### kill — Send Signal to Process

```bash
# Graceful termination (SIGTERM, default)
kill PID
kill -15 PID
kill -TERM PID

# Force kill (SIGKILL, last resort)
kill -9 PID
kill -KILL PID

# Send HUP (reload config)
kill -HUP PID
kill -1 PID

# Stop (pause) process
kill -STOP PID

# Resume stopped process
kill -CONT PID

# Kill multiple processes
kill PID1 PID2 PID3

# Kill process group (negative PID)
kill -- -PGID
```

### pkill — Kill by Name

```bash
# Kill by name (SIGTERM)
pkill nginx

# Force kill by name
pkill -9 nginx

# Kill by exact name
pkill -x nginx

# Kill processes of specific user
pkill -u username

# Kill by pattern
pkill -f "python.*server.py"

# Send signal by name
pkill -HUP nginx
```

### killall — Kill All by Name

```bash
killall nginx              # kill all with name
killall -9 nginx           # force kill
killall -u username        # kill by user
killall -w nginx           # wait for processes to die
```

### Graceful Shutdown Pattern

```bash
kill PID && sleep 5 && kill -0 PID 2>/dev/null && kill -9 PID
```

## Job Control

### Background and Foreground

```bash
# Run command in background
command &

# Suspend current foreground process
# Press Ctrl+Z

# List jobs
jobs
jobs -l     # with PIDs

# Resume in foreground
fg           # most recent job
fg %1        # job number 1

# Resume in background
bg           # most recent job
bg %2        # job number 2

# Run command immune to hangup (persists after logout)
nohup command &
nohup command > output.log 2>&1 &

# Disown a running job (detach from shell)
command &
disown %1

# Disown and suppress HUP
disown -h %1

# Wait for background jobs
wait          # wait for all
wait PID      # wait for specific
wait %1       # wait for job 1
```

### Job Identifiers

| Identifier | Meaning |
|-----------|---------|
| `%1` | Job number 1 |
| `%+` or `%%` | Current (most recent) job |
| `%-` | Previous job |
| `%string` | Job whose command starts with string |
| `%?string` | Job whose command contains string |

## Resource Limits

### ulimit — User Limits

```bash
# Show all limits
ulimit -a

# Max open files (soft limit)
ulimit -n
ulimit -n 65536       # set

# Max user processes
ulimit -u
ulimit -u 4096        # set

# Max stack size (KB)
ulimit -s

# Max virtual memory (KB)
ulimit -v

# Max file size (blocks)
ulimit -f

# Core dump size (0 = disabled)
ulimit -c
ulimit -c unlimited   # enable core dumps

# Hard limits (cannot increase above)
ulimit -Hn            # hard limit for open files
```

### nice and renice — Process Priority

```bash
# Run with lower priority (higher nice = lower priority)
nice -n 10 command

# Run with higher priority (requires root)
sudo nice -n -10 command

# Change priority of running process
renice 10 -p PID

# Change priority for all processes of user
renice 5 -u username

# Nice values: -20 (highest priority) to 19 (lowest)
```

## Service Management

### systemctl (Linux, systemd)

```bash
# Start/stop/restart service
sudo systemctl start nginx
sudo systemctl stop nginx
sudo systemctl restart nginx

# Reload configuration (without restart)
sudo systemctl reload nginx

# Check status
systemctl status nginx

# Enable/disable on boot
sudo systemctl enable nginx
sudo systemctl disable nginx

# List all services
systemctl list-units --type=service

# List active services
systemctl list-units --type=service --state=running

# List failed services
systemctl list-units --type=service --state=failed

# View service logs
journalctl -u nginx
journalctl -u nginx -f         # follow
journalctl -u nginx --since "1 hour ago"
journalctl -u nginx -n 50      # last 50 lines

# Check if service is active
systemctl is-active nginx

# Check if enabled on boot
systemctl is-enabled nginx

# Mask service (prevent starting)
sudo systemctl mask nginx
sudo systemctl unmask nginx
```

### launchctl (macOS)

```bash
# List all services
launchctl list

# List with filter
launchctl list | grep nginx

# Start/stop service
sudo launchctl load /Library/LaunchDaemons/com.example.service.plist
sudo launchctl unload /Library/LaunchDaemons/com.example.service.plist

# User agent (no sudo)
launchctl load ~/Library/LaunchAgents/com.example.agent.plist
launchctl unload ~/Library/LaunchAgents/com.example.agent.plist

# New syntax (macOS 10.10+)
launchctl bootstrap system /Library/LaunchDaemons/com.example.plist
launchctl bootout system /Library/LaunchDaemons/com.example.plist

# Print service info
launchctl print system/com.example.service

# Kick (force restart)
sudo launchctl kickstart system/com.example.service
sudo launchctl kickstart -k system/com.example.service  # kill first
```

## Common Workflows

### Find and Kill Process on Port

```bash
lsof -i :8080                    # find process on port
kill $(lsof -t -i :8080)        # kill it
kill -9 $(lsof -t -i :8080)     # force kill
```

### Find Zombie Processes

```bash
ps aux | awk '$8 ~ /Z/'         # find zombies
ps -o ppid= -p ZOMBIE_PID       # find parent of zombie (kill parent to reap)
```

## Important Notes

- Always try `SIGTERM` before `SIGKILL` (-9); SIGKILL does not allow cleanup
- `kill -0 PID` checks if a process exists without sending a signal
- Zombie processes cannot be killed; kill their parent to reap them
- On macOS, `ps aux` works but some Linux-specific flags (like `--forest`) are unavailable
- `ulimit` changes apply only to the current shell session and its children
