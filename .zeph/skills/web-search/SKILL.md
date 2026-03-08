---
name: web-search
description: Search the internet for current information. Use when the user asks to find, look up, or search for something online, or needs up-to-date information not available locally.
compatibility: Requires curl and jq
---
# Web Search

Search the internet using DuckDuckGo API via curl.

## Quick answer
```bash
curl -s "https://api.duckduckgo.com/?q=QUERY&format=json" | jq -r '.AbstractText // .RelatedTopics[0].Text // "No results found"'
```

## Detailed results
```bash
curl -s "https://api.duckduckgo.com/?q=QUERY&format=json" | jq '{abstract: .AbstractText, source: .AbstractSource, url: .AbstractURL, related: [.RelatedTopics[:5][] | {text: .Text, url: .FirstURL}]}'
```

## Instructions

1. Replace QUERY with the URL-encoded user query (spaces become `+`)
2. Execute the curl command in a bash block
3. Read the output and summarize the results for the user
4. Always cite the source URL from the response
5. If the first query returns no results, try rephrasing with different keywords
