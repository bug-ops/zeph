---
name: api-request
description: HTTP API requests using curl - GET, POST, PUT, DELETE with headers and JSON.
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
