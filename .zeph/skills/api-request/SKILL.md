---
name: api-request
description: Send HTTP API requests using curl. Use when the user asks to call an API, fetch a URL, send POST/PUT/DELETE requests, or work with REST endpoints and JSON payloads.
compatibility: Requires curl
---
# API Requests

## GET request
```bash
curl -s URL
```

## POST JSON
```bash
curl -s -X POST -H "Content-Type: application/json" -d '{"key":"value"}' URL
```

## With headers
```bash
curl -s -H "Authorization: Bearer TOKEN" URL
```

## Response with status code
```bash
curl -s -o /dev/null -w "%{http_code}" URL
```
