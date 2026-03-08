# Windows System Diagnostics (PowerShell)

Use these commands in PowerShell on Windows hosts.

## OS and kernel
```powershell
Get-ComputerInfo | Select-Object WindowsProductName, WindowsVersion, OsHardwareAbstractionLayer
Get-CimInstance Win32_OperatingSystem | Select-Object Caption, Version, BuildNumber, LastBootUpTime
```

## Uptime and load
```powershell
(Get-Date) - (Get-CimInstance Win32_OperatingSystem).LastBootUpTime
Get-Counter '\Processor(_Total)\% Processor Time' -SampleInterval 1 -MaxSamples 5
```

## CPU and memory
```powershell
Get-CimInstance Win32_Processor | Select-Object Name, NumberOfCores, NumberOfLogicalProcessors
Get-CimInstance Win32_OperatingSystem | Select-Object TotalVisibleMemorySize, FreePhysicalMemory
```

## Disk and filesystem
```powershell
Get-PSDrive -PSProvider FileSystem
Get-CimInstance Win32_LogicalDisk | Select-Object DeviceID, VolumeName, Size, FreeSpace
```

## Top processes
```powershell
Get-Process | Sort-Object CPU -Descending | Select-Object -First 20 Name, Id, CPU, WS
Get-Process | Sort-Object WorkingSet -Descending | Select-Object -First 20 Name, Id, WorkingSet
```

## Network snapshot
```powershell
Get-NetIPConfiguration
Get-NetTCPConnection | Select-Object -First 40 LocalAddress, LocalPort, RemoteAddress, RemotePort, State
```

## Recent critical events
```powershell
Get-WinEvent -LogName System -MaxEvents 100 | Where-Object { $_.LevelDisplayName -in @("Error", "Critical") }
```
