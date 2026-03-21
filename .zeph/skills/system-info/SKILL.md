---
name: system-info
description: >
  System diagnostics and resource monitoring — retrieve OS version and kernel
  info, check CPU load and core count, inspect memory and swap usage, analyze
  disk space and filesystem mounts, list running processes sorted by resource
  consumption, show network interfaces and open ports, display system uptime
  and boot time, review hardware details and installed packages. Use when the
  user asks about system health, performance bottlenecks, resource utilization,
  disk pressure, process monitoring, or environment diagnostics on Linux,
  macOS, or Windows.
metadata:
  author: zeph
  version: "1.0"
---
# System Info

Collect host diagnostics and resource metrics.

Before running commands, detect the OS and load the matching reference for
platform-specific syntax:

- **Linux** — `references/linux.md` (procfs, sysfs, GNU coreutils)
- **macOS** — `references/macos.md` (sysctl, vm_stat, diskutil, system_profiler)
- **Windows** — `references/windows.md` (PowerShell: Get-CimInstance, Get-Counter, Get-Process)

OS detection:
```bash
uname -s 2>/dev/null || echo Windows
```

## Workflow

1. Gather only the metrics the user requested — do not dump everything.
2. Prefer read-only, non-invasive commands. Never change system settings unless explicitly asked.
3. If a command is unavailable, check the OS reference for a fallback.
4. Return a short summary with key numbers (percentages, GB, top processes).
5. Flag values that indicate problems (see thresholds below).

## Diagnostic categories

### OS and kernel
Identify the operating system, distribution, kernel version, hostname, and architecture. Useful as a starting point for any troubleshooting session.

Key information to collect:
- OS name and version (e.g., Ubuntu 24.04, macOS 15.2, Windows 11 23H2)
- Kernel version
- System architecture (x86_64, arm64)
- Hostname

### Uptime and load average
Determine how long the system has been running and its current load.

Interpreting load average (Unix):
- Three numbers represent 1-minute, 5-minute, and 15-minute averages
- Compare against CPU core count: load equal to core count means full utilization
- Load consistently above core count indicates CPU contention
- Rising trend (1min > 5min > 15min) means load is increasing

### CPU
Inspect processor model, core count, current utilization, and per-core activity.

| Metric | Healthy | Warning | Critical |
|--------|---------|---------|----------|
| Overall utilization | < 70% | 70-90% | > 90% sustained |
| Load avg / core count | < 1.0 | 1.0-2.0 | > 2.0 |
| I/O wait | < 10% | 10-30% | > 30% |

### Memory
Check total, used, free, available, cached, and buffered memory plus swap usage.

| Metric | Healthy | Warning | Critical |
|--------|---------|---------|----------|
| Available memory | > 20% of total | 10-20% | < 10% |
| Swap usage | < 10% of total swap | 10-50% | > 50% |

Interpreting memory on Linux:
- "available" (not "free") is the true indicator of remaining capacity
- High "buff/cache" is normal and healthy — the kernel uses spare RAM for caching
- Swap usage above zero is not inherently bad, but heavy swap I/O indicates memory pressure

Interpreting memory on macOS:
- `vm_stat` reports in pages (typically 16384 bytes on Apple Silicon, 4096 on Intel)
- `memory_pressure` gives a simple "normal / warn / critical" assessment
- "Pages purgeable" and "Pages cached" are reclaimable — not a concern

### Disk and filesystem
Review disk space, mount points, inode usage, and I/O throughput.

| Metric | Healthy | Warning | Critical |
|--------|---------|---------|----------|
| Disk usage | < 80% | 80-90% | > 90% |
| Inode usage | < 80% | 80-90% | > 90% |

Common culprits for disk pressure:
- Log files (`/var/log/`, application logs)
- Build artifacts (`target/`, `node_modules/`, `__pycache__/`)
- Docker images and volumes
- Package manager caches

### Processes
List running processes, identify top resource consumers, find specific processes by name or PID.

Useful views:
- Top CPU consumers (top 10 by CPU %)
- Top memory consumers (top 10 by RSS)
- Process tree (parent-child relationships)
- Zombie or defunct processes
- Processes by a specific user

### Network
Inspect network interfaces, IP addresses, open ports, active connections, and routing.

Common checks:
- List interfaces and their IP addresses
- Show listening ports and the processes bound to them
- Display active connections (established TCP)
- Check DNS resolution
- Test connectivity to a remote host

### Hardware
Retrieve details about physical hardware: CPU model, RAM modules, GPU, storage devices, USB peripherals.

### Installed software
List installed packages, language runtimes, and key tool versions. Useful for environment replication and debugging compatibility issues.

## Quick reference: cross-platform command map

| Task | Linux | macOS | Windows (PowerShell) |
|------|-------|-------|---------------------|
| OS version | `cat /etc/os-release` | `sw_vers` | `Get-ComputerInfo` |
| Kernel | `uname -r` | `uname -r` | `[Environment]::OSVersion` |
| Uptime | `uptime` | `uptime` | `(Get-Date) - (gcim Win32_OS).LastBootUpTime` |
| CPU info | `lscpu` | `sysctl -n machdep.cpu.brand_string` | `gcim Win32_Processor` |
| Memory | `free -h` | `vm_stat` | `gcim Win32_OperatingSystem` |
| Disk space | `df -h` | `df -h` | `Get-PSDrive -PSProvider FileSystem` |
| Top processes | `ps aux --sort=-%cpu \| head` | `ps aux -r \| head` | `Get-Process \| Sort CPU -Desc \| Select -First 10` |
| Open ports | `ss -tlnp` | `lsof -iTCP -sTCP:LISTEN` | `Get-NetTCPConnection -State Listen` |
| Network IFs | `ip addr` | `ifconfig` | `Get-NetIPAddress` |

## Output interpretation guidelines

When presenting results to the user:

1. **Lead with the answer.** State the key finding first ("Disk is 94% full on `/`"), then provide supporting data.
2. **Use human-readable units.** Always prefer GB/MB over raw bytes, percentages over absolute numbers where both are available.
3. **Highlight anomalies.** If any metric falls in the Warning or Critical range, call it out explicitly.
4. **Provide actionable next steps.** If disk is full, suggest what to clean. If a process is consuming excessive CPU, name it and suggest investigation commands.
5. **Compare against baselines.** When the user asks "is this normal?", use the threshold tables above as reference points.

## Common troubleshooting patterns

### System is slow
1. Check load average vs core count (CPU contention).
2. Check available memory and swap activity (memory pressure).
3. Check disk I/O wait (storage bottleneck).
4. Identify top CPU and memory consumers.

### Out of disk space
1. Check `df -h` for filesystem usage.
2. Find largest directories with `du -sh /* | sort -rh | head`.
3. Check for large log files, build artifacts, and package caches.
4. Check inode usage (`df -i`) — many small files can exhaust inodes before space.

### Process investigation
1. Find the process by name or PID.
2. Check its CPU and memory usage over time.
3. Examine open files and network connections (`lsof -p PID`).
4. Check parent process to understand how it was started.

### Network connectivity issues
1. Check if the interface is up and has an IP address.
2. Test DNS resolution (`dig`, `nslookup`).
3. Test connectivity with `ping` or `curl`.
4. Check listening ports and firewall rules.
5. Review routing table for misconfigurations.
