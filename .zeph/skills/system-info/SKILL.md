---
name: system-info
description: Retrieve system diagnostics and resource usage. Use when the user asks about OS version, disk space, memory usage, running processes, CPU load, or system uptime.
---
# System Info

Collect host diagnostics and resource metrics.

Before running commands, detect the OS and use the matching reference:

- **Linux**: `references/linux.md`
- **macOS**: `references/macos.md`
- **Windows**: `references/windows.md` (PowerShell)

```bash
uname -s 2>/dev/null || echo Windows
```

## Workflow
1. Gather only the metrics the user requested.
2. Prefer read-only commands first.
3. If a command is unavailable, use a fallback from the same OS reference.
4. Return a short summary with key numbers (percentages, GB usage, top processes).
