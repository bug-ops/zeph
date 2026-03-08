# Linux File Operations (GNU Coreutils)

## List directory contents

Current directory:
```bash
ls -la
```

Specific path with human-readable sizes:
```bash
ls -lah /path/to/dir
```

Recursive tree view (depth limited):
```bash
find . -maxdepth 3 -print | head -100
```

## Find files by name or pattern

By exact name:
```bash
find . -name "config.toml" -type f
```

By extension:
```bash
find . -name "*.rs" -type f
```

By glob pattern with depth limit:
```bash
find . -maxdepth 3 -name "*.toml" -type f
```

Multiple extensions:
```bash
find . -type f \( -name "*.rs" -o -name "*.toml" \)
```

Regex match on full path (GNU find):
```bash
find . -regextype posix-extended -regex ".*/(test|spec)_.*\.rs"
```

Exclude directories:
```bash
find . -type f -name "*.rs" -not -path "*/target/*" -not -path "*/.git/*"
```

Files modified in the last 24 hours:
```bash
find . -type f -mtime -1 -not -path "*/target/*"
```

Files larger than 1MB:
```bash
find . -type f -size +1M -not -path "*/target/*"
```

Find directories by name:
```bash
find . -type d -name "tests"
```

Execute command on each result:
```bash
find . -name "*.log" -type f -exec rm {} +
```

Find broken symlinks:
```bash
find . -xtype l
```

Find by inode number:
```bash
find . -inum 12345
```

## Search text inside files

Basic search (recursive, with line numbers):
```bash
grep -rn "pattern" .
```

Case-insensitive:
```bash
grep -rni "pattern" .
```

With context (3 lines before and after):
```bash
grep -rn -C 3 "pattern" .
```

Extended regex:
```bash
grep -rn -E "fn\s+\w+\(" . --include="*.rs"
```

Perl-compatible regex (GNU grep only):
```bash
grep -rn -P "(?<=fn\s)\w+" . --include="*.rs"
```

Filter by file type:
```bash
grep -rn "TODO" . --include="*.rs"
grep -rn "import" . --include="*.py"
grep -rn "SELECT" . --include="*.sql"
```

Exclude directories:
```bash
grep -rn "pattern" . --exclude-dir=target --exclude-dir=.git --exclude-dir=node_modules
```

List only filenames with matches:
```bash
grep -rl "pattern" . --include="*.rs"
```

Count matches per file:
```bash
grep -rc "pattern" . --include="*.rs" | grep -v ":0$"
```

Search for exact word (not substring):
```bash
grep -rnw "Config" . --include="*.rs"
```

Multi-pattern search (OR):
```bash
grep -rn -E "error|warn|panic" . --include="*.rs"
```

Inverted match (lines NOT containing pattern):
```bash
grep -rn -v "test" src/main.rs
```

Skip binary files:
```bash
grep -rn --binary-files=without-match "pattern" .
```

ripgrep (faster, respects .gitignore):
```bash
rg "pattern" --type rust
rg "pattern" -g "*.toml"
rg "pattern" --type rust -C 3
rg -l "pattern"                    # files only
rg "pattern" --stats               # with match statistics
rg "TODO|FIXME|HACK" --type rust   # multi-pattern
```

## Read file contents

Entire file:
```bash
cat file.txt
```

First N lines:
```bash
head -n 20 file.txt
```

Last N lines:
```bash
tail -n 20 file.txt
```

Specific line range (lines 50-70):
```bash
sed -n '50,70p' file.txt
```

With line numbers:
```bash
cat -n file.txt
```

Follow log in real-time:
```bash
tail -f /var/log/syslog
tail -f file.log | grep --line-buffered "ERROR"
```

## File metadata and analysis

File type and encoding:
```bash
file path/to/file
```

Line, word, and byte count:
```bash
wc -l file.txt
```

File size (human-readable):
```bash
ls -lh file.txt
```

Detailed stat (GNU coreutils):
```bash
stat file.txt
stat --format='%s bytes, modified %y' file.txt
```

Inode info:
```bash
ls -li file.txt
```

Directory disk usage:
```bash
du -sh /path/to/dir
du -sh */ | sort -rh
```

Filesystem disk usage:
```bash
df -h
```

Top 10 largest files:
```bash
find . -type f -not -path "*/target/*" -not -path "*/.git/*" -exec ls -la {} + | sort -k5 -rn | head -10
```

Count files by extension:
```bash
find . -type f -not -path "*/target/*" -not -path "*/.git/*" | sed 's/.*\.//' | sort | uniq -c | sort -rn
```

## Compare files

Side-by-side diff:
```bash
diff file1.txt file2.txt
```

Unified diff (patch format):
```bash
diff -u file1.txt file2.txt
```

## Permissions and ownership

Numeric permissions:
```bash
stat -c '%a %n' file.txt
```

Recursive chmod:
```bash
find . -type f -name "*.sh" -exec chmod +x {} +
```

Find files by owner:
```bash
find . -user username -type f
```

Find world-writable files:
```bash
find . -perm -o+w -type f
```

ACL and extended attributes:
```bash
getfacl file.txt
```

SELinux context:
```bash
ls -Z file.txt
```

## Checksums

```bash
sha256sum file.txt
md5sum file.txt
```

Canonical path resolution:
```bash
readlink -f ./relative/path
realpath ./relative/path
```

## Inotify file watcher

```bash
inotifywait -m -r -e modify,create,delete /path/to/watch
```

## Recommended tools

Install via package manager:
```bash
# Debian/Ubuntu
sudo apt install ripgrep fd-find tree bat

# Fedora/RHEL
sudo dnf install ripgrep fd-find tree bat

# Arch
sudo pacman -S ripgrep fd tree bat
```

fd (faster find, respects .gitignore):
```bash
fd "\.rs$"                        # find by regex
fd -e toml                        # find by extension
fd -e rs -x wc -l                 # execute on each result
fd -H "\.env"                     # include hidden files
fd -t d tests                     # directories only
```

bat (cat with syntax highlighting):
```bash
bat file.rs
bat --line-range 50:70 file.rs
bat --diff file.rs
```
