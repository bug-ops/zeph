---
name: text-processing
category: data
description: >
  Transform, filter, sort, and analyze text using sed, awk, sort, uniq, cut,
  tr, wc, and other text processing tools. Use when the user asks to find and
  replace text in files, extract columns from data, count lines or words, sort
  and deduplicate output, reformat delimited data, perform field-based
  calculations, or build text transformation pipelines.
license: MIT
compatibility: Requires standard text processing tools (sed, awk, sort, uniq, cut, tr, wc)
metadata:
  author: zeph
  version: "1.0"
---

# Text Processing

Before running commands, detect the OS and load the matching reference for
platform-specific syntax and gotchas:

- **Linux** — `references/linux.md` (GNU sed/awk/sort with extended flags)
- **macOS** — `references/macos.md` (BSD sed/awk/sort, Homebrew GNU alternatives)
- **Windows** — `references/windows.md` (PowerShell: Select-String, Import-Csv, ForEach-Object)

OS detection:
```bash
uname -s 2>/dev/null || echo Windows
```

**Key platform difference:** `sed -i` syntax varies — check the OS reference before in-place edits.

## Quick Reference

| Task | Portable command |
|------|-----------------|
| Find and replace (stdout) | `sed 's/old/new/g' file` |
| Extract column | `awk '{print $2}' file` |
| Cut field (delimited) | `cut -d',' -f2 file` |
| Sort lines | `sort file` |
| Unique lines | `sort file \| uniq` |
| Count lines/words/chars | `wc -lwc file` |
| Translate characters | `tr 'a-z' 'A-Z' < file` |
| First N lines | `head -n 10 file` |
| Last N lines | `tail -n 10 file` |

## sed — Stream Editor

### Substitution (portable)

```bash
# Replace first occurrence per line
sed 's/foo/bar/' file.txt

# Replace all occurrences per line
sed 's/foo/bar/g' file.txt

# Use different delimiter (when pattern contains /)
sed 's|/usr/local|/opt|g' file.txt

# Replace on specific lines
sed '5s/foo/bar/g' file.txt          # line 5 only
sed '10,20s/foo/bar/g' file.txt      # lines 10-20
sed '/pattern/s/foo/bar/g' file.txt  # lines matching pattern

# Extended regex (-E works on both GNU and BSD)
sed -E 's/([a-z]+)_([a-z]+)/\2_\1/g' file.txt

# Capture groups (BRE)
sed 's/\(.*\)=\(.*\)/\2=\1/' file.txt
```

### Deletion (portable)

```bash
sed '/pattern/d' file.txt            # delete matching lines
sed '5d' file.txt                    # delete line 5
sed '10,20d' file.txt                # delete range
sed '/^$/d' file.txt                 # delete empty lines
sed '/pattern/!d' file.txt           # delete NON-matching
```

### Print and Extract (portable)

```bash
sed -n '/pattern/p' file.txt         # print matching lines (like grep)
sed -n '10,20p' file.txt             # print line range
sed -n '/pattern/=' file.txt         # print line numbers of matches
```

### Common Transforms (portable)

```bash
sed 's/[[:space:]]*$//' file.txt     # remove trailing whitespace
sed 's/^[[:space:]]*//' file.txt     # remove leading whitespace
sed 's/\r$//' file.txt               # DOS → Unix line endings
sed -e 's/foo/bar/g' -e 's/baz/qux/g' file.txt  # multiple commands
```

## awk — Pattern Scanning and Processing

### Field Extraction

```bash
awk '{print $1}' file.txt            # first field
awk '{print $NF}' file.txt           # last field
awk -F',' '{print $2}' data.csv      # comma delimiter
awk -F'\t' '{print $1}' data.tsv     # tab delimiter
awk -F',' -v OFS='\t' '{print $1, $2}' data.csv  # change output separator
```

### Pattern Matching

```bash
awk '/error/' log.txt                # lines matching pattern
awk '!/comment/' file.txt            # lines NOT matching
awk '$3 > 100' data.txt              # numeric comparison
awk '$1 == "Alice"' data.txt         # string comparison
awk '$2 ~ /^admin/' users.txt       # regex match on field
awk '/START/,/END/' file.txt         # range (between patterns)
awk 'NR >= 10 && NR <= 20' file.txt  # line range
```

### Calculations and Aggregation

```bash
awk '{sum += $2} END {print sum}' data.txt                           # sum
awk '{sum += $2; n++} END {print sum/n}' data.txt                    # average
awk 'NR==1 || $2 > max {max=$2} END {print max}' data.txt           # max
awk '$3 > 100 {count++} END {print count}' data.txt                 # count
awk '{count[$1]++} END {for (k in count) print k, count[k]}' data.txt  # group count
awk '{sum[$1] += $2} END {for (k in sum) print k, sum[k]}' data.txt # group sum
```

### String Operations

```bash
awk '{print length($1), $1}' file.txt                # string length
awk '{print substr($0, 1, 10)}' file.txt             # substring
awk '{print toupper($1)}' file.txt                   # uppercase
awk '{print tolower($1)}' file.txt                   # lowercase
awk '{gsub(/old/, "new"); print}' file.txt           # global replace
awk '{printf "%-20s %10.2f\n", $1, $2}' data.txt    # formatted output
```

### Built-in Variables

| Variable | Meaning |
|----------|---------|
| `NR` | Current line number (across all files) |
| `NF` | Number of fields in current line |
| `FS` / `OFS` | Input / output field separator |
| `RS` / `ORS` | Input / output record separator |
| `FILENAME` | Current filename |
| `FNR` | Line number within current file |

### BEGIN and END

```bash
awk 'BEGIN {FS=","; OFS="\t"} {$1=$1; print}' data.csv   # CSV → TSV
awk 'BEGIN {print "Name\tScore"} {print $1 "\t" $2} END {print "Done"}' data.txt
```

## sort (portable flags)

```bash
sort file.txt                        # alphabetical
sort -r file.txt                     # reverse
sort -n file.txt                     # numeric
sort -k2 file.txt                    # by 2nd field
sort -k2,2 file.txt                  # by 2nd field only
sort -t',' -k3 -n data.csv          # 3rd CSV field, numeric
sort -f file.txt                     # case-insensitive
sort -u file.txt                     # unique (deduplicate)
sort -s -k2 file.txt                # stable sort
sort -k1,1 -k2,2n file.txt          # multiple keys
sort -c file.txt                     # check if sorted
```

## uniq (requires sorted input)

```bash
sort file.txt | uniq                 # deduplicate
sort file.txt | uniq -c              # count occurrences
sort file.txt | uniq -d              # show only duplicates
sort file.txt | uniq -u              # show only unique
sort file.txt | uniq -c | sort -rn | head -10   # top 10 most frequent
```

## cut

```bash
cut -d',' -f1 data.csv              # first field
cut -d',' -f1,3 data.csv            # fields 1 and 3
cut -d',' -f2-4 data.csv            # fields 2-4
cut -c1-10 file.txt                  # first 10 characters
cut -c5- file.txt                    # from 5th character onward
```

## tr — Translate Characters

```bash
tr 'a-z' 'A-Z' < file.txt           # to uppercase
tr 'A-Z' 'a-z' < file.txt           # to lowercase
tr ':' '\t' < file.txt               # replace characters
tr -d '0-9' < file.txt              # delete digits
tr -d '\r' < file.txt               # remove carriage returns
tr -s ' ' < file.txt                # squeeze repeated spaces
tr '[:lower:]' '[:upper:]' < file.txt  # POSIX character classes
```

## wc — Word Count

```bash
wc file.txt                          # lines, words, characters
wc -l file.txt                       # lines only
wc -w file.txt                       # words only
wc -m file.txt                       # characters
wc -c file.txt                       # bytes
```

## Common Pipelines

```bash
# Top 10 most common words
tr -s ' ' '\n' < file.txt | sort | uniq -c | sort -rn | head -10

# Count unique values in a column
awk -F',' '{print $2}' data.csv | sort | uniq -c | sort -rn

# Remove duplicate lines preserving order
awk '!seen[$0]++' file.txt

# Sum numbers (one per line)
paste -s -d+ numbers.txt | bc

# Extract email addresses
grep -oE '[a-zA-Z0-9._%+-]+@[a-zA-Z0-9.-]+\.[a-zA-Z]{2,}' file.txt | sort -u

# Log analysis: requests per hour
awk '{print $4}' access.log | cut -d: -f1,2 | sort | uniq -c | sort -rn
```

## Important Notes

- `sed -i` syntax differs by OS — always check the platform reference before in-place edits
- Use `-E` for extended regex in sed (portable across GNU and BSD)
- `uniq` only removes **adjacent** duplicates — always `sort` first
- `awk` field numbering starts at `$1`; `$0` is the entire line
- `cut` cannot reorder fields — use `awk` for that
- `tr` operates on characters, not strings — use `sed` for string replacement
- For large files, `awk` is generally faster than multiple piped commands
- `sort -n` treats fields as numbers; without `-n`, "9" sorts after "10" (lexicographic)
- When processing binary files, use `LC_ALL=C` to avoid locale issues
