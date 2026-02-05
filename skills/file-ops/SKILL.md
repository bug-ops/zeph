---
name: file-ops
description: File system operations - list, search, read, and analyze files.
---
# File Operations

## List files
```bash
ls -la PATH
```

## Search for files
```bash
find PATH -name "PATTERN" -type f
```

## Search content
```bash
grep -rn "PATTERN" PATH
```

## File info
```bash
wc -l FILE && file FILE
```
