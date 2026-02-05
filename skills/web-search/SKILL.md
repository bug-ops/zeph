---
name: web-search
description: Search the internet for current information using CLI tools.
---
# Web Search

Use curl and jq to query DuckDuckGo:

## Quick answer
```bash
curl -s "https://api.duckduckgo.com/?q=QUERY&format=json" | jq -r '.AbstractText // .RelatedTopics[0].Text // "No results found"'
```

Replace QUERY with URL-encoded user query. Always cite the source.
