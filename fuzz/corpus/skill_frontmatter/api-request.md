---
name: api-request
category: web
description: >
  Send HTTP API requests using curl. Use when the user asks to call an API,
  fetch data from a URL, send POST/PUT/PATCH/DELETE requests, work with REST
  or GraphQL endpoints, upload files, authenticate with Bearer tokens or API
  keys, debug HTTP responses, or interact with any web service via HTTP.
license: MIT
compatibility: Requires curl (pre-installed on macOS and most Linux distributions)
metadata:
  author: zeph
  version: "1.0"
---

# HTTP API Requests with curl

## Quick Reference

| Action | Command |
|--------|---------|
| GET | `curl -s URL` |
| POST JSON | `curl -s -X POST -H "Content-Type: application/json" -d '{}' URL` |
| PUT | `curl -s -X PUT -H "Content-Type: application/json" -d '{}' URL` |
| PATCH | `curl -s -X PATCH -H "Content-Type: application/json" -d '{}' URL` |
| DELETE | `curl -s -X DELETE URL` |
| HEAD | `curl -s -I URL` |

## Essential Options

| Option | Long form | Purpose |
|--------|-----------|---------|
| `-s` | `--silent` | Suppress progress bar (always use for scripting) |
| `-S` | `--show-error` | Show errors even when silent |
| `-X` | `--request` | HTTP method (GET, POST, PUT, PATCH, DELETE, HEAD, OPTIONS) |
| `-H` | `--header` | Add request header (`-H "Name: Value"`) |
| `-d` | `--data` | Send request body (implies POST) |
| `-o` | `--output` | Write response body to file |
| `-w` | `--write-out` | Print metadata after transfer (status code, timing) |
| `-L` | `--location` | Follow HTTP redirects (3xx) |
| `-i` | `--include` | Include response headers in output |
| `-I` | `--head` | Fetch headers only (HEAD request) |
| `-k` | `--insecure` | Skip TLS certificate verification |
| `-v` | `--verbose` | Show full request/response exchange for debugging |
| `-f` | `--fail` | Return exit code 22 on HTTP errors (4xx/5xx) |
| `-F` | `--form` | Multipart form field (implies POST, multipart/form-data) |
| `-u` | `--user` | Basic auth credentials (`user:password`) |

## Timeouts

```bash
# Connection timeout (seconds to establish TCP connection)
curl -s --connect-timeout 10 URL

# Max total time (entire operation including transfer)
curl -s --max-time 30 URL

# Both together (recommended for robustness)
curl -sS --connect-timeout 10 --max-time 30 URL
```

## GET Requests

```bash
# Simple GET
curl -s "https://api.example.com/users"

# With query parameters (URL-encode special characters)
curl -s "https://api.example.com/users?status=active&limit=10"

# Follow redirects
curl -sL "https://example.com/redirect"

# Save response to file
curl -sS -o response.json "https://api.example.com/data"
```

## POST Requests

```bash
# JSON body (most common for REST APIs)
curl -s -X POST \
  -H "Content-Type: application/json" \
  -d '{"name": "Alice", "email": "alice@example.com"}' \
  "https://api.example.com/users"

# JSON body from file
curl -s -X POST \
  -H "Content-Type: application/json" \
  -d @payload.json \
  "https://api.example.com/users"

# URL-encoded form data
curl -s -X POST \
  -d "username=alice&password=secret" \
  "https://api.example.com/login"

# Empty POST (some APIs require POST with no body)
curl -s -X POST "https://api.example.com/trigger"
```

## PUT and PATCH

```bash
# PUT — full resource replacement
curl -s -X PUT \
  -H "Content-Type: application/json" \
  -d '{"name": "Alice", "email": "alice@new.com", "role": "admin"}' \
  "https://api.example.com/users/42"

# PATCH — partial update
curl -s -X PATCH \
  -H "Content-Type: application/json" \
  -d '{"email": "alice@new.com"}' \
  "https://api.example.com/users/42"
```

## DELETE

```bash
# Simple DELETE
curl -s -X DELETE "https://api.example.com/users/42"

# DELETE with body (some APIs require it)
curl -s -X DELETE \
  -H "Content-Type: application/json" \
  -d '{"reason": "duplicate"}' \
  "https://api.example.com/users/42"
```

## Authentication

```bash
# Bearer token (OAuth2, JWT)
curl -s -H "Authorization: Bearer eyJhbGciOi..." "https://api.example.com/me"

# API key in header
curl -s -H "X-API-Key: abc123" "https://api.example.com/data"

# API key as query parameter
curl -s "https://api.example.com/data?api_key=abc123"

# Basic auth (user:password)
curl -s -u "admin:secret" "https://api.example.com/admin"

# Basic auth with prompt for password (interactive)
curl -s -u "admin" "https://api.example.com/admin"
```

## File Upload

```bash
# Single file upload (multipart/form-data)
curl -s -F "file=@/path/to/document.pdf" "https://api.example.com/upload"

# File with explicit content type
curl -s -F "file=@photo.jpg;type=image/jpeg" "https://api.example.com/upload"

# File with additional form fields
curl -s \
  -F "file=@report.pdf" \
  -F "title=Q4 Report" \
  -F "category=finance" \
  "https://api.example.com/documents"

# Multiple files
curl -s \
  -F "files=@file1.txt" \
  -F "files=@file2.txt" \
  "https://api.example.com/batch-upload"

# Raw binary upload (PUT)
curl -s -X PUT \
  -H "Content-Type: application/octet-stream" \
  --data-binary @image.png \
  "https://api.example.com/files/image.png"
```

## Response Handling

```bash
# Get HTTP status code only
curl -s -o /dev/null -w "%{http_code}" "https://api.example.com/health"

# Get status code + response body
curl -s -w "\n%{http_code}" "https://api.example.com/users"

# Get response headers + body
curl -si "https://api.example.com/users"

# Detailed timing info
curl -s -o /dev/null -w "DNS: %{time_namelookup}s\nConnect: %{time_connect}s\nTLS: %{time_appconnect}s\nTotal: %{time_total}s\nStatus: %{http_code}\n" "https://api.example.com"

# Pretty-print JSON response (pipe to jq)
curl -s "https://api.example.com/users" | jq .

# Extract specific field from JSON
curl -s "https://api.example.com/users/1" | jq -r '.email'

# Response headers only
curl -sI "https://api.example.com/users"
```

## Common Patterns

### Pagination
```bash
# Offset-based
curl -s "https://api.example.com/items?offset=0&limit=50"
curl -s "https://api.example.com/items?offset=50&limit=50"

# Page-based
curl -s "https://api.example.com/items?page=1&per_page=25"

# Cursor-based (use cursor from previous response)
curl -s "https://api.example.com/items?cursor=eyJpZCI6MTAwfQ"
```

### Retry on Failure
```bash
# Retry up to 3 times with exponential backoff
curl -s --retry 3 --retry-delay 2 --retry-max-time 30 "https://api.example.com/data"

# Retry only on specific errors (transient)
curl -s --retry 3 --retry-all-errors "https://api.example.com/data"
```

### Conditional Requests
```bash
# If-Modified-Since (returns 304 if unchanged)
curl -s -H "If-Modified-Since: Mon, 01 Jan 2024 00:00:00 GMT" "https://api.example.com/data"

# ETag-based caching
curl -s -H 'If-None-Match: "abc123"' "https://api.example.com/data"
```

### GraphQL
```bash
curl -s -X POST \
  -H "Content-Type: application/json" \
  -d '{"query": "{ users(first: 10) { id name email } }"}' \
  "https://api.example.com/graphql"
```

## Debugging

```bash
# Verbose output (shows full request/response headers and TLS handshake)
curl -v "https://api.example.com/users" 2>&1

# Trace to file (hex dump of all data)
curl --trace trace.log "https://api.example.com/users"

# Show only response headers
curl -sI "https://api.example.com/users"

# Dump request and response headers to file
curl -sS -D headers.txt -o body.json "https://api.example.com/users"
```

## HTTP Status Code Reference

| Range | Meaning | Common Codes |
|-------|---------|-------------|
| 2xx | Success | 200 OK, 201 Created, 204 No Content |
| 3xx | Redirect | 301 Moved, 302 Found, 304 Not Modified |
| 4xx | Client error | 400 Bad Request, 401 Unauthorized, 403 Forbidden, 404 Not Found, 409 Conflict, 422 Unprocessable, 429 Too Many Requests |
| 5xx | Server error | 500 Internal Error, 502 Bad Gateway, 503 Unavailable, 504 Timeout |

## Important Notes

- Always use `-s` (silent) to suppress the progress bar in scripts
- Use `-sS` to suppress progress but still show errors
- When sending JSON, always set `-H "Content-Type: application/json"`
- Use `-L` to follow redirects (curl does not follow them by default)
- Use `--connect-timeout` and `--max-time` for robustness against hanging requests
- Pipe to `jq .` for readable JSON output, or `jq -r '.field'` to extract values
- Use `-f` (fail) when you need curl to return a non-zero exit code on HTTP errors
- For binary data in POST body, use `--data-binary` instead of `-d` (which strips newlines)
- Special characters in URLs should be percent-encoded (spaces = `%20`, `&` = `%26`)
- Use single quotes around JSON payloads to avoid shell variable expansion issues
