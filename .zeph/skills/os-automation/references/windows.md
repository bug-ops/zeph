# Windows OS Automation Reference (PowerShell)

All commands in this reference run in PowerShell (5.1+ or PowerShell 7+).

## Notifications

### Using BurntToast module (recommended, persistent)

```powershell
# Install module (one-time, user scope)
Install-Module -Name BurntToast -Scope CurrentUser -Force

# Basic notification
New-BurntToastNotification -Text "Title", "Body"

# With app logo
New-BurntToastNotification -Text "Title", "Body" -AppLogo "C:\path\to\icon.png"

# Silent notification
New-BurntToastNotification -Text "Title", "Body" -Silent
```

### Using Windows Runtime directly (no external module)

```powershell
[Windows.UI.Notifications.ToastNotificationManager,
 Windows.UI.Notifications, ContentType=WindowsRuntime] | Out-Null
[Windows.Data.Xml.Dom.XmlDocument,
 Windows.Data.Xml.Dom.XmlDocument, ContentType=WindowsRuntime] | Out-Null

$template = [Windows.UI.Notifications.ToastTemplateType]::ToastText02
$xml = [Windows.UI.Notifications.ToastNotificationManager]::GetTemplateContent($template)
$xml.GetElementsByTagName("text")[0].AppendChild($xml.CreateTextNode("Title"))
$xml.GetElementsByTagName("text")[1].AppendChild($xml.CreateTextNode("Body"))
$toast = [Windows.UI.Notifications.ToastNotification]::new($xml)
[Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier("PowerShell").Show($toast)
```

## Clipboard

```powershell
# Write to clipboard
Set-Clipboard -Value "text to copy"

# Write multi-line
"Line1`nLine2" | Set-Clipboard

# Write file contents
Get-Content "C:\path\to\file.txt" | Set-Clipboard

# Read from clipboard
$text = Get-Clipboard
$text = Get-Clipboard -Raw    # preserve line endings

# Read clipboard as text
Get-Clipboard -Format Text

# Clear clipboard
Set-Clipboard -Value $null
```

## Screenshots

### Using .NET (no external tools)

```powershell
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing

$screen = [System.Windows.Forms.Screen]::PrimaryScreen
$bounds = $screen.Bounds
$bitmap = [System.Drawing.Bitmap]::new($bounds.Width, $bounds.Height)
$graphics = [System.Drawing.Graphics]::FromImage($bitmap)
$graphics.CopyFromScreen($bounds.Location, [System.Drawing.Point]::Empty, $bounds.Size)
$bitmap.Save("$env:USERPROFILE\Desktop\screenshot.png")
$graphics.Dispose()
$bitmap.Dispose()
```

### Using Snipping Tool (interactive)

```powershell
# Open Snipping Tool
Start-Process snippingtool

# Open Snip & Sketch (Windows 10/11)
Start-Process "ms-screensketch:"

# Keyboard shortcut equivalent (runs silently)
Add-Type -AssemblyName System.Windows.Forms
[System.Windows.Forms.SendKeys]::SendWait("%{PRTSC}")  # Alt+PrintScreen (active window)
```

## Open Files and URLs

```powershell
# Open URL in default browser
Start-Process "https://example.com"

# Open file with default app
Start-Process "C:\Users\user\Documents\report.pdf"
Invoke-Item "C:\Users\user\Documents\report.pdf"

# Open folder in Explorer
Start-Process explorer.exe "C:\Users\user\Downloads"

# Run executable
Start-Process "notepad.exe"
Start-Process "C:\Program Files\App\app.exe"

# Run with arguments
Start-Process "powershell.exe" -ArgumentList "-File C:\scripts\job.ps1"

# Open app from Start Menu by name
Start-Process "calc"         # Calculator
Start-Process "mspaint"      # Paint
Start-Process "notepad"      # Notepad
```

## Volume Control

### Using nircmd (if installed)

```powershell
# Set master volume (0–65535)
nircmd changesysvolume 32767    # ~50%
nircmd changesysvolume 0        # 0% (mute)

# Mute / unmute
nircmd mutesysvolume 1    # mute
nircmd mutesysvolume 0    # unmute
nircmd mutesysvolume 2    # toggle

# Download: https://www.nirsoft.net/utils/nircmd.html
```

### Using COM AudioEndpointVolume (no external tools)

```powershell
Add-Type -TypeDefinition @'
using System.Runtime.InteropServices;
[Guid("5CDF2C82-841E-4546-9722-0CF74078229A"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
interface IAudioEndpointVolume {
    int _VtblGap1_6();
    int SetMasterVolumeLevelScalar(float fLevel, System.Guid pguidEventContext);
    int _VtblGap2_1();
    int GetMasterVolumeLevelScalar(out float pfLevel);
    int SetMute([MarshalAs(UnmanagedType.Bool)] bool bMute, System.Guid pguidEventContext);
    int GetMute([MarshalAs(UnmanagedType.Bool)] out bool pbMute);
}
'@

# Get current volume
$vol = [System.Runtime.InteropServices.Marshal]::GetActiveObject("MMDeviceEnumerator")
# (Simplified — use AudioDeviceCmdlets module for a cleaner API)

# Recommended: install AudioDeviceCmdlets
Install-Module -Name AudioDeviceCmdlets -Scope CurrentUser -Force
Get-AudioDevice -PlaybackVolume        # get current %
Set-AudioDevice -PlaybackVolume 50     # set to 50%
Set-AudioDevice -PlaybackMute $true    # mute
Set-AudioDevice -PlaybackMute $false   # unmute
```

## WiFi

```powershell
# List available networks
netsh wlan show networks mode=bssid

# Show current connection
netsh wlan show interfaces

# Show saved profiles
netsh wlan show profiles

# Connect to a saved profile
netsh wlan connect name="ProfileName"

# Disconnect
netsh wlan disconnect

# Turn WiFi on / off
netsh interface set interface "Wi-Fi" enable
netsh interface set interface "Wi-Fi" disable

# PowerShell 7+ with netsh
Get-NetAdapter -Name "Wi-Fi"
Get-NetConnectionProfile
```

## Scheduled Tasks

### Using schtasks (cmd-style, available everywhere)

```powershell
# List all tasks
schtasks /query /fo LIST /v

# Create a daily task at 09:00
schtasks /create /tn "MyJob" /tr "powershell.exe -File C:\scripts\job.ps1" /sc daily /st 09:00

# Create a task that runs at logon
schtasks /create /tn "MyStartupJob" /tr "C:\scripts\startup.bat" /sc onlogon

# Run task immediately
schtasks /run /tn "MyJob"

# Delete task
schtasks /delete /tn "MyJob" /f
```

### Using Register-ScheduledTask (PowerShell, more control)

```powershell
# Create action
$action = New-ScheduledTaskAction -Execute "powershell.exe" -Argument "-File C:\scripts\job.ps1"

# Create trigger (daily at 09:00)
$trigger = New-ScheduledTaskTrigger -Daily -At "09:00"

# Register task
Register-ScheduledTask -TaskName "MyJob" -Action $action -Trigger $trigger -RunLevel Highest

# List registered tasks
Get-ScheduledTask | Where-Object State -eq "Ready" | Select TaskName, TaskPath

# Run immediately
Start-ScheduledTask -TaskName "MyJob"

# Remove task
Unregister-ScheduledTask -TaskName "MyJob" -Confirm:$false
```

## Display Brightness

```powershell
# Get current brightness (WMI)
(Get-WmiObject -Namespace root/WMI -Class WmiMonitorBrightness).CurrentBrightness

# Set brightness (0–100)
(Get-WmiObject -Namespace root/WMI -Class WmiMonitorBrightnessMethods).WmiSetBrightness(1, 50)

# PowerShell 7 (Get-CimInstance)
Get-CimInstance -Namespace root/WMI -ClassName WmiMonitorBrightness |
    Select-Object CurrentBrightness

Invoke-CimMethod -Namespace root/WMI -ClassName WmiMonitorBrightnessMethods `
    -MethodName WmiSetBrightness -Arguments @{Timeout=1; Brightness=60}
```

Note: WMI brightness control works only on laptops with supported display drivers (not external monitors).

## System Control

```powershell
# Lock workstation
rundll32.exe user32.dll,LockWorkStation

# Sleep
rundll32.exe powrprof.dll,SetSuspendState 0,1,0

# Restart
Restart-Computer -Force

# Shutdown
Stop-Computer -Force

# Text-to-speech (SAPI)
$sapi = New-Object -ComObject SAPI.SpVoice
$sapi.Speak("Hello from Zeph")

# List available voices
$sapi.GetVoices() | ForEach-Object { $_.GetDescription() }

# Change voice
$sapi.Voice = $sapi.GetVoices().Item(1)
$sapi.Speak("Different voice")
```

## ExecutionPolicy Note

If scripts are blocked by ExecutionPolicy, PowerShell will show an error like:
`File cannot be loaded because running scripts is disabled on this system.`

To allow user-scope scripts (recommended over machine-scope):
```powershell
Set-ExecutionPolicy RemoteSigned -Scope CurrentUser
```

Always inform the user before suggesting this change. It relaxes a security boundary.

## Process Management

```powershell
# Find process by name
Get-Process -Name "firefox" -ErrorAction SilentlyContinue

# Stop process
Stop-Process -Name "notepad" -Force

# Check if process is running
$running = Get-Process -Name "app" -ErrorAction SilentlyContinue
if ($running) { "running" } else { "not found" }
```
