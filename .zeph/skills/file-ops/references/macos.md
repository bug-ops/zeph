# macOS File Operations (BSD Userland)

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

Regex match on full path (BSD find — use `-E`, not `--regextype`):
```bash
find -E . -regex ".*/(test|spec)_.*\.rs"
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

Spotlight search (indexed, instant results):
```bash
mdfind "pattern"                          # full-text content search
mdfind -name "config.toml"               # by filename
mdfind -onlyin /path "pattern"           # scoped to directory
mdfind "kMDItemFSSize > 1000000"         # files > 1MB
mdfind "kMDItemContentModificationDate > $time.today(-1)" -onlyin .
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

Extended regex (BSD grep does NOT support `-P` PCRE — use `-E`):
```bash
grep -rn -E "fn\s+\w+\(" . --include="*.rs"
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

ripgrep (recommended — install via Homebrew):
```bash
rg "pattern" --type rust
rg "pattern" -g "*.toml"
rg "pattern" --type rust -C 3
rg -l "pattern"
rg "TODO|FIXME|HACK" --type rust
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
tail -f /var/log/system.log
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

BSD stat (different syntax from GNU):
```bash
stat -f '%z bytes, modified %Sm' file.txt
stat -f '%p %N' file.txt                    # permissions + name
```

Extended attributes (macOS-specific):
```bash
xattr file.txt                    # list xattrs
xattr -l file.txt                 # list with values
xattr -d com.apple.quarantine file.txt  # remove quarantine flag
```

File flags (macOS-specific):
```bash
ls -lO file.txt                   # show flags (uchg, hidden, etc.)
chflags hidden file.txt           # set hidden flag
chflags nouchg file.txt           # remove immutable flag
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

APFS snapshots:
```bash
tmutil listlocalsnapshots /
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

Numeric permissions (BSD stat format):
```bash
stat -f '%A %N' file.txt
```

Recursive chmod:
```bash
find . -type f -name "*.sh" -exec chmod +x {} +
```

## Checksums

```bash
shasum -a 256 file.txt
md5 file.txt
```

## BSD sed differences

In-place edit without backup (BSD sed requires `''`):
```bash
sed -i '' 's/old/new/g' file.txt
```

## Canonical path resolution

No `readlink -f` on macOS — use `realpath`:
```bash
realpath ./relative/path
```

## Open files

```bash
open file.txt                              # default app
open -a "Visual Studio Code" file.txt      # specific app
open .                                     # Finder
```

## Recommended tools

Install via Homebrew:
```bash
brew install ripgrep fd tree bat eza
```

fd (faster find, respects .gitignore):
```bash
fd "\.rs$"
fd -e toml
fd -e rs -x wc -l
fd -H "\.env"                     # include hidden files
fd -t d tests                     # directories only
```

eza (modern ls replacement):
```bash
eza -la                           # long format with hidden
eza -la --tree --level=3          # tree view
eza -la --sort=size --reverse     # sorted by size
eza -la --git                     # show git status
```

bat (cat with syntax highlighting):
```bash
bat file.rs
bat --line-range 50:70 file.rs
```
