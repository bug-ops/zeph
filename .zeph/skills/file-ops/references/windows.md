# Windows-Specific File Operations (PowerShell)

## List directory contents

```powershell
Get-ChildItem                             # ls equivalent
Get-ChildItem -Force                      # include hidden files
Get-ChildItem -Recurse -Depth 3          # recursive with depth limit
Get-ChildItem -Recurse -File | Sort-Object Length -Descending | Select-Object -First 10  # top 10 largest
```

## Find files by name or pattern

By name:
```powershell
Get-ChildItem -Recurse -Filter "config.toml" -File
```

By extension:
```powershell
Get-ChildItem -Recurse -Include "*.rs" -File
```

Multiple extensions:
```powershell
Get-ChildItem -Recurse -Include "*.rs","*.toml" -File
```

Exclude directories:
```powershell
Get-ChildItem -Recurse -Include "*.rs" -File | Where-Object { $_.FullName -notmatch '\\target\\|\\\.git\\' }
```

Files modified in the last 24 hours:
```powershell
Get-ChildItem -Recurse -File | Where-Object { $_.LastWriteTime -gt (Get-Date).AddDays(-1) }
```

Files larger than 1MB:
```powershell
Get-ChildItem -Recurse -File | Where-Object { $_.Length -gt 1MB }
```

## Search text inside files

Basic search (recursive):
```powershell
Select-String -Path "*.rs" -Pattern "pattern" -Recurse
```

Case-insensitive (default, use `-CaseSensitive` for exact):
```powershell
Select-String -Path "*.rs" -Pattern "pattern" -Recurse -CaseSensitive
```

With context:
```powershell
Select-String -Path "*.rs" -Pattern "pattern" -Recurse -Context 3,3
```

Regex:
```powershell
Select-String -Path "*.rs" -Pattern "fn\s+\w+\(" -Recurse
```

List only filenames:
```powershell
Select-String -Path "*.rs" -Pattern "pattern" -Recurse | Select-Object -Unique Path
```

Multi-pattern:
```powershell
Select-String -Path "*.rs" -Pattern "error|warn|panic" -Recurse
```

Exclude directories:
```powershell
Get-ChildItem -Recurse -Include "*.rs" -File |
  Where-Object { $_.FullName -notmatch '\\target\\' } |
  Select-String -Pattern "pattern"
```

## Read file contents

Entire file:
```powershell
Get-Content file.txt
```

First N lines:
```powershell
Get-Content file.txt -TotalCount 20
```

Last N lines:
```powershell
Get-Content file.txt -Tail 20
```

Specific line range (e.g. lines 50-70):
```powershell
Get-Content file.txt | Select-Object -Skip 49 -First 21
```

With line numbers:
```powershell
Get-Content file.txt | ForEach-Object { "{0,5}: {1}" -f $_.ReadCount, $_ }
```

## File metadata

File info:
```powershell
Get-Item file.txt | Format-List *
```

File size (human-readable):
```powershell
Get-Item file.txt | Select-Object Name, @{N='SizeMB';E={[math]::Round($_.Length/1MB,2)}}
```

File hash:
```powershell
Get-FileHash file.txt -Algorithm SHA256
Get-FileHash file.txt -Algorithm MD5
```

Directory size:
```powershell
(Get-ChildItem -Recurse -File | Measure-Object -Property Length -Sum).Sum / 1MB
```

Count files by extension:
```powershell
Get-ChildItem -Recurse -File |
  Group-Object Extension |
  Sort-Object Count -Descending |
  Select-Object Count, Name
```

## Compare files

```powershell
Compare-Object (Get-Content file1.txt) (Get-Content file2.txt)
```

With line indicator:
```powershell
Compare-Object (Get-Content file1.txt) (Get-Content file2.txt) -IncludeEqual |
  Format-Table InputObject, SideIndicator
```

## File watcher

Monitor directory for changes:
```powershell
$watcher = New-Object System.IO.FileSystemWatcher
$watcher.Path = "."
$watcher.Filter = "*.rs"
$watcher.IncludeSubdirectories = $true
$watcher.EnableRaisingEvents = $true
Register-ObjectEvent $watcher "Changed" -Action { Write-Host "Changed: $($Event.SourceEventArgs.FullPath)" }
```

## Permissions

View ACL:
```powershell
Get-Acl file.txt | Format-List
```

Check if file exists:
```powershell
Test-Path "file.txt"
Test-Path "directory" -PathType Container
```
