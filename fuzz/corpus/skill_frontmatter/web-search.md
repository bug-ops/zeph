---
name: web-search
category: web
description: >
  Search the internet for current information using the DuckDuckGo Instant
  Answer API. Use when the user asks to find, look up, or search for something
  online, needs up-to-date information, wants facts about a topic, definitions,
  or general knowledge that is not available locally. Keywords: search, find,
  look up, google, what is, who is, define, current, latest, news.
license: MIT
compatibility: Requires curl and jq
metadata:
  author: zeph
  version: "1.0"
---

# Web Search via DuckDuckGo Instant Answer API

## Endpoint

```
GET https://api.duckduckgo.com/?q=QUERY&format=json&no_html=1&skip_disambig=1
```

## Step-by-Step Instructions

1. Take the user's query and convert it into effective search terms
2. URL-encode the query (spaces become `+`, special characters percent-encoded)
3. Execute the curl command below
4. Parse the JSON response to find the best answer
5. Summarize the results for the user in natural language
6. Always cite the source URL from the response

## Quick Search

```bash
curl -s "https://api.duckduckgo.com/?q=QUERY&format=json&no_html=1&skip_disambig=1" | jq '{
  type: .Type,
  abstract: .AbstractText,
  abstract_source: .AbstractSource,
  abstract_url: .AbstractURL,
  answer: .Answer,
  definition: .Definition,
  definition_source: .DefinitionSource,
  redirect: .Redirect,
  related: [.RelatedTopics[:5][] | select(.Text) | {text: .Text, url: .FirstURL}]
}'
```

## Compact Search (Best Single Answer)

```bash
curl -s "https://api.duckduckgo.com/?q=QUERY&format=json&no_html=1&skip_disambig=1" | jq -r '
  if .Redirect != "" then "Redirect: " + .Redirect
  elif .Answer != "" then .Answer
  elif .AbstractText != "" then .AbstractText + "\nSource: " + .AbstractURL
  elif .Definition != "" then .Definition + "\nSource: " + .DefinitionURL
  elif (.RelatedTopics | length) > 0 then .RelatedTopics[0].Text + "\nURL: " + .RelatedTopics[0].FirstURL
  else "No results found. Try different keywords."
  end'
```

## Detailed Results (All Fields)

```bash
curl -s "https://api.duckduckgo.com/?q=QUERY&format=json&no_html=1&skip_disambig=1" | jq '{
  type: .Type,
  heading: .Heading,
  abstract: .AbstractText,
  abstract_source: .AbstractSource,
  abstract_url: .AbstractURL,
  answer: .Answer,
  answer_type: .AnswerType,
  definition: .Definition,
  definition_source: .DefinitionSource,
  definition_url: .DefinitionURL,
  image: .Image,
  redirect: .Redirect,
  related: [.RelatedTopics[]? | select(.Text) | {text: .Text, url: .FirstURL, icon: .Icon.URL}],
  results: [.Results[]? | {text: .Text, url: .FirstURL}],
  infobox: .Infobox
}'
```

## Query Parameters

| Parameter | Value | Purpose |
|-----------|-------|---------|
| `q` | search terms | The search query (required) |
| `format` | `json` | Response format (use `json`, also supports `xml`) |
| `no_html` | `1` | Strip HTML from response text (recommended) |
| `skip_disambig` | `1` | Skip disambiguation pages, return first result directly |
| `no_redirect` | `1` | Do not follow `!bang` redirects |
| `pretty` | `1` | Pretty-print JSON (for debugging only) |
| `t` | app name | Application name identifier (optional, for attribution) |

## Response Fields

### Primary Answer Fields

| Field | Description |
|-------|-------------|
| `Type` | Response category: `A` (article), `D` (disambiguation), `C` (category), `N` (name), `E` (exclusive), or empty |
| `AbstractText` | Summary text for the topic (no HTML when `no_html=1`) |
| `AbstractSource` | Name of the source (e.g., "Wikipedia") |
| `AbstractURL` | Deep link to the full article at the source |
| `Heading` | Title/heading of the result |
| `Image` | URL of a relevant image |
| `Answer` | Instant answer text (for calculations, conversions, IP lookups, etc.) |
| `AnswerType` | Type of instant answer: `calc`, `color`, `digest`, `info`, `ip`, `iploc`, `phone`, `pw`, `rand`, `regexp`, `unicode`, `upc`, `zip` |

### Definition Fields

| Field | Description |
|-------|-------------|
| `Definition` | Dictionary definition text |
| `DefinitionSource` | Source of the definition |
| `DefinitionURL` | URL to the full definition |

### Related and Redirect

| Field | Description |
|-------|-------------|
| `RelatedTopics` | Array of related topic objects (see below) |
| `Results` | Array of external result links |
| `Redirect` | URL to redirect to (for `!bang` commands) |
| `Infobox` | Structured data box (key-value facts) |

### RelatedTopics Object

Each item in `RelatedTopics` contains:

| Field | Description |
|-------|-------------|
| `Text` | Description text |
| `FirstURL` | Link to the DuckDuckGo page for this topic |
| `Icon.URL` | Icon/thumbnail URL |
| `Icon.Width` | Icon width |
| `Icon.Height` | Icon height |
| `Result` | HTML link string |

When `Type` is `D` (disambiguation), `RelatedTopics` may contain grouped items with a `Name` key and nested `Topics` array.

## Search Strategies

### Strategy 1: Direct Query
Use the user's question directly as the query.
```bash
# "What is Rust programming language?"
curl -s "https://api.duckduckgo.com/?q=Rust+programming+language&format=json&no_html=1&skip_disambig=1"
```

### Strategy 2: Keyword Extraction
Extract key nouns and concepts, drop filler words.
```bash
# "Can you tell me about the history of the Internet?" -> "history Internet"
curl -s "https://api.duckduckgo.com/?q=history+of+the+Internet&format=json&no_html=1&skip_disambig=1"
```

### Strategy 3: Specific Entity Lookup
For people, places, or things, use the proper name.
```bash
# "Who created Linux?"
curl -s "https://api.duckduckgo.com/?q=Linus+Torvalds&format=json&no_html=1&skip_disambig=1"
```

### Strategy 4: Fallback Rephrasing
If the first query returns empty fields, try rephrasing:
- Add context words: "Python" -> "Python programming language"
- Use the full proper name: "JS" -> "JavaScript"
- Try a definition query: "define quantum computing"
- Remove overly specific terms and broaden the search

## Parsing the Response

### Priority Order for Extracting the Best Answer

1. **Redirect** — if non-empty, the query is a `!bang` command; follow the URL
2. **Answer** — instant answers (calculations, IP lookups, conversions) are the most direct
3. **AbstractText** — topic summaries from sources like Wikipedia
4. **Definition** — dictionary definitions
5. **RelatedTopics[0].Text** — first related topic as a fallback
6. If all are empty, rephrase and retry with different keywords

### Extracting Structured Data from Infobox

```bash
curl -s "https://api.duckduckgo.com/?q=Albert+Einstein&format=json&no_html=1" | jq '.Infobox.content[]? | {label: .label, value: .value}'
```

## URL Encoding Rules

| Character | Encoded |
|-----------|---------|
| Space | `+` or `%20` |
| `&` | `%26` |
| `=` | `%3D` |
| `?` | `%3F` |
| `/` | `%2F` |
| `#` | `%23` |
| `+` | `%2B` |
| `"` | `%22` |

In bash, use `jq -rn --arg q "search terms" '$q | @uri'` for proper encoding.

## Important Notes

- The DuckDuckGo API returns **instant answers**, not web search results. It excels at factual lookups, definitions, and entity information, but may return empty for very recent events or niche topics
- Always use `format=json` for machine-readable output
- Always use `no_html=1` to get clean text without HTML tags
- Use `skip_disambig=1` to get direct answers instead of disambiguation pages
- Always cite the source URL (`AbstractURL`, `DefinitionURL`, or `FirstURL`) when presenting results
- The API is free and does not require authentication
- Rate limiting: be respectful; do not send rapid-fire requests. A brief pause between retries is courteous
- If the API returns no useful data after 2 attempts with different phrasings, inform the user that the information could not be found via this source
- The `Redirect` field handles DuckDuckGo `!bang` shortcuts (e.g., `!w` for Wikipedia, `!so` for StackOverflow)
