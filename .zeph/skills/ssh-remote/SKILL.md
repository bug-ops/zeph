---
name: ssh-remote
description: >
  Manage SSH connections, key management, remote command execution, tunneling,
  and file transfer with scp and rsync. Use when the user asks to connect to
  a remote server, generate SSH keys, set up port forwarding or tunnels,
  transfer files remotely, configure jump hosts, manage known_hosts, or
  set up SSH config for multiple hosts.
license: MIT
compatibility: Requires openssh-client (pre-installed on macOS and most Linux distributions)
metadata:
  author: zeph
  version: "1.0"
---

# SSH, SCP, and Rsync

## Quick Reference

| Task | Command |
|------|---------|
| Connect | `ssh user@host` |
| Connect on port | `ssh -p 2222 user@host` |
| Run remote command | `ssh user@host 'command'` |
| Generate key | `ssh-keygen -t ed25519` |
| Copy key to server | `ssh-copy-id user@host` |
| Copy file to remote | `scp file.txt user@host:/path/` |
| Copy file from remote | `scp user@host:/path/file.txt .` |
| Sync directory | `rsync -avz dir/ user@host:/path/` |
| Local tunnel | `ssh -L 8080:localhost:80 user@host` |

## SSH Key Management

### Generate Keys

```bash
# Ed25519 (recommended — fast, secure, short keys)
ssh-keygen -t ed25519 -C "user@hostname"

# Ed25519 with custom filename
ssh-keygen -t ed25519 -f ~/.ssh/id_project -C "user@hostname"

# RSA 4096-bit (for legacy systems that don't support Ed25519)
ssh-keygen -t rsa -b 4096 -C "user@hostname"

# Generate without passphrase (for automation only)
ssh-keygen -t ed25519 -f ~/.ssh/id_automation -N ""

# Change passphrase on existing key
ssh-keygen -p -f ~/.ssh/id_ed25519

# View key fingerprint
ssh-keygen -lf ~/.ssh/id_ed25519.pub

# View key in different format
ssh-keygen -lf ~/.ssh/id_ed25519.pub -E md5
```

### Deploy Public Key

```bash
# Copy public key to remote server (recommended)
ssh-copy-id user@host

# Copy specific key
ssh-copy-id -i ~/.ssh/id_project.pub user@host

# Copy to non-standard port
ssh-copy-id -p 2222 user@host

# Manual method (when ssh-copy-id is unavailable)
cat ~/.ssh/id_ed25519.pub | ssh user@host 'mkdir -p ~/.ssh && cat >> ~/.ssh/authorized_keys && chmod 600 ~/.ssh/authorized_keys'
```

### SSH Agent

```bash
# Start agent (if not running)
eval "$(ssh-agent -s)"

# Add key to agent
ssh-add ~/.ssh/id_ed25519

# Add key with timeout (auto-remove after 1 hour)
ssh-add -t 3600 ~/.ssh/id_ed25519

# List keys in agent
ssh-add -l

# Remove specific key
ssh-add -d ~/.ssh/id_ed25519

# Remove all keys
ssh-add -D

# macOS: add key to Keychain
ssh-add --apple-use-keychain ~/.ssh/id_ed25519
```

## SSH Connection

### Basic Connection

```bash
# Connect as specific user
ssh user@host

# Connect on non-standard port
ssh -p 2222 user@host

# Connect with specific key
ssh -i ~/.ssh/id_project user@host

# Verbose output (debugging)
ssh -v user@host      # level 1
ssh -vv user@host     # level 2
ssh -vvv user@host    # level 3 (most verbose)

# Force password authentication
ssh -o PreferredAuthentications=password user@host

# Force key authentication
ssh -o PreferredAuthentications=publickey user@host

# Disable host key checking (lab/ephemeral hosts only)
ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null user@host

# Connect with X11 forwarding
ssh -X user@host

# Allocate pseudo-terminal (for interactive commands via script)
ssh -t user@host 'sudo systemctl restart nginx'
```

### Remote Command Execution

```bash
# Run single command
ssh user@host 'uptime'

# Run command with arguments
ssh user@host 'df -h /home'

# Run multiple commands
ssh user@host 'cd /app && git pull && systemctl restart app'

# Run command with sudo
ssh -t user@host 'sudo systemctl status nginx'

# Run command with environment variable
ssh user@host 'LANG=C df -h'

# Pipe local data to remote command
cat data.csv | ssh user@host 'cat > /tmp/data.csv'

# Run local script on remote host
ssh user@host 'bash -s' < local_script.sh

# Run local script with arguments
ssh user@host 'bash -s' < local_script.sh -- arg1 arg2

# Capture remote output in local variable
result=$(ssh user@host 'cat /etc/hostname')
```

## SSH Config (~/.ssh/config)

```
# Default settings for all hosts
Host *
    ServerAliveInterval 60
    ServerAliveCountMax 3
    AddKeysToAgent yes
    IdentitiesOnly yes

# Named host with short alias
Host myserver
    HostName 192.168.1.100
    User admin
    Port 2222
    IdentityFile ~/.ssh/id_project

# Jump host (bastion) setup
Host bastion
    HostName bastion.example.com
    User jumpuser
    IdentityFile ~/.ssh/id_bastion

Host internal-*
    ProxyJump bastion
    User admin
    IdentityFile ~/.ssh/id_internal

Host internal-web
    HostName 10.0.1.10

Host internal-db
    HostName 10.0.1.20

# Wildcard host pattern
Host *.staging.example.com
    User deploy
    IdentityFile ~/.ssh/id_staging

# Tunnel configuration (persistent)
Host tunnel-db
    HostName db-server.example.com
    User admin
    LocalForward 5432 localhost:5432
    IdentityFile ~/.ssh/id_db

# Multiple jump hosts
Host deep-internal
    HostName 10.0.2.50
    ProxyJump bastion1,bastion2
    User admin
```

Usage after config:
```bash
# Connect using alias
ssh myserver

# Connect to internal host via bastion (automatic jump)
ssh internal-web

# Start DB tunnel
ssh -N tunnel-db   # then connect to localhost:5432
```

## SSH Tunneling (Port Forwarding)

### Local Port Forwarding (-L)

Access a remote service through a local port.

```bash
# Forward local 8080 to remote's localhost:80
ssh -L 8080:localhost:80 user@host

# Access remote database via local port
ssh -L 5432:localhost:5432 user@db-server

# Forward to a third host through SSH server
ssh -L 3306:db-host:3306 user@bastion

# Background tunnel (no shell)
ssh -fNL 8080:localhost:80 user@host

# Bind to all interfaces (not just localhost)
ssh -L 0.0.0.0:8080:localhost:80 user@host

# Multiple forwards
ssh -L 8080:localhost:80 -L 8443:localhost:443 user@host
```

### Remote Port Forwarding (-R)

Make a local service accessible from the remote side.

```bash
# Expose local port 3000 on remote port 9000
ssh -R 9000:localhost:3000 user@host

# Background tunnel
ssh -fNR 9000:localhost:3000 user@host

# Bind to all interfaces on remote (requires GatewayPorts yes in sshd_config)
ssh -R 0.0.0.0:9000:localhost:3000 user@host
```

### Dynamic Port Forwarding (-D) — SOCKS Proxy

```bash
# Create SOCKS5 proxy on local port 1080
ssh -D 1080 user@host

# Background SOCKS proxy
ssh -fND 1080 user@host

# Use with curl
curl --socks5 localhost:1080 https://example.com

# Use with any application that supports SOCKS5
export ALL_PROXY=socks5://localhost:1080
```

### Jump Hosts (ProxyJump)

```bash
# Single jump host
ssh -J bastion user@internal-server

# Multiple jump hosts
ssh -J bastion1,bastion2 user@internal-server

# Jump with specific user and port
ssh -J jumpuser@bastion:2222 admin@internal

# SCP through jump host
scp -J bastion file.txt user@internal:/path/

# Rsync through jump host
rsync -avz -e "ssh -J bastion" dir/ user@internal:/path/
```

## File Transfer — SCP

```bash
# Copy file to remote
scp file.txt user@host:/remote/path/

# Copy file from remote
scp user@host:/remote/file.txt /local/path/

# Copy directory recursively
scp -r dir/ user@host:/remote/path/

# Custom port
scp -P 2222 file.txt user@host:/path/

# Specific key
scp -i ~/.ssh/id_project file.txt user@host:/path/

# Preserve timestamps and permissions
scp -p file.txt user@host:/path/

# Bandwidth limit (KB/s)
scp -l 1000 large-file.tar.gz user@host:/path/

# Copy between two remote hosts
scp user1@host1:/path/file.txt user2@host2:/path/

# Multiple files
scp file1.txt file2.txt user@host:/path/

# Through jump host
scp -J bastion file.txt user@internal:/path/
```

## File Transfer — Rsync

Rsync transfers only changed parts of files, making subsequent syncs much faster.

```bash
# Sync directory to remote (trailing / matters)
rsync -avz dir/ user@host:/remote/dir/

# Sync from remote to local
rsync -avz user@host:/remote/dir/ local-dir/

# Dry run (show what would change)
rsync -avzn dir/ user@host:/remote/dir/

# Delete files on destination that don't exist in source
rsync -avz --delete dir/ user@host:/remote/dir/

# Exclude patterns
rsync -avz --exclude='*.log' --exclude='node_modules' dir/ user@host:/path/

# Exclude from file
rsync -avz --exclude-from='rsync-exclude.txt' dir/ user@host:/path/

# Custom SSH port
rsync -avz -e "ssh -p 2222" dir/ user@host:/path/

# Custom SSH key
rsync -avz -e "ssh -i ~/.ssh/id_project" dir/ user@host:/path/

# Bandwidth limit (KB/s)
rsync -avz --bwlimit=1000 dir/ user@host:/path/

# Show progress
rsync -avz --progress dir/ user@host:/path/
rsync -avz --info=progress2 dir/ user@host:/path/   # overall progress

# Compress during transfer
rsync -avz dir/ user@host:/path/

# Preserve hard links
rsync -avzH dir/ user@host:/path/

# Resume interrupted transfer
rsync -avz --partial --append dir/ user@host:/path/

# Checksum-based comparison (instead of timestamp+size)
rsync -avzc dir/ user@host:/path/
```

### Rsync Flags

| Flag | Meaning |
|------|---------|
| `-a` | Archive mode (recursive, preserves permissions, symlinks, timestamps, group, owner) |
| `-v` | Verbose |
| `-z` | Compress during transfer |
| `-n` | Dry run |
| `-P` | Show progress + keep partial files |
| `-H` | Preserve hard links |
| `-e` | Specify remote shell |
| `--delete` | Delete extraneous files on receiver |
| `--exclude` | Exclude pattern |
| `--bwlimit` | Bandwidth limit in KB/s |

## Known Hosts Management

```bash
# View known hosts
cat ~/.ssh/known_hosts

# Remove specific host entry
ssh-keygen -R hostname
ssh-keygen -R "[hostname]:port"
ssh-keygen -R 192.168.1.100

# Hash known hosts (privacy)
ssh-keygen -H -f ~/.ssh/known_hosts

# Scan and add host key manually
ssh-keyscan -t ed25519 hostname >> ~/.ssh/known_hosts
ssh-keyscan -p 2222 hostname >> ~/.ssh/known_hosts
```

## Permissions Reference

SSH is strict about file permissions. Incorrect permissions cause silent authentication failures.

```bash
# Directory permissions
chmod 700 ~/.ssh

# Private key
chmod 600 ~/.ssh/id_ed25519

# Public key
chmod 644 ~/.ssh/id_ed25519.pub

# authorized_keys
chmod 600 ~/.ssh/authorized_keys

# config
chmod 600 ~/.ssh/config

# known_hosts
chmod 644 ~/.ssh/known_hosts
```

## Important Notes

- Always prefer Ed25519 keys over RSA for new setups (shorter, faster, more secure)
- Use `ssh-copy-id` instead of manually editing `authorized_keys`
- Set `IdentitiesOnly yes` in SSH config to avoid sending all keys to every server
- Use `ServerAliveInterval` to prevent idle connections from being dropped by firewalls
- For tunnels, use `-fN` flags to run in background without a shell
- The trailing `/` in rsync source path matters: `dir/` syncs contents, `dir` syncs the directory itself
- Use `rsync --dry-run` before `--delete` to verify what will be removed
- Never disable `StrictHostKeyChecking` on production or public networks
- Use `ProxyJump` (SSH 7.3+) instead of the older `ProxyCommand` for jump hosts
- SSH agent forwarding (`-A`) is convenient but carries security risks; prefer `ProxyJump`
- Keep `~/.ssh/config` permissions at 600; SSH may silently ignore it otherwise
