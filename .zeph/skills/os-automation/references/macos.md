# macOS OS Automation Reference

## Notifications

```bash
# Basic notification
osascript -e 'display notification "Message body" with title "Title"'

# Notification with subtitle
osascript -e 'display notification "Message body" with title "Title" subtitle "Subtitle"'

# Alert dialog (blocks until dismissed)
osascript -e 'display alert "Title" message "Body"'

# Alert with buttons
osascript -e 'display alert "Confirm?" buttons {"Cancel", "OK"} default button "OK"'
```

## Clipboard

```bash
# Write to clipboard
echo "text" | pbcopy
cat file.txt | pbcopy

# Read from clipboard
pbpaste

# Copy file path to clipboard
echo "$PWD/file.txt" | pbcopy
```

## Screenshots

```bash
# Full screen to file (silent — no shutter sound)
screencapture -x ~/Desktop/screenshot.png

# Selection (interactive crop)
screencapture -s ~/Desktop/screenshot.png

# Window only (click to select)
screencapture -w ~/Desktop/window.png

# Timed (5 second delay)
screencapture -T 5 ~/Desktop/screenshot.png

# Copy to clipboard instead of file
screencapture -c

# JPEG format
screencapture -t jpg ~/Desktop/screenshot.jpg

# Specific display (0 = main display)
screencapture -D 0 ~/Desktop/main.png
```

## Open Files, URLs, and Apps

```bash
# Open URL in default browser
open https://example.com

# Open file with default app
open ~/Documents/report.pdf

# Open file with specific app
open -a "Preview" ~/Desktop/image.png
open -a "TextEdit" ~/Documents/notes.txt

# Launch app by name
open -a "Safari"
open -a "Terminal"
open -a "Finder"

# Reveal file in Finder
open -R ~/Documents/file.txt

# Open folder in Finder
open ~/Downloads
```

## Volume Control

```bash
# Get current output volume (0–100)
osascript -e 'output volume of (get volume settings)'

# Get all volume settings
osascript -e 'get volume settings'
# Returns: output volume:75, input volume:75, alert volume:100, output muted:false

# Set output volume (0–100)
osascript -e 'set volume output volume 50'

# Mute / unmute
osascript -e 'set volume with output muted'
osascript -e 'set volume without output muted'

# Set input (microphone) volume
osascript -e 'set volume input volume 75'

# Set alert volume
osascript -e 'set volume alert volume 50'
```

## WiFi

```bash
# Current WiFi network
networksetup -getairportnetwork en0

# List all WiFi networks
/System/Library/PrivateFrameworks/Apple80211.framework/Versions/Current/Resources/airport -s

# Turn WiFi off / on
networksetup -setairportpower en0 off
networksetup -setairportpower en0 on

# Current WiFi status
networksetup -getairportpower en0

# List all network services
networksetup -listallnetworkservices

# Get IP address for a service
networksetup -getinfo "Wi-Fi"

# Set DNS servers (requires admin)
networksetup -setdnsservers "Wi-Fi" 1.1.1.1 8.8.8.8
```

## Scheduled Tasks (cron and launchd)

### crontab (user-level)

```bash
# List current crontab
crontab -l

# Edit crontab
crontab -e

# Add entry non-interactively
(crontab -l 2>/dev/null; echo "0 9 * * 1-5 /path/to/script.sh >> /tmp/job.log 2>&1") | crontab -

# Remove all cron entries
crontab -r
```

Cron expression format: `minute hour day-of-month month day-of-week`
Examples:
- `0 9 * * 1-5` — weekdays at 09:00
- `*/15 * * * *` — every 15 minutes
- `0 0 1 * *` — first day of each month at midnight

### launchd (persistent, survives reboot)

```bash
# List loaded agents
launchctl list

# Load a plist agent
launchctl load ~/Library/LaunchAgents/com.example.myjob.plist

# Unload an agent
launchctl unload ~/Library/LaunchAgents/com.example.myjob.plist

# Start immediately
launchctl start com.example.myjob
```

Example plist (`~/Library/LaunchAgents/com.example.myjob.plist`):
```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>com.example.myjob</string>
  <key>ProgramArguments</key>
  <array><string>/usr/local/bin/my-script.sh</string></array>
  <key>StartCalendarInterval</key>
  <dict><key>Hour</key><integer>9</integer><key>Minute</key><integer>0</integer></dict>
  <key>StandardOutPath</key><string>/tmp/myjob.log</string>
  <key>StandardErrorPath</key><string>/tmp/myjob-err.log</string>
</dict>
</plist>
```

## Display Brightness

```bash
# Install brightness CLI (one-time)
brew install brightness

# Get current brightness (0.0–1.0)
brightness -l

# Set brightness (0.0–1.0)
brightness 0.5    # 50%
brightness 1.0    # full
brightness 0.0    # off
```

Note: Without `brew install brightness`, use System Settings > Displays or keyboard keys.

## System Control

```bash
# Text-to-speech
say "Hello from Zeph"
say -v "Samantha" "Using a specific voice"
say -r 180 "Faster speech"   # words per minute

# Sleep immediately
pmset sleepnow

# Prevent sleep for 1 hour
caffeinate -t 3600

# Prevent sleep while a command runs
caffeinate -i ./long-running-script.sh

# Restart
sudo shutdown -r now

# Shut down
sudo shutdown -h now

# Lock screen
/System/Library/CoreServices/Menu\ Extras/User.menu/Contents/Resources/CGSession -suspend
```

## Browser Control (Safari and Chrome via AppleScript)

Note: for full browser automation, prefer the Playwright MCP skill (`browser`). Use these commands for lightweight OS-level control (open URL, manage tabs) without a running MCP server.

### Safari

```bash
# Open URL in Safari
osascript -e 'tell application "Safari" to open location "https://example.com"'

# Get URL of front tab
osascript -e 'tell application "Safari" to get URL of front document'

# Get page title of front tab
osascript -e 'tell application "Safari" to get name of front document'

# Open new tab with URL
osascript -e 'tell application "Safari"
    make new document with properties {URL:"https://example.com"}
end tell'

# Get URL of all tabs in front window
osascript -e 'tell application "Safari"
    set tabURLs to {}
    repeat with t in tabs of front window
        set end of tabURLs to URL of t
    end repeat
    return tabURLs
end tell'

# Close front tab
osascript -e 'tell application "Safari" to close front document'

# Reload front page
osascript -e 'tell application "Safari" to do JavaScript "location.reload()" in front document'

# Run JavaScript in front page
osascript -e 'tell application "Safari" to do JavaScript "document.title" in front document'
```

### Google Chrome

```bash
# Open URL in Chrome
osascript -e 'tell application "Google Chrome" to open location "https://example.com"'

# Get URL of active tab in front window
osascript -e 'tell application "Google Chrome" to get URL of active tab of front window'

# Get title of active tab
osascript -e 'tell application "Google Chrome" to get title of active tab of front window'

# Open new tab
osascript -e 'tell application "Google Chrome"
    tell front window to make new tab with properties {URL:"https://example.com"}
end tell'

# List all tab URLs in front window
osascript -e 'tell application "Google Chrome"
    set tabURLs to {}
    repeat with t in tabs of front window
        set end of tabURLs to URL of t
    end repeat
    return tabURLs
end tell'

# Execute JavaScript in active tab
osascript -e 'tell application "Google Chrome" to execute front window'\''s active tab javascript "document.title"'

# Close active tab
osascript -e 'tell application "Google Chrome" to delete active tab of front window'

# Reload active tab
osascript -e 'tell application "Google Chrome" to reload active tab of front window'
```

## Finder and File Operations

```bash
# Reveal file in Finder
open -R "/path/to/file"

# Empty Trash
osascript -e 'tell application "Finder" to empty trash'

# Get frontmost application name
osascript -e 'tell application "System Events" to get name of first application process whose frontmost is true'

# Activate an application
osascript -e 'tell application "Safari" to activate'
```
