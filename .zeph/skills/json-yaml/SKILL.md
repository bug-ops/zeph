---
name: json-yaml
category: data
description: >
  Parse, query, transform, validate, and convert JSON and YAML data using jq
  and fy (fast-yaml). Use when the user asks to filter JSON arrays, extract
  nested fields, reshape data structures, convert between JSON and YAML formats,
  validate or lint YAML files, pretty-print or minify JSON, format YAML, or
  perform complex data transformations on structured data.
license: MIT
compatibility: Requires jq. Requires fy (fast-yaml) for YAML processing (cargo install fast-yaml-cli)
metadata:
  author: zeph
  version: "1.0"
---

# JSON & YAML Processing with jq and fy

## Quick Reference

| Action | Command |
|--------|---------|
| Pretty-print JSON | `jq . file.json` |
| Minify JSON | `jq -c . file.json` |
| Extract field | `jq '.name' file.json` |
| Extract raw string | `jq -r '.name' file.json` |
| Validate YAML | `fy parse file.yaml` |
| Format YAML | `fy format -i file.yaml` |
| Lint YAML | `fy lint file.yaml` |
| YAML to JSON | `fy convert json file.yaml` |
| JSON to YAML | `fy convert yaml file.json` |
| Validate JSON | `jq empty file.json` |

## jq Basics

### Identity and Field Access

```bash
# Identity (pass-through, pretty-print)
cat data.json | jq .

# Access nested field
jq '.user.address.city' data.json

# Access array element
jq '.[0]' data.json
jq '.items[2]' data.json

# Access multiple fields (comma operator)
jq '.name, .age' data.json

# Optional field access (no error if missing)
jq '.missing?' data.json
```

### String Interpolation

```bash
# Build strings from fields
jq -r '"Name: \(.name), Age: \(.age)"' data.json

# Use in filters
jq -r '.[] | "User \(.id): \(.email)"' users.json
```

### Raw Output

```bash
# -r removes quotes from string output
jq -r '.name' data.json

# -R treats input as raw strings (not JSON)
echo "hello" | jq -R .

# --raw-output0 uses NUL separator (for xargs -0)
jq -r0 '.[]' list.json | xargs -0 -I{} echo "{}"
```

## jq Filters

### select — Filter by Condition

```bash
# Select objects matching condition
jq '.[] | select(.age > 30)' users.json

# Select by string match
jq '.[] | select(.name == "Alice")' users.json

# Select with contains
jq '.[] | select(.tags | contains(["admin"]))' users.json

# Select with test (regex)
jq '.[] | select(.email | test("@gmail\\.com$"))' users.json

# Select non-null values
jq '.[] | select(.address != null)' users.json

# Multiple conditions (and / or)
jq '.[] | select(.age > 25 and .status == "active")' users.json
jq '.[] | select(.role == "admin" or .role == "superadmin")' users.json
```

### map — Transform Each Element

```bash
# Extract field from each element
jq '[.[] | .name]' users.json
jq 'map(.name)' users.json          # equivalent shorthand

# Transform each element
jq 'map({name: .name, upper: (.name | ascii_upcase)})' users.json

# Map with select (filter then transform)
jq 'map(select(.active) | .email)' users.json

# map_values — transform values of an object
jq 'map_values(. + 1)' '{"a": 1, "b": 2}'
```

### reduce — Accumulate a Result

```bash
# Sum all values
jq 'reduce .[] as $x (0; . + $x)' numbers.json

# Build an object from array
jq 'reduce .[] as $item ({}; . + {($item.id | tostring): $item.name})' items.json

# Concatenate strings
jq 'reduce .[] as $s (""; . + $s + " ")' strings.json

# Count by category
jq 'reduce .[] as $item ({}; .[$item.category] += 1)' items.json
```

### group_by — Group Elements

```bash
# Group by field value
jq 'group_by(.department)' employees.json

# Group and count
jq 'group_by(.status) | map({status: .[0].status, count: length})' items.json

# Group and collect names
jq 'group_by(.team) | map({team: .[0].team, members: map(.name)})' people.json
```

### sort_by — Sort Elements

```bash
# Sort by field
jq 'sort_by(.name)' users.json

# Sort descending (reverse)
jq 'sort_by(.age) | reverse' users.json

# Sort by multiple fields
jq 'sort_by(.department, .name)' employees.json

# Sort by numeric value
jq 'sort_by(-.score)' scores.json

# unique_by — deduplicate
jq 'unique_by(.email)' users.json
```

## jq Object Construction

```bash
# Build new objects
jq '.[] | {name, email}' users.json

# Rename fields
jq '.[] | {user_name: .name, user_email: .email}' users.json

# Add computed fields
jq '.[] | . + {full_name: "\(.first) \(.last)"}' users.json

# Merge objects
jq '. as $a | input as $b | $a * $b' file1.json file2.json

# Delete fields
jq 'del(.password, .secret)' user.json

# Convert object to key-value pairs
jq 'to_entries' object.json
jq 'to_entries | map({key, value})' object.json

# Convert key-value pairs back to object
jq 'from_entries' entries.json
```

## jq Array Operations

```bash
# Length
jq 'length' array.json
jq '.items | length' data.json

# Flatten nested arrays
jq 'flatten' nested.json
jq 'flatten(1)' nested.json      # one level only

# Add (sum numbers, concat strings/arrays)
jq 'add' numbers.json

# Min / max
jq 'min' numbers.json
jq 'max_by(.score)' items.json
jq 'min_by(.price)' products.json

# First / last / nth
jq 'first' array.json
jq 'last' array.json
jq 'nth(3)' array.json

# Limit / until
jq 'limit(5; .[])' array.json

# indices / index
jq 'index("needle")' array.json

# Cartesian product (combinations)
jq '[.a, .b] | combinations' data.json
```

## jq Conditionals

```bash
# If-then-else
jq '.[] | if .age >= 18 then "adult" else "minor" end' people.json

# Alternative operator (default value)
jq '.name // "unknown"' data.json

# Try-catch
jq 'try .foo.bar catch "not found"' data.json
```

## jq Type Functions

```bash
# Type checking
jq '.[] | type' data.json
jq '.[] | select(type == "string")' data.json

# Type conversion
jq '.count | tonumber' data.json
jq '.id | tostring' data.json

# Numeric operations
jq '.price | floor' item.json
jq '.price | ceil' item.json
jq '.price | round' item.json
jq '.values | add / length' data.json  # average
```

## jq String Functions

```bash
# Length, case conversion
jq '.name | length' data.json
jq '.name | ascii_upcase' data.json
jq '.name | ascii_downcase' data.json

# Split and join
jq '.path | split("/")' data.json
jq '.tags | join(", ")' data.json

# Test (regex match, returns bool)
jq '.email | test("^[a-z]+@")' data.json

# Match (regex capture groups)
jq '.line | match("(\\w+)=(\\w+)")' data.json

# Capture (named groups)
jq '.line | capture("(?<key>\\w+)=(?<val>\\w+)")' data.json

# Replace (gsub / sub)
jq '.text | gsub("foo"; "bar")' data.json
jq '.text | sub("^prefix_"; "")' data.json

# Starts/ends with
jq '.name | startswith("Dr.")' data.json
jq '.file | endswith(".json")' data.json

# ltrimstr / rtrimstr
jq '.path | ltrimstr("/api/v1")' data.json
```

## jq with Pipes and Shell

```bash
# Pipe curl output to jq
curl -s "https://api.example.com/users" | jq '.data[].name'

# Use jq output in shell variables
name=$(jq -r '.name' config.json)

# Process line by line
jq -r '.[] | .id' items.json | while read -r id; do echo "Processing $id"; done

# Read from stdin
echo '{"a":1}' | jq '.a'

# Multiple files (slurp mode)
jq -s '.[0] * .[1]' defaults.json overrides.json

# Create JSON from shell variables
jq -n --arg name "$NAME" --arg email "$EMAIL" '{name: $name, email: $email}'

# Pass arguments
jq --arg threshold "$THRESH" '.[] | select(.score > ($threshold | tonumber))' data.json
```

## fy (fast-yaml) — YAML Processing

All YAML operations use `fy` (fast-yaml CLI). Install: `cargo install fast-yaml-cli`.

### Validate YAML

```bash
# Validate syntax (reports errors with rustc-style diagnostics)
fy parse config.yaml

# Validate multiple files
fy parse config.yaml values.yaml

# Validate a directory (recursive, respects .gitignore)
fy parse src/
```

### Format YAML

```bash
# Format and print to stdout
fy format config.yaml

# Format in-place
fy format -i config.yaml

# Format entire directory in-place
fy format -i src/

# Format with glob pattern
fy format -i "**/*.yaml"

# Parallel formatting (8 workers)
fy format -i -j 8 project/
```

### Lint YAML

```bash
# Lint a file (validation + style rules)
fy lint config.yaml

# Lint with exclusions
fy lint --exclude "tests/**" .

# Lint a directory
fy lint src/
```

### Convert Between Formats

```bash
# YAML to JSON
fy convert json config.yaml

# JSON to YAML
fy convert yaml data.json

# Convert and redirect
fy convert json config.yaml > config.json
fy convert yaml data.json > data.yaml
```

### Batch Operations

fy activates batch mode automatically for directories, glob patterns, or multiple files:
- Respects `.gitignore` exclusions
- Supports include/exclude patterns
- Parallel worker configuration via `-j` flag
- 6-15x speedup on multi-file operations

```bash
# Format all YAML in a project with 4 workers
fy format -i -j 4 .

# Lint all YAML except test fixtures
fy lint --exclude "tests/fixtures/**" .

# Validate all YAML in multiple directories
fy parse src/ config/ deploy/
```

### Workflow: Edit and Validate

After editing any YAML file, always validate with fy:

```bash
# 1. Edit the file (manually or with sed/awk)
# 2. Validate
fy parse config.yaml

# 3. Format (normalize style)
fy format -i config.yaml
```

## Important Notes

- `jq` uses single quotes for the filter expression to avoid shell expansion
- Use `-r` for raw output (no quotes around strings) when piping to other commands
- Use `-e` to set exit code based on output (null/false = exit 1)
- Use `-c` for compact (single-line) output, useful for line-by-line processing
- Use `--slurpfile` to load a JSON file into a variable: `jq --slurpfile cfg config.json '...'`
- When constructing JSON with `jq -n`, use `--arg` for strings and `--argjson` for numbers/booleans
- Always use `fy` for YAML validation, formatting, and conversion — never edit YAML manually without validating with `fy` afterward
- `fy` supports YAML 1.2.2 specification
