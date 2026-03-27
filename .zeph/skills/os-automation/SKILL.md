---
name: os-automation
description: >
  Cross-platform OS automation — send desktop notifications, read and write
  the clipboard, take screenshots, open files and URLs in default apps,
  launch and manage applications, control system volume, check WiFi status,
  create scheduled tasks and cron jobs, and manage display brightness.
  Use when the user asks to send a notification, copy to clipboard, take a
  screenshot, open a file or URL, launch an app, adjust volume, check WiFi,
  schedule a task, or automate any OS-level action on macOS, Windows, or Linux.
license: MIT
compatibility: Uses built-in OS utilities; some features require specific tools (see per-platform references)
metadata:
  author: zeph
  version: "1.0"
---

# OS Automation

Automate OS-level actions using platform-native utilities.

Before running commands, detect the OS and load the matching reference for
platform-specific syntax:

- **macOS** — `references/macos.md` (osascript, pbcopy/pbpaste, screencapture, networksetup, open, launchctl)
- **Linux** — `references/linux.md` (notify-send, xclip/wl-copy, scrot/grim, xdg-open, amixer/pactl, nmcli)
- **Windows** — `references/windows.md` (PowerShell: Set-Clipboard, BurntToast, schtasks, netsh)

OS detection:
```bash
uname -s 2>/dev/null || echo Windows
```

## Capability Matrix

| Task | macOS | Linux | Windows |
|------|-------|-------|---------|
| Desktop notification | `osascript` | `notify-send` | PowerShell / BurntToast |
| Clipboard read | `pbpaste` | `xclip` / `wl-paste` | `Get-Clipboard` |
| Clipboard write | `pbcopy` | `xclip` / `wl-copy` | `Set-Clipboard` |
| Screenshot | `screencapture` | `scrot` / `grim` | `snippingtool` / PowerShell |
| Open file / URL | `open` | `xdg-open` | `Start-Process` |
| Launch app | `open -a "App"` | `gtk-launch` / app name | `Start-Process` |
| Volume control | `osascript` | `amixer` / `pactl` | PowerShell / nircmd |
| WiFi status | `networksetup` | `nmcli` / `iwconfig` | `netsh wlan` |
| Scheduled tasks | `crontab` / `launchctl` | `crontab` / `systemd` timer | `schtasks` / `Register-ScheduledTask` |
| Display brightness | `brightness` (brew) | `brightnessctl` / `xrandr` | PowerShell / WMI |
| Text-to-speech | `say` | `espeak` / `festival` | `Add-Type` / SAPI |
| System sleep | `pmset sleepnow` | `systemctl suspend` | `rundll32 powrprof.dll,SetSuspendState` |

## Workflow

1. Detect OS first — always.
2. Load the matching reference file for exact command syntax.
3. For read-only actions (clipboard read, WiFi status, screenshots to file), proceed directly.
4. For write or side-effect actions (volume change, notification, scheduled task), show what will be done and confirm with the user.
5. After execution, verify the result where possible (check clipboard content, confirm notification sent, verify cron entry exists).

## Safety Rules

**ALWAYS follow these rules — no exceptions:**

1. **Preview before acting** — for any action that modifies system state (volume, brightness, scheduled tasks, system settings), show the exact command and what it will do before executing.

2. **Confirm irreversible actions** — never remove cron entries, kill processes, or change system-wide settings (firewall, network, proxy) without explicit user confirmation.

3. **Never send keystrokes or click UI elements** without an explicit user request for each action.

4. **Screen captures may contain sensitive data** — treat screenshot output files as sensitive. Do not display screenshot content beyond what is needed.

5. **networksetup (macOS) requires sudo for some operations** — warn the user before running commands that may require elevated privileges. Never attempt `sudo` without the user's knowledge.

6. **PowerShell ExecutionPolicy** — if blocked by ExecutionPolicy on Windows, explain the restriction and suggest `Set-ExecutionPolicy RemoteSigned -Scope CurrentUser` only if the user wants to proceed.

7. **Scheduled tasks** — always display the full cron expression or task definition before creating. Explain what it will run, when, and with what permissions.

8. **Volume and brightness** — show the current level before changing it so the user can gauge the delta.

## Common Patterns

### Send a desktop notification
```bash
# macOS
osascript -e 'display notification "Task complete" with title "Zeph"'

# Linux
notify-send "Zeph" "Task complete"

# Windows (PowerShell)
[Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType=WindowsRuntime] | Out-Null
```

### Copy text to clipboard
```bash
# macOS
echo "text" | pbcopy

# Linux (X11)
echo "text" | xclip -selection clipboard

# Windows
Set-Clipboard -Value "text"
```

### Take a screenshot
```bash
# macOS — save to file
screencapture -x ~/Desktop/screenshot.png

# macOS — selection only
screencapture -s ~/Desktop/screenshot.png

# Linux (scrot)
scrot ~/screenshot.png

# Windows (PowerShell)
Add-Type -AssemblyName System.Windows.Forms
[System.Windows.Forms.SendKeys]::SendWait('%{PRTSC}')
```

### Open a file or URL
```bash
# macOS
open https://example.com
open ~/Documents/file.pdf

# Linux
xdg-open https://example.com
xdg-open ~/Documents/file.pdf

# Windows
Start-Process https://example.com
```

### Schedule a recurring task (cron)
```bash
# Show current crontab first
crontab -l

# Add entry (macOS / Linux)
(crontab -l 2>/dev/null; echo "0 9 * * 1-5 /path/to/script.sh") | crontab -
```

## Notes

- Some tools are optional and may not be installed on all systems (e.g. `notify-send` requires `libnotify`, `brightnessctl` may need installation on Linux).
- On Linux, clipboard tools differ for X11 (`xclip`, `xsel`) vs Wayland (`wl-copy`, `wl-paste`). Detect with `echo $WAYLAND_DISPLAY`.
- macOS `screencapture -x` suppresses the shutter sound.
- Always prefer the platform reference file for exact flags and edge cases.
