---
name: archive
category: system
description: >
  Create, extract, list, and convert compressed archives using tar, zip, gzip,
  bzip2, xz, zstd, and 7z. Use when the user asks to compress files, extract
  archives, list archive contents, split large archives, create backups,
  convert between archive formats, or work with password-protected archives.
license: MIT
compatibility: Requires tar. Optional: zip, unzip, 7z (p7zip), zstd
metadata:
  author: zeph
  version: "1.0"
---

# Archive and Compression Operations

## Quick Reference

| Action | Command |
|--------|---------|
| Create tar.gz | `tar czf archive.tar.gz dir/` |
| Extract tar.gz | `tar xzf archive.tar.gz` |
| List tar.gz | `tar tzf archive.tar.gz` |
| Create zip | `zip -r archive.zip dir/` |
| Extract zip | `unzip archive.zip` |
| List zip | `unzip -l archive.zip` |
| Compress file (gzip) | `gzip file` |
| Decompress gzip | `gunzip file.gz` |

## tar — Tape Archive

### Core Flags

| Flag | Purpose |
|------|---------|
| `c` | Create archive |
| `x` | Extract archive |
| `t` | List contents |
| `f` | Archive filename (must be last before filename) |
| `v` | Verbose (show files being processed) |
| `z` | Compress/decompress with gzip (.tar.gz, .tgz) |
| `j` | Compress/decompress with bzip2 (.tar.bz2) |
| `J` | Compress/decompress with xz (.tar.xz) |
| `--zstd` | Compress/decompress with zstd (.tar.zst) |
| `r` | Append files to archive (uncompressed tar only) |
| `u` | Update: append files newer than in archive |

### Create Archives

```bash
# Create tar.gz (most common)
tar czf archive.tar.gz dir/
tar czf archive.tar.gz file1.txt file2.txt dir/

# Create tar.bz2 (better compression, slower)
tar cjf archive.tar.bz2 dir/

# Create tar.xz (best compression, slowest)
tar cJf archive.tar.xz dir/

# Create tar.zst (fast compression, good ratio)
tar --zstd -cf archive.tar.zst dir/

# Create uncompressed tar
tar cf archive.tar dir/

# Create with verbose output
tar czvf archive.tar.gz dir/

# Exclude files/directories
tar czf archive.tar.gz --exclude='*.log' --exclude='node_modules' dir/
tar czf archive.tar.gz --exclude-vcs dir/  # exclude .git, .svn, etc.

# Exclude from file
tar czf archive.tar.gz -X exclude-list.txt dir/

# Only include specific file types
tar czf code.tar.gz dir/ --include='*.rs' --include='*.toml'

# Preserve permissions and ownership
tar czf archive.tar.gz --preserve-permissions dir/

# With timestamp in filename
tar czf "backup-$(date +%Y%m%d-%H%M%S).tar.gz" dir/

# Follow symlinks
tar czfh archive.tar.gz dir/

# Absolute paths (remove leading /)
tar czf archive.tar.gz -C / home/user/data
```

### Extract Archives

```bash
# Extract tar.gz
tar xzf archive.tar.gz

# Extract to specific directory
tar xzf archive.tar.gz -C /destination/

# Extract tar.bz2
tar xjf archive.tar.bz2

# Extract tar.xz
tar xJf archive.tar.xz

# Extract tar.zst
tar --zstd -xf archive.tar.zst

# Extract specific files
tar xzf archive.tar.gz path/to/file.txt
tar xzf archive.tar.gz --wildcards '*.rs'

# Extract with verbose output
tar xzvf archive.tar.gz

# Overwrite without prompting
tar xzf archive.tar.gz --overwrite

# Keep existing files (don't overwrite)
tar xzf archive.tar.gz --keep-old-files

# Strip leading directory components
tar xzf archive.tar.gz --strip-components=1

# Auto-detect compression (GNU tar)
tar xf archive.tar.gz   # auto-detects gz
tar xf archive.tar.bz2  # auto-detects bz2
tar xf archive.tar.xz   # auto-detects xz
```

### List Contents

```bash
# List files in tar.gz
tar tzf archive.tar.gz

# List with details (permissions, sizes, dates)
tar tzvf archive.tar.gz

# List with specific pattern
tar tzf archive.tar.gz --wildcards '*.rs'

# Count files in archive
tar tzf archive.tar.gz | wc -l
```

### Append and Update (uncompressed tar only)

```bash
# Append files to existing tar
tar rf archive.tar newfile.txt

# Update (add only newer files)
tar uf archive.tar dir/

# Concatenate tar archives
tar Af archive1.tar archive2.tar
```

## gzip / gunzip

```bash
# Compress file (replaces original)
gzip file.txt                   # creates file.txt.gz

# Compress keeping original
gzip -k file.txt

# Compress with level (1=fastest, 9=best)
gzip -9 file.txt

# Decompress
gunzip file.txt.gz
gzip -d file.txt.gz

# Decompress keeping original
gunzip -k file.txt.gz

# View compressed file without extracting
zcat file.txt.gz
zless file.txt.gz
zgrep "pattern" file.txt.gz

# Test integrity
gzip -t file.txt.gz

# Show compression info
gzip -l file.txt.gz

# Compress multiple files (each gets own .gz)
gzip file1.txt file2.txt file3.txt

# Recursive compress all files in directory
gzip -r dir/
```

## bzip2 / bunzip2

```bash
# Compress (replaces original)
bzip2 file.txt                  # creates file.txt.bz2

# Compress keeping original
bzip2 -k file.txt

# Decompress
bunzip2 file.txt.bz2
bzip2 -d file.txt.bz2

# View compressed file
bzcat file.txt.bz2

# Test integrity
bzip2 -t file.txt.bz2
```

## xz / unxz

```bash
# Compress (replaces original)
xz file.txt                    # creates file.txt.xz

# Compress keeping original
xz -k file.txt

# Compress with level (0=fastest, 9=best)
xz -9 file.txt

# Multi-threaded compression
xz -T0 file.txt                # use all CPU cores

# Decompress
unxz file.txt.xz
xz -d file.txt.xz

# View compressed file
xzcat file.txt.xz

# Test integrity
xz -t file.txt.xz

# Show file info
xz -l file.txt.xz
```

## zstd — Zstandard

```bash
# Compress (replaces original)
zstd file.txt                  # creates file.txt.zst

# Compress keeping original
zstd -k file.txt

# Compress with level (1-19, default 3; 20-22 ultra)
zstd -19 file.txt
zstd --ultra -22 file.txt

# Multi-threaded compression
zstd -T0 file.txt              # use all CPU cores

# Decompress
zstd -d file.txt.zst
unzstd file.txt.zst

# View compressed file
zstdcat file.txt.zst

# Test integrity
zstd -t file.txt.zst

# Streaming compression (pipe)
cat large.log | zstd > large.log.zst
zstd -d < large.log.zst > large.log
```

## zip / unzip

### Create Zip Archives

```bash
# Compress directory recursively
zip -r archive.zip dir/

# Compress specific files
zip archive.zip file1.txt file2.txt

# Add to existing zip
zip archive.zip newfile.txt

# Compress with level (0=store, 9=best)
zip -9 -r archive.zip dir/

# Exclude files
zip -r archive.zip dir/ -x "*.log" -x "*.tmp"
zip -r archive.zip dir/ -x "dir/cache/*"

# Password-protected zip
zip -e -r archive.zip dir/

# Split large zip (each part max 100MB)
zip -r -s 100m archive.zip dir/

# Store without compression
zip -0 -r archive.zip dir/

# Update existing zip (add changed files)
zip -u archive.zip dir/*
```

### Extract Zip Archives

```bash
# Extract to current directory
unzip archive.zip

# Extract to specific directory
unzip archive.zip -d /destination/

# Extract specific files
unzip archive.zip "path/to/file.txt"
unzip archive.zip "*.rs"

# List contents
unzip -l archive.zip

# List with detailed info
unzip -v archive.zip

# Test integrity
unzip -t archive.zip

# Overwrite without prompting
unzip -o archive.zip

# Never overwrite
unzip -n archive.zip

# Show only filenames
unzip -Z1 archive.zip

# Extract password-protected zip
unzip -P password archive.zip
```

## 7z — 7-Zip

```bash
# Create 7z archive
7z a archive.7z dir/

# Create with maximum compression
7z a -mx=9 archive.7z dir/

# Extract
7z x archive.7z

# Extract to specific directory
7z x archive.7z -o/destination/

# List contents
7z l archive.7z

# Test integrity
7z t archive.7z

# Password-protected
7z a -p archive.7z dir/       # prompts for password
7z a -pMyPassword archive.7z dir/

# Encrypt filenames too
7z a -p -mhe=on archive.7z dir/

# Create self-extracting archive
7z a -sfx archive.exe dir/

# Split archive (100MB volumes)
7z a -v100m archive.7z dir/

# Exclude files
7z a archive.7z dir/ -x!*.log -x!node_modules
```

## Archive Format Comparison

| Format | Extension | Compression | Speed | Features |
|--------|-----------|-------------|-------|----------|
| gzip | .gz | Good | Fast | Ubiquitous, single file |
| bzip2 | .bz2 | Better | Slow | Better ratio than gzip |
| xz | .xz | Best | Slowest | Best ratio, multi-thread |
| zstd | .zst | Very good | Very fast | Best speed/ratio tradeoff |
| zip | .zip | Good | Fast | Cross-platform, per-file compression |
| 7z | .7z | Excellent | Moderate | Strong encryption, best ratio |

## Common Workflows

### Archive Conversion

```bash
# tar.gz to tar.xz (recompress)
gunzip -c archive.tar.gz | xz > archive.tar.xz

# tar.gz to tar.zst
gunzip -c archive.tar.gz | zstd > archive.tar.zst

# zip to tar.gz
mkdir tmp && unzip archive.zip -d tmp/ && tar czf archive.tar.gz -C tmp . && rm -rf tmp
```

### Inspect Without Extracting

```bash
# View a single file from tar.gz
tar xzf archive.tar.gz -O path/to/file.txt

# Search within tar.gz
tar xzf archive.tar.gz -O | grep "pattern"

# View a file from zip
unzip -p archive.zip path/to/file.txt
```

### Incremental Backups

```bash
# Create snapshot file for incremental backup
tar czf backup-full.tar.gz --listed-incremental=snapshot.snar dir/

# Subsequent incremental (only changed files)
tar czf backup-inc1.tar.gz --listed-incremental=snapshot.snar dir/
```

## Important Notes

- `tar czf` — the `f` flag must come last, immediately before the filename
- gzip, bzip2, and xz replace the original file by default; use `-k` to keep it
- zstd is the modern choice: nearly as fast as gzip with compression ratios close to xz
- zip stores compression per-file; tar compresses the entire stream (generally better ratio)
- For cross-platform sharing, zip is the safest choice (native support on Windows, macOS, Linux)
- Password-protected zip uses weak encryption (ZipCrypto); use 7z with AES-256 for security
- Split archives require all parts present for extraction
- When extracting untrusted archives, use `--strip-components=1` or extract to a subdirectory to avoid overwriting files in the current directory
- GNU tar auto-detects compression format on extraction (`tar xf` works for .gz, .bz2, .xz, .zst)
