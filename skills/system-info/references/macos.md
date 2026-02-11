# macOS System Diagnostics

Use these commands on macOS hosts.

## OS and kernel
```bash
sw_vers
uname -a
system_profiler SPSoftwareDataType
```

## Uptime and load
```bash
uptime
sysctl -n vm.loadavg
```

## CPU and memory
```bash
sysctl -n machdep.cpu.brand_string
vm_stat
memory_pressure
```

## Disk and filesystem
```bash
df -h
diskutil list
du -sh ./*
```

## Top processes
```bash
ps aux | sort -nrk 3 | head -20
ps aux | sort -nrk 4 | head -20
top -l 1 | head -40
```

## Network snapshot
```bash
ifconfig
netstat -an | head -40
```

## Recent system errors
```bash
log show --last 1h --predicate 'eventMessage CONTAINS[c] "error"' --style compact
```
