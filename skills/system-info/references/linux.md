# Linux System Diagnostics

Use these commands on Linux hosts.

## OS and kernel
```bash
cat /etc/os-release
uname -a
hostnamectl
```

## Uptime and load
```bash
uptime
cat /proc/loadavg
```

## CPU and memory
```bash
lscpu
free -h
vmstat 1 5
```

## Disk and filesystem
```bash
df -h
lsblk
du -sh ./*
```

## Top processes
```bash
ps aux --sort=-%cpu | head -20
ps aux --sort=-%mem | head -20
top -b -n 1 | head -40
```

## Network snapshot
```bash
ip -br a
ss -tulpen | head -40
```

## Error logs (recent)
```bash
journalctl -p err -n 100 --no-pager
```
