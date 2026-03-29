---
name: network
category: web
description: >
  Network diagnostics, DNS lookup, port scanning, and connectivity testing.
  Use when the user asks to check if a host is reachable, trace network routes,
  resolve DNS records, inspect open ports, debug HTTP requests, check SSL/TLS
  certificates, test bandwidth, diagnose network latency, or troubleshoot
  connectivity issues using ping, traceroute, dig, nslookup, netstat, ss,
  curl, and related tools.
license: MIT
compatibility: Requires standard networking tools (ping, curl, dig). Some tools may need installation on minimal systems
metadata:
  author: zeph
  version: "1.0"
---

# Network Diagnostics

Before running commands, detect the OS and load the matching reference for
platform-specific syntax:

- **Linux** — `references/linux.md` (ss, ip, iptables/nftables, iproute2)
- **macOS** — `references/macos.md` (lsof, ifconfig, pfctl, networkQuality)
- **Windows** — `references/windows.md` (PowerShell: Test-NetConnection, Get-NetTCPConnection)

OS detection:
```bash
uname -s 2>/dev/null || echo Windows
```

## Quick Reference

| Task | Command |
|------|---------|
| Check host reachability | `ping -c 4 host` |
| Trace route to host | `traceroute host` |
| DNS lookup | `dig +short host` |
| Reverse DNS | `dig -x IP` |
| Check specific port | `nc -zv host port` |
| HTTP status | `curl -sI -o /dev/null -w "%{http_code}" URL` |
| SSL certificate | `openssl s_client -connect host:443 </dev/null` |

## Diagnostic Workflow

### Is a service accessible?

1. **DNS resolves** — `dig +short host`
2. **Host reachable** — `ping -c 2 host`
3. **Port open** — `nc -zv -w 3 host 443`
4. **HTTP responds** — `curl -sI -o /dev/null -w "%{http_code}" https://host`
5. **SSL valid** — `openssl s_client -connect host:443 </dev/null 2>/dev/null | openssl x509 -noout -checkend 0`

### Diagnose slow connections

1. **DNS time** — `dig +stats host | grep "Query time"`
2. **Latency** — `ping -c 10 host`
3. **Route bottleneck** — `mtr -r -c 10 host` (or `traceroute host`)
4. **HTTP timing** — use curl timing breakdown (see below)

## HTTP Debugging (curl, cross-platform)

```bash
# Response headers
curl -sI https://host

# Status code only
curl -s -o /dev/null -w "%{http_code}" https://host

# Full verbose debug
curl -v https://host 2>&1

# Follow redirects
curl -sIL https://host

# Timing breakdown
curl -s -o /dev/null -w "\
  DNS:     %{time_namelookup}s\n\
  Connect: %{time_connect}s\n\
  TLS:     %{time_appconnect}s\n\
  TTFB:    %{time_starttransfer}s\n\
  Total:   %{time_total}s\n\
  Status:  %{http_code}\n\
  Size:    %{size_download} bytes\n" https://host

# Resolve to specific IP (bypass DNS)
curl --resolve host:443:93.184.216.34 https://host

# Test specific Host header
curl -s -H "Host: host" http://10.0.0.1/
```

## SSL/TLS Inspection (cross-platform)

```bash
# Certificate details
openssl s_client -connect host:443 </dev/null 2>/dev/null | \
  openssl x509 -noout -subject -issuer -dates

# Expiration check
openssl s_client -connect host:443 </dev/null 2>/dev/null | \
  openssl x509 -noout -enddate

# Full certificate chain
openssl s_client -connect host:443 -showcerts </dev/null

# TLS version check
openssl s_client -connect host:443 -tls1_2 </dev/null 2>&1 | head -5
openssl s_client -connect host:443 -tls1_3 </dev/null 2>&1 | head -5

# Quick SSL check via curl
curl -vI https://host 2>&1 | grep -E '^\*'
```

## DNS Concepts

| Record | Purpose |
|--------|---------|
| A / AAAA | IPv4 / IPv6 address |
| MX | Mail exchange server |
| NS | Authoritative name server |
| TXT | SPF, DKIM, DMARC, verification |
| CNAME | Alias to another domain |
| SOA | Zone authority info |
| SRV | Service location (port + host) |
| CAA | Certificate authority authorization |
| PTR | Reverse DNS (IP → hostname) |

## Important Notes

- `ping` uses ICMP — some hosts block it; a failed ping does not mean the host is down
- `traceroute` uses UDP on Linux, ICMP on macOS — use OS-specific reference for flags
- `dig` is preferred over `nslookup` for scripting (predictable output)
- Port/socket tools differ by OS: Linux uses `ss`, macOS uses `lsof`, Windows uses `Get-NetTCPConnection`
- Always use timeouts (`-W`, `-w`, `--connect-timeout`) in scripts
- Running network scans against systems you do not own may violate terms of service
