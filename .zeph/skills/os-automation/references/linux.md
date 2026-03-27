# Linux OS Automation Reference

## Notifications

```bash
# Basic notification (requires libnotify-bin / notify-send)
notify-send "Title" "Body"

# Urgency levels: low, normal, critical
notify-send -u critical "Alert" "Something critical happened"
notify-send -u low "Info" "FYI"

# With icon (icon name or path)
notify-send -i dialog-information "Title" "Body"
notify-send -i /path/to/icon.png "Title" "Body"

# Expire after 5 seconds (milliseconds)
notify-send -t 5000 "Title" "Body"

# Check if notify-send is available
command -v notify-send && echo "available" || echo "not installed"
```

Note: on minimal systems, install with:
- Debian/Ubuntu: `sudo apt install libnotify-bin`
- Fedora: `sudo dnf install libnotify`
- Arch: `sudo pacman -S libnotify`

## Clipboard

### X11 (most desktop environments)

```bash
# Write to clipboard
echo "text" | xclip -selection clipboard
echo "text" | xsel --clipboard --input

# Read from clipboard
xclip -selection clipboard -o
xsel --clipboard --output

# Copy file contents
cat file.txt | xclip -selection clipboard
```

### Wayland (GNOME, KDE on Wayland)

```bash
# Detect Wayland session
echo $WAYLAND_DISPLAY   # non-empty = Wayland

# Write to clipboard
echo "text" | wl-copy
cat file.txt | wl-copy

# Read from clipboard
wl-paste
wl-paste --no-newline

# Paste image from clipboard
wl-paste --type image/png > screenshot.png
```

Install: `sudo apt install wl-clipboard` / `sudo pacman -S wl-clipboard`

## Screenshots

### scrot (X11)

```bash
# Full screen
scrot ~/screenshot.png

# With delay (2 seconds)
scrot -d 2 ~/screenshot.png

# Focused window only
scrot -u ~/window.png

# Interactive selection
scrot -s ~/selection.png

# Include window decorations
scrot -b ~/window-with-border.png

# Quality (1–100, default 75)
scrot -q 90 ~/screenshot.jpg
```

### gnome-screenshot

```bash
# Full screen
gnome-screenshot -f ~/screenshot.png

# Window
gnome-screenshot -w -f ~/window.png

# Area selection (interactive)
gnome-screenshot -a -f ~/area.png

# Copy to clipboard
gnome-screenshot -c
```

### grim (Wayland / Sway / GNOME on Wayland)

```bash
# Full screen
grim ~/screenshot.png

# Specific output
grim -o DP-1 ~/screenshot.png

# Region selection (with slurp)
grim -g "$(slurp)" ~/selection.png

# Copy to clipboard
grim - | wl-copy
```

Install: `sudo apt install grim slurp` / `sudo pacman -S grim slurp`

### maim (X11 alternative)

```bash
# Full screen
maim ~/screenshot.png

# Selection
maim -s ~/selection.png

# Specific window by ID
maim -i $(xdotool getactivewindow) ~/window.png
```

## Open Files and URLs

```bash
# Open URL in default browser
xdg-open https://example.com

# Open file with default app
xdg-open ~/Documents/report.pdf
xdg-open ~/Music/song.mp3

# Launch application
gtk-launch firefox.desktop
gtk-launch org.gnome.Nautilus.desktop

# Get default application for MIME type
xdg-mime query default text/html
xdg-mime query default application/pdf

# Set default application
xdg-mime default firefox.desktop text/html
```

## Volume Control

### ALSA (amixer)

```bash
# Get current volume
amixer get Master

# Set volume (percentage)
amixer set Master 50%

# Increase volume
amixer set Master 10%+

# Decrease volume
amixer set Master 10%-

# Mute
amixer set Master mute

# Unmute
amixer set Master unmute

# Toggle mute
amixer set Master toggle
```

### PulseAudio (pactl)

```bash
# List sinks (output devices)
pactl list sinks short

# Get volume of default sink
pactl get-sink-volume @DEFAULT_SINK@

# Set volume (percentage, 0–150%)
pactl set-sink-volume @DEFAULT_SINK@ 50%

# Increase / decrease
pactl set-sink-volume @DEFAULT_SINK@ +10%
pactl set-sink-volume @DEFAULT_SINK@ -10%

# Mute / unmute
pactl set-sink-mute @DEFAULT_SINK@ 1
pactl set-sink-mute @DEFAULT_SINK@ 0
pactl set-sink-mute @DEFAULT_SINK@ toggle

# Microphone (source) volume
pactl set-source-volume @DEFAULT_SOURCE@ 75%
```

### PipeWire (wpctl)

```bash
# Get volume
wpctl get-volume @DEFAULT_AUDIO_SINK@

# Set volume
wpctl set-volume @DEFAULT_AUDIO_SINK@ 0.5    # 0.0–1.0

# Limit to 100% max
wpctl set-volume -l 1.0 @DEFAULT_AUDIO_SINK@ 0.8

# Mute / unmute
wpctl set-mute @DEFAULT_AUDIO_SINK@ toggle
wpctl set-mute @DEFAULT_AUDIO_SINK@ 1
wpctl set-mute @DEFAULT_AUDIO_SINK@ 0
```

## WiFi

```bash
# List available networks
nmcli dev wifi list

# Show current connection
nmcli connection show --active

# Show device status
nmcli device status

# Connect to a network
nmcli dev wifi connect "SSID" password "passphrase"

# Disconnect
nmcli dev disconnect wlan0

# Turn WiFi on / off
nmcli radio wifi on
nmcli radio wifi off

# Legacy (if nmcli not available)
iwconfig wlan0
iwlist wlan0 scan
```

## Scheduled Tasks

### crontab (user-level, persistent)

```bash
# List current crontab
crontab -l

# Edit crontab
crontab -e

# Add entry non-interactively
(crontab -l 2>/dev/null; echo "0 9 * * 1-5 /path/to/script.sh >> /tmp/job.log 2>&1") | crontab -

# Remove all entries
crontab -r
```

Cron expression format: `minute hour day-of-month month day-of-week`
Examples:
- `0 9 * * 1-5` — weekdays at 09:00
- `*/15 * * * *` — every 15 minutes
- `@reboot` — run at startup

### systemd user timers (persistent, survives reboot)

Create `~/.config/systemd/user/myjob.service`:
```ini
[Unit]
Description=My scheduled job

[Service]
Type=oneshot
ExecStart=/path/to/script.sh
```

Create `~/.config/systemd/user/myjob.timer`:
```ini
[Unit]
Description=Run myjob daily at 09:00

[Timer]
OnCalendar=*-*-* 09:00:00
Persistent=true

[Install]
WantedBy=timers.target
```

```bash
# Enable and start
systemctl --user daemon-reload
systemctl --user enable --now myjob.timer

# Check status
systemctl --user status myjob.timer
systemctl --user list-timers
```

## Display Brightness

```bash
# brightnessctl (recommended)
brightnessctl get                  # current value
brightnessctl max                  # maximum value
brightnessctl set 50%              # set to 50%
brightnessctl set +10%             # increase by 10%
brightnessctl set 10%-             # decrease by 10%

# Install: sudo apt install brightnessctl

# xrandr (X11, software brightness)
xrandr --listmonitors
xrandr --output DP-1 --brightness 0.8   # 0.0–1.0

# ddcutil (hardware DDC/CI, requires i2c-dev kernel module)
ddcutil getvcp 10          # get brightness VCP code
ddcutil setvcp 10 50       # set brightness to 50
```

## System Control

```bash
# Text-to-speech
espeak "Hello from Zeph"
festival --tts <<< "Hello from Zeph"

# Suspend / sleep
systemctl suspend

# Lock screen (GNOME)
loginctl lock-session

# Lock screen (general X11)
xdg-screensaver lock

# Reboot
systemctl reboot

# Shutdown
systemctl poweroff
```

## Process Management

```bash
# Find process by name
pgrep -a firefox

# Send signal to process
pkill firefox
kill -TERM <PID>

# Check if process is running
pgrep -x "process-name" && echo "running" || echo "not running"
```
